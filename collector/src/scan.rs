//! 只读扫描本机 Claude Code 各档位目录，逐条复刻 server.py 的 scan() 语义。
//!
//! 输出 JSON 的字段名、字段顺序与取值规则与 Python 版一致；
//! 区别仅在容错：单个文件/目录读取失败只跳过该项，不会让整轮扫描失败。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::timefmt::iso_ms;

type Obj = Map<String, Value>;

/// 扫描中因 stat/open/read/解析失败而跳过的文件/目录数（不逐条打日志，由心跳汇总后清零）
pub static SKIPPED: AtomicU64 = AtomicU64::new(0);

fn skip() {
    SKIPPED.fetch_add(1, Ordering::Relaxed);
}

pub const LIVE_SECS: f64 = 90.0; // 记录文件在这段时间内有写入，视为仍在工作
const HEAD_BYTES: u64 = 96 * 1024;
const TAIL_BYTES: u64 = 256 * 1024;
const SMALL_JSON_MAX: u64 = 64 * 1024;
/// 尾窗里找不到 reply 时，从尾窗起点往前分块回看
const BACK_CHUNK: u64 = 256 * 1024;
const BACK_MAX: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------- 名字池

const NAMES_FALLBACK: &str = "Ada Alma Amber Arlo Basil Bea Bram Bruno Cass Cleo Clyde Cora Dax Dora Eli Elsa Emmet Esme \
Ezra Faye Finn Flora Freya Gus Greta Hazel Hugo Ida Ines Ivan Ivy Jade Juno Kai Kit Lars Leo \
Levi Lina Lou Mabel Mae Milo Mira Nadia Nell Nico Noor Olive Omar Otto Pearl Pip Quinn Rex \
Rhea Rory Ruby Sage Sol Suki Tess Theo Toby Uma Vera Wren Yara Zane Zora";
const LEAD_FALLBACK: &str = "Atlas Beacon Comet Delta Echo Falcon Gemini Halo Ion Jet Kestrel Lumen Nova Orion Pulsar Quasar Rook \
Sirius Tango Vega Aurora Boreal Cobalt Dune Ember Fjord Granite Harbor Indigo Juniper Kite Lynx \
Meteor Nimbus Onyx Pike Raven Summit Tundra Umber Vortex Willow Zenith";
/// 编译期内嵌角色表（名字与头像一一对应）
const ROSTER: &str = include_str!("../../avatars/roster.json");

/// 取角色表中某组的名字；任一项缺 name 视为整组失败（同 Python 的异常分支）。
fn roster_names(r: &Value, key: &str) -> Option<Vec<String>> {
    let arr = r.get(key)?.as_array()?;
    arr.iter().map(|p| p.get("name").and_then(Value::as_str).map(str::to_owned)).collect()
}

/// 返回 (NAMES, LEAD_NAMES)。顺序同 Python：先 crew 再 captains，前者失败则后者不再读取。
pub fn name_pools() -> (Vec<String>, Vec<String>) {
    let split = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let mut names = split(NAMES_FALLBACK);
    let mut leads = split(LEAD_FALLBACK);
    if let Ok(r) = serde_json::from_str::<Value>(ROSTER) {
        if let Some(c) = roster_names(&r, "crew") {
            if !c.is_empty() {
                names = c;
            }
            if let Some(l) = roster_names(&r, "captains") {
                if !l.is_empty() {
                    leads = l;
                }
            }
        }
    }
    (names, leads)
}

/// 同 Python pick()：sha1 十六进制前 8 位转整数取模。
pub fn pick<'a>(seq: &'a [String], key: &str) -> &'a str {
    if seq.is_empty() {
        return "?";
    }
    let d = sha1_smol::Sha1::from(key.as_bytes()).digest().bytes();
    let h = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
    &seq[h % seq.len()]
}

// ---------------------------------------------------------------- 通用小工具

/// Python 真值判断
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `o.get(k)` 且为真
fn tget<'a>(o: &'a Obj, k: &str) -> Option<&'a Value> {
    o.get(k).filter(|v| truthy(v))
}

/// `o.get(k) == s`
fn is_str(o: &Obj, k: &str, s: &str) -> bool {
    o.get(k).and_then(Value::as_str) == Some(s)
}

/// Python `a or b` 链：返回第一个为真的值
fn or_chain(vals: &[Option<&Value>]) -> Option<Value> {
    vals.iter().flatten().find(|v| truthy(v)).map(|v| (*v).clone())
}

fn now_f() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// 同 Python st_mtime：sec + nsec * 1e-9（双精度）
fn mtime_f(m: &fs::Metadata) -> f64 {
    m.mtime() as f64 + m.mtime_nsec() as f64 * 1e-9
}

/// int(st_mtime * 1000)
fn mtime_ms(m: &fs::Metadata) -> i64 {
    (mtime_f(m) * 1000.0) as i64
}

fn is_dir(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

/// 列目录（readdir 顺序，同 Python iterdir/glob），读失败返回空；非 UTF-8 文件名跳过。
fn list_dir(p: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(p) else {
        skip();
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some((name, e.path()))
        })
        .collect()
}

/// Python Path.stem：去掉最后一个后缀（名字以点开头且无其他点时不算后缀）
fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[..i],
        _ => name,
    }
}

/// Python PurePosixPath(p).name
fn path_name(p: &str) -> String {
    p.split('/').rfind(|c| !c.is_empty() && *c != ".").unwrap_or("").to_owned()
}

/// 项目名：cwd 最后一段目录；工作树（`<仓库>/.claude/worktrees/<名字>[/...]`）归到所属仓库名，与 Claude App 侧边栏一致
fn project_of(cwd: &str) -> String {
    match cwd.find("/.claude/worktrees/") {
        Some(i) if !path_name(&cwd[..i]).is_empty() => path_name(&cwd[..i]),
        _ => path_name(cwd),
    }
}

/// os.path.expanduser：只展开 "~" 与 "~/..."
pub fn expand_user(dir: &str) -> PathBuf {
    if dir == "~" || dir.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            if dir.len() > 2 {
                p.push(&dir[2..]);
            }
            return p;
        }
    }
    PathBuf::from(dir)
}

// ---------------------------------------------------------------- 数值：int 或 float（Python max(int, float) 的结果）

#[derive(Clone, Copy, Debug)]
pub enum Num {
    I(i64),
    F(f64),
}

impl Num {
    pub fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }
    /// JSON 数字 → Num；非数字视为 0（updatedAt 非数字时当 0）
    fn from_value(v: Option<&Value>) -> Num {
        match v {
            Some(Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    Num::I(i)
                } else {
                    Num::F(n.as_f64().unwrap_or(0.0))
                }
            }
            _ => Num::I(0),
        }
    }
    /// Python max(a, b)：b 严格大于 a 才取 b
    fn max(a: Num, b: Num) -> Num {
        if b.f() > a.f() {
            b
        } else {
            a
        }
    }
}

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            Num::I(i) => s.serialize_i64(i),
            Num::F(f) => s.serialize_f64(f),
        }
    }
}

// ---------------------------------------------------------------- jsonl 读取与摘要

/// 读文件头尾若干行 JSON（大文件不全读）。只接受 object 行，坏行/半行跳过。
/// 返回 (head 行, tail 行, tail 第一行在文件中的偏移；tail 覆盖整个文件时为 0)。
fn read_head_tail(path: &Path, size: u64) -> io::Result<(Vec<Obj>, Vec<Obj>, u64)> {
    let mut f = File::open(path)?;
    let mut head = Vec::new();
    (&mut f).take(HEAD_BYTES).read_to_end(&mut head)?;
    let tail;
    let mut tail_start = 0;
    if size > HEAD_BYTES + TAIL_BYTES {
        f.seek(SeekFrom::Start(size - TAIL_BYTES))?;
        let mut t = Vec::new();
        // 上限防止文件在 stat 之后暴涨导致读入过多
        (&mut f).take(HEAD_BYTES + TAIL_BYTES).read_to_end(&mut t)?;
        let cut = t.iter().position(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        tail = t.split_off(cut);
        tail_start = size - TAIL_BYTES + cut as u64;
    } else if size > HEAD_BYTES {
        let mut t = head;
        (&mut f).take(HEAD_BYTES + TAIL_BYTES).read_to_end(&mut t)?;
        head = t.clone();
        tail = t;
    } else {
        tail = head.clone();
    }
    Ok((parse_lines(&head, size > HEAD_BYTES), parse_lines(&tail, false), tail_start))
}

/// 从尾窗起点 start（= size - TAIL_BYTES，可能落在行中间）往前按 BACK_CHUNK 分块反向读取，
/// 逐块解析完整行找 reply，累计回看不超过 BACK_MAX（同 Python reply_with_lookback 的范围）。
/// 跨 start 的那一行与回看下界处的半行都不解析；块起点处的半行留给下一块拼接；任何读错误都只返回 null。
fn reply_backward(path: &Path, start: u64) -> Value {
    if start == 0 {
        return Value::Null;
    }
    let Ok(mut f) = File::open(path) else { return Value::Null };
    let floor = start.saturating_sub(BACK_MAX);
    let mut end = start; // 尚未解析区域的右边界：首块可能落在行中间，之后始终是行首
    let mut lo = end;
    while lo > floor {
        lo = lo.saturating_sub(BACK_CHUNK).max(floor);
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(lo)).is_err() || (&mut f).take(end - lo).read_to_end(&mut buf).is_err() {
            return Value::Null;
        }
        // lo 之前若还有内容，lo 处可能是半行：从第一个换行之后开始解析，半行与更前一块拼接。
        // 非首块时 buf 末字节是 end 前一行的换行符，不算作分界（否则会原地打转）。
        let off = if lo == 0 {
            0
        } else {
            let body = &buf[..buf.len().saturating_sub(1)];
            match body.iter().position(|&b| b == b'\n') {
                Some(i) => i + 1,
                None => continue, // 整块都在同一行里：保持 end，继续向前扩
            }
        };
        // 首块末尾若是跨 start 的半行则丢弃；之后的块都以换行结尾，不受影响
        let r = last_reply(&parse_lines(&buf[off..], true));
        if !r.is_null() {
            return r;
        }
        end = lo + off as u64;
        lo = end;
    }
    Value::Null
}

/// 尾窗没有 reply 时的回退（尾窗已覆盖整个文件时不回退）：
/// 追加写（新 size >= 旧 size）沿用上次的 reply，否则从尾窗起点向前回看。
fn reply_with_fallback(path: &Path, tail: &[Obj], size: u64, prev: Option<(u64, &Value)>) -> Value {
    let r = last_reply(tail);
    if !r.is_null() || size <= HEAD_BYTES + TAIL_BYTES {
        return r;
    }
    match prev {
        Some((old_size, old_reply)) if size >= old_size => old_reply.clone(),
        _ => reply_backward(path, size - TAIL_BYTES),
    }
}

fn parse_lines(buf: &[u8], drop_last_partial: bool) -> Vec<Obj> {
    let mut lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
    if drop_last_partial && !buf.ends_with(b"\n") {
        lines.pop();
    }
    let mut out = Vec::new();
    for ln in lines {
        // bytes.strip() 的空白集合
        if ln.iter().all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
            continue;
        }
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(ln) {
            out.push(m);
        }
    }
    out
}

/// 小 JSON 文件（registry / meta）：必须是普通文件，最多读 64KiB
fn read_small(p: &Path) -> Option<String> {
    let m = fs::metadata(p).ok()?;
    if !m.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    File::open(p).ok()?.take(SMALL_JSON_MAX).read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// assistant 消息里的 message 对象（不是对象则视为空）
fn message_of(o: &Obj) -> Option<&Obj> {
    tget(o, "message").and_then(Value::as_object)
}

fn models_in(lines: &[Obj]) -> Vec<Value> {
    let mut out = Vec::new();
    for o in lines {
        if is_str(o, "type", "assistant") {
            if let Some(m) = message_of(o).and_then(|m| tget(m, "model")) {
                if m.as_str() != Some("<synthetic>") {
                    out.push(m.clone());
                }
            }
        }
    }
    out
}

/// 最后一条 assistant 的结束方式；子 agent 以 SubagentHandback/StructuredOutput 工具交回结果视为正常完成。
fn last_assistant_stop(lines: &[Obj]) -> Value {
    for o in lines.iter().rev() {
        if is_str(o, "type", "assistant") {
            let Some(m) = message_of(o) else { return Value::Null };
            if let Some(Value::Array(content)) = tget(m, "content") {
                for c in content {
                    if let Value::Object(c) = c {
                        if is_str(c, "type", "tool_use")
                            && matches!(c.get("name").and_then(Value::as_str), Some("SubagentHandback" | "StructuredOutput"))
                        {
                            return Value::String("end_turn".into());
                        }
                    }
                }
            }
            return m.get("stop_reason").cloned().unwrap_or(Value::Null);
        }
    }
    Value::Null
}

/// Python str.isspace()（比 Rust 的 Unicode White_Space 多 \x1c-\x1f）
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// 同 Python last_reply()：从后往前找最后一条回复。
/// 子 agent 交回结果（SubagentHandback/StructuredOutput 的 input.message，取该条中最后一个非空的）优先，
/// 否则取该消息最后一段非空 text；连续空白压成单个空格，截断到 400 个字符。
fn last_reply(lines: &[Obj]) -> Value {
    for o in lines.iter().rev() {
        if !is_str(o, "type", "assistant") {
            continue;
        }
        let Some(Value::Array(content)) = message_of(o).and_then(|m| m.get("content")) else { continue };
        let mut text: Option<&str> = None;
        for c in content.iter().filter_map(Value::as_object) {
            if is_str(c, "type", "tool_use")
                && matches!(c.get("name").and_then(Value::as_str), Some("SubagentHandback" | "StructuredOutput"))
            {
                if let Some(m) = c.get("input").and_then(Value::as_object).and_then(|i| i.get("message")).and_then(Value::as_str) {
                    if !m.chars().all(py_space) {
                        text = Some(m);
                    }
                }
            }
        }
        if text.is_none() {
            text = content.iter().rev().filter_map(Value::as_object).find_map(|c| {
                if !is_str(c, "type", "text") {
                    return None;
                }
                c.get("text").and_then(Value::as_str).filter(|t| !t.chars().all(py_space))
            });
        }
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            let joined = t.split(py_space).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
            return Value::String(joined.chars().take(400).collect());
        }
    }
    Value::Null
}

/// 尾窗中最后一条 assistant 消息：若其 content 最后一个块是 tool_use，返回工具名（清洗后截到 40 字符）；
/// SubagentHandback/StructuredOutput 不算。其余情况为 null。
fn last_tool(lines: &[Obj]) -> Value {
    let Some(o) = lines.iter().rev().find(|o| is_str(o, "type", "assistant")) else { return Value::Null };
    let Some(Value::Array(content)) = message_of(o).and_then(|m| m.get("content")) else { return Value::Null };
    let Some(Value::Object(c)) = content.last() else { return Value::Null };
    if !is_str(c, "type", "tool_use") {
        return Value::Null;
    }
    match c.get("name").and_then(Value::as_str) {
        Some("SubagentHandback" | "StructuredOutput") | None => Value::Null,
        Some(n) => Value::String(crate::text::clean(n, 40)),
    }
}

fn last_model(tail: &[Obj], head: &[Obj]) -> Option<Value> {
    let ms = models_in(tail);
    let ms = if ms.is_empty() { models_in(head) } else { ms };
    ms.last().cloned()
}

#[derive(Clone)]
struct SessInfo {
    start: Option<i64>,
    entry: Value,
    cwd: Value,
    title: Value,
    model: Value,
    branch: Value,
    stop: Value,
    reply: Value,
    tool: Value,
}

fn summarize_session(path: &Path, size: u64, prev: Option<(u64, &SessInfo)>) -> io::Result<SessInfo> {
    let (head, tail, _) = read_head_tail(path, size)?;
    let start = iso_ms(head.iter().find_map(|o| tget(o, "timestamp")));
    let entry = head.iter().chain(tail.iter()).find_map(|o| tget(o, "entrypoint")).cloned().unwrap_or(Value::Null);
    let cwd = tail
        .iter()
        .rev()
        .find_map(|o| tget(o, "cwd"))
        .or_else(|| head.iter().find_map(|o| tget(o, "cwd")))
        .cloned()
        .unwrap_or(Value::Null);
    // 标题：custom-title 优先，其次 agent-name（不再回退到 lastPrompt）
    let mut title = Value::Null;
    for o in tail.iter().rev() {
        if is_str(o, "type", "custom-title") {
            if let Some(t) = tget(o, "customTitle") {
                title = t.clone();
                break;
            }
        }
        if is_str(o, "type", "agent-name") && !truthy(&title) {
            if let Some(t) = tget(o, "agentName") {
                title = t.clone();
            }
        }
    }
    let branch = tail.iter().rev().find_map(|o| tget(o, "gitBranch")).cloned().unwrap_or(Value::Null);
    Ok(SessInfo {
        start,
        entry,
        cwd,
        title,
        model: last_model(&tail, &head).unwrap_or(Value::Null),
        branch,
        stop: last_assistant_stop(&tail),
        reply: reply_with_fallback(path, &tail, size, prev.map(|(s, p)| (s, &p.reply))),
        tool: last_tool(&tail),
    })
}

#[derive(Clone)]
struct SubInfo {
    start: Option<i64>,
    model: Value,
    typ: Value,
    desc: Value,
    bg: bool,
    stop: Value,
    msgs: usize,
    reply: Value,
    tool: Value,
}

fn summarize_sub(path: &Path, size: u64, prev: Option<(u64, &SubInfo)>) -> io::Result<SubInfo> {
    let (head, tail, _) = read_head_tail(path, size)?;
    let start = iso_ms(head.iter().find_map(|o| tget(o, "timestamp")));
    let meta: Obj = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".jsonl"))
        .and_then(|base| read_small(&path.with_file_name(format!("{base}.meta.json"))))
        .and_then(|s| match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    let model = last_model(&tail, &head).unwrap_or_else(|| meta.get("pickedEffortModel").cloned().unwrap_or(Value::Null));
    Ok(SubInfo {
        start,
        model,
        typ: tget(&meta, "agentType").cloned().unwrap_or_else(|| Value::String("agent".into())),
        desc: meta.get("description").cloned().unwrap_or(Value::Null),
        bg: is_str(&meta, "requestShape", "background"),
        stop: last_assistant_stop(&tail),
        msgs: tail.iter().filter(|o| is_str(o, "type", "assistant")).count(),
        reply: reply_with_fallback(path, &tail, size, prev.map(|(s, p)| (s, &p.reply))),
        tool: last_tool(&tail),
    })
}

/// 摘要函数：(路径, 当前 size, 上一次的 (size, 摘要))
type SummarizeFn<T> = fn(&Path, u64, Option<(u64, &T)>) -> io::Result<T>;

/// 按 (mtime_ns, size) 缓存单个 jsonl 的解析摘要；每轮结束清理本轮未访问的项。
/// 文件变化时把上一次的摘要交给摘要函数（用于追加写时沿用 reply）。
struct FileCache<T: Clone> {
    d: HashMap<PathBuf, ((i64, i64, u64), T)>,
    seen: HashSet<PathBuf>,
}

impl<T: Clone> FileCache<T> {
    fn new() -> Self {
        FileCache { d: HashMap::new(), seen: HashSet::new() }
    }
    fn get(&mut self, path: &Path, m: &fs::Metadata, f: SummarizeFn<T>) -> Option<T> {
        let sig = (m.mtime(), m.mtime_nsec(), m.size());
        self.seen.insert(path.to_path_buf());
        let prev = self.d.get(path);
        if let Some((s, v)) = prev {
            if *s == sig {
                return Some(v.clone());
            }
        }
        match f(path, m.size(), prev.map(|(s, v)| (s.2, v))) {
            Ok(v) => {
                self.d.insert(path.to_path_buf(), (sig, v.clone()));
                Some(v)
            }
            Err(_) => {
                skip();
                self.d.remove(path);
                None
            }
        }
    }
    fn sweep(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        self.d.retain(|k, _| seen.contains(k));
    }
}

// ---------------------------------------------------------------- 档位与登记表

#[derive(Clone, Debug)]
pub struct Source {
    pub id: String,
    pub label: Value,
    pub dir: String,
    pub entry: Value, // null = 不过滤；"x" = 等于；"!x" = 不等于
    /// 设置后该档位下 lead 与 agents 的 model 一律替换为此值
    pub model_label: Option<String>,
}

pub fn default_sources() -> Vec<Source> {
    let mk = |id: &str, label: &str, dir: &str, entry: Option<&str>, model_label: Option<&str>| Source {
        id: id.into(),
        label: Value::String(label.into()),
        dir: dir.into(),
        entry: entry.map(|e| Value::String(e.into())).unwrap_or(Value::Null),
        model_label: model_label.map(str::to_owned),
    };
    vec![
        mk("app", "Claude App", "~/.claude", Some("claude-desktop"), None),
        mk("cli", "claude", "~/.claude", Some("!claude-desktop"), None),
        mk("ds", "claude-ds", "~/.claude-deepseek", None, Some("DeepSeek")),
        mk("qwen", "claude-qwen", "~/.claude-qwen", None, Some("Qwen4")),
    ]
}

/// 解析档位 JSON（格式同 DEFAULT_SOURCES）
pub fn parse_sources(s: &str) -> Result<Vec<Source>, String> {
    let v: Value = serde_json::from_str(s).map_err(|e| format!("档位 JSON 解析失败: {e}"))?;
    let arr = v.as_array().ok_or("档位 JSON 必须是数组")?;
    arr.iter()
        .map(|x| {
            let o = x.as_object().ok_or("档位项必须是对象")?;
            let id = o.get("id").and_then(Value::as_str).ok_or("档位项缺少字符串 id")?;
            let dir = o.get("dir").and_then(Value::as_str).ok_or("档位项缺少字符串 dir")?;
            Ok(Source {
                id: id.into(),
                label: o.get("label").cloned().unwrap_or(Value::Null),
                dir: dir.into(),
                entry: o.get("entry").cloned().unwrap_or(Value::Null),
                model_label: o.get("model_label").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
            })
        })
        .collect()
}

fn entry_match(rule: &Value, entry: &Value) -> bool {
    match rule {
        Value::Null => true,
        Value::String(r) => match r.strip_prefix('!') {
            Some(neg) => entry.as_str() != Some(neg),
            None => entry.as_str() == Some(r.as_str()),
        },
        other => other == entry,
    }
}

fn pid_alive(pid: i64) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: kill(pid, 0) 只做存在性/权限检查，不发送信号
    if unsafe { libc::kill(p, 0) } == 0 {
        return true;
    }
    // ESRCH = 进程不存在；EPERM 等其他错误视为存活
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

pub struct Config {
    pub pid_check: bool,
    pub registry_ttl: f64, // 秒
    pub host: String,
    pub redact: bool,
    /// Claude App 的会话元数据目录（claude-code-sessions）；None = 不过滤已归档会话
    pub app_sessions: Option<PathBuf>,
}

// ---------------------------------------------------------------- Claude App 归档标记

/// App 元数据单个文件最多读这么多（文件里带 MCP 工具清单，常见几十 KiB）
const APP_META_MAX: u64 = 8 * 1024 * 1024;

/// 一个 App 会话（local_*.json）：是否已归档 + 它用过的 CLI 会话 id
#[derive(Clone)]
struct AppMeta {
    archived: bool,
    ids: Vec<String>,
}

fn summarize_app_meta(path: &Path, _size: u64, _prev: Option<(u64, &AppMeta)>) -> io::Result<AppMeta> {
    let mut buf = Vec::new();
    File::open(path)?.take(APP_META_MAX).read_to_end(&mut buf)?;
    let Value::Object(o) = serde_json::from_slice::<Value>(&buf).map_err(io::Error::other)? else {
        return Err(io::Error::other("not an object"));
    };
    let mut ids: Vec<String> = Vec::new();
    let mut add = |v: Option<&Value>| {
        if let Some(s) = v.and_then(Value::as_str).filter(|s| !s.is_empty()) {
            ids.push(s.to_owned());
        }
    };
    add(o.get("cliSessionId"));
    add(o.get("lastAssistantCliSessionId"));
    if let Some(Value::Array(a)) = o.get("priorCliSessionIds") {
        a.iter().for_each(|v| add(Some(v)));
    }
    Ok(AppMeta { archived: o.get("isArchived") == Some(&Value::Bool(true)), ids })
}

/// 列出 root 下（最多 3 层）的 local_*.json
fn app_meta_files(root: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name();
        let name = name.to_string_lossy();
        if ft.is_dir() && depth > 0 {
            app_meta_files(&e.path(), depth - 1, out);
        } else if ft.is_file() && name.starts_with("local_") && name.ends_with(".json") {
            out.push(e.path());
        }
    }
}

fn load_registry(cfg: &Path, conf: &Config) -> HashMap<String, Obj> {
    let mut reg = HashMap::new();
    let d = cfg.join("sessions");
    if !is_dir(&d) {
        return reg;
    }
    for (name, f) in list_dir(&d) {
        if !name.ends_with(".json") {
            continue;
        }
        let Some(text) = read_small(&f) else {
            skip();
            continue;
        };
        let Ok(Value::Object(r)) = serde_json::from_str::<Value>(&text) else {
            skip();
            continue;
        };
        let Some(sid) = tget(&r, "sessionId").and_then(Value::as_str).map(str::to_owned) else { continue };
        // pid 必须是正整数（布尔不算）
        let Some(pid) = r.get("pid").and_then(Value::as_i64).filter(|&p| p > 0) else { continue };
        if conf.pid_check {
            if !pid_alive(pid) {
                continue;
            }
        } else {
            let Ok(m) = fs::metadata(&f) else { continue };
            let upd = match r.get("updatedAt") {
                Some(v @ Value::Number(_)) if truthy(v) => v.as_f64().unwrap_or(0.0),
                _ => 0.0,
            };
            let fresh = mtime_f(&m).max(upd / 1000.0);
            if now_f() - fresh > conf.registry_ttl {
                continue;
            }
        }
        reg.insert(sid, r);
    }
    reg
}

// ---------------------------------------------------------------- 输出结构（字段顺序同 Python）

#[derive(Serialize, Clone)]
pub struct SrcStat {
    pub id: String,
    pub label: Value,
    pub dir: String,
    pub online: u64,
    pub working: u64,
    pub sessions: u64,
    pub agents: u64,
}

#[derive(Serialize, Clone)]
pub struct Agent {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub typ: Value,
    pub task: Value,
    pub model: Value,
    pub bg: bool,
    pub status: &'static str,
    pub start: i64,
    pub last: i64,
    pub reply: Value,
    pub tool: Value,
}

#[derive(Serialize, Clone)]
pub struct Lead {
    pub id: String,
    pub src: String,
    pub name: String,
    pub title: Value,
    pub project: String,
    pub branch: Value,
    pub model: Value,
    pub pid: Value,
    pub status: &'static str,
    pub start: i64,
    pub last: Num,
    pub reply: Value,
    pub tool: Value,
    pub agents: Vec<Agent>,
    pub face: String,
}

#[derive(Serialize, Clone)]
pub struct Event {
    pub src: String,
    pub lid: String,
    pub aid: Option<String>,
    pub who: Option<String>,
    pub job: Value,
    pub task: Value,
    pub at: Num,
    pub kind: &'static str,
    pub lead: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct State {
    pub now: i64,
    pub hours: f64,
    pub host: String,
    pub sources: Vec<SrcStat>,
    pub leads: Vec<Lead>,
    pub events: Vec<Event>,
}

pub fn status_order(s: &str) -> u8 {
    match s {
        "working" => 0,
        "idle" => 1,
        "stopped" => 2,
        "done" => 3,
        _ => 9,
    }
}

// ---------------------------------------------------------------- 扫描器

pub struct Scanner {
    pub conf: Config,
    names: Vec<String>,
    lead_names: Vec<String>,
    sess_cache: FileCache<SessInfo>,
    sub_cache: FileCache<SubInfo>,
    app_cache: FileCache<AppMeta>,
}

impl Scanner {
    pub fn new(conf: Config) -> Self {
        let (names, lead_names) = name_pools();
        Scanner { conf, names, lead_names, sess_cache: FileCache::new(), sub_cache: FileCache::new(), app_cache: FileCache::new() }
    }

    /// 在 Claude App 里已归档的 CLI 会话 id。同一 id 也出现在未归档的 App 会话里时不算归档。
    fn archived_ids(&mut self) -> HashSet<String> {
        let mut out = HashSet::new();
        let Some(root) = self.conf.app_sessions.clone() else { return out };
        let mut files = Vec::new();
        app_meta_files(&root, 3, &mut files);
        let mut keep = HashSet::new();
        for f in files {
            let Ok(m) = fs::metadata(&f) else { continue };
            let Some(meta) = self.app_cache.get(&f, &m, summarize_app_meta) else { continue };
            let set = if meta.archived { &mut out } else { &mut keep };
            set.extend(meta.ids);
        }
        self.app_cache.sweep();
        out.retain(|id| !keep.contains(id));
        out
    }

    pub fn scan(&mut self, sources: &[Source], hours: f64) -> State {
        let now = (now_f() * 1000.0) as i64;
        let horizon = now_f() - hours * 3600.0;
        let mut leads: Vec<Lead> = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        let mut src_stats: Vec<SrcStat> = Vec::new();
        let archived = self.archived_ids();

        // 按 dir 原样分组，保持首次出现顺序
        let mut by_dir: Vec<(String, Vec<&Source>)> = Vec::new();
        for s in sources {
            match by_dir.iter_mut().find(|(d, _)| *d == s.dir) {
                Some((_, v)) => v.push(s),
                None => by_dir.push((s.dir.clone(), vec![s])),
            }
        }

        for (dir_, srcs) in &by_dir {
            let cfg = expand_user(dir_);
            let reg = load_registry(&cfg, &self.conf);
            let proj = cfg.join("projects");
            for s in srcs {
                let st = SrcStat {
                    id: s.id.clone(),
                    label: s.label.clone(),
                    dir: dir_.clone(),
                    online: 0,
                    working: 0,
                    sessions: 0,
                    agents: 0,
                };
                // 同 Python dict 赋值：同 id 覆盖值但保留原位置
                match src_stats.iter_mut().find(|x| x.id == s.id) {
                    Some(x) => *x = st,
                    None => src_stats.push(st),
                }
            }
            if !is_dir(&proj) {
                continue;
            }
            for (_, pdir) in list_dir(&proj) {
                if !is_dir(&pdir) {
                    continue;
                }
                let pdir_name = pdir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
                for (jname, jf) in list_dir(&pdir) {
                    if !jname.ends_with(".jsonl") {
                        continue;
                    }
                    let Ok(st) = fs::metadata(&jf) else {
                        skip();
                        continue;
                    };
                    if !st.is_file() {
                        continue;
                    }
                    let sid = stem(&jname).to_owned();
                    if archived.contains(&sid) {
                        continue; // 已在 Claude App 里归档：不再上报
                    }
                    let subdir = pdir.join(&sid).join("subagents");
                    let mut sub_files: Vec<(String, PathBuf, fs::Metadata)> = Vec::new();
                    if is_dir(&subdir) {
                        for (sname, sf) in list_dir(&subdir) {
                            if !(sname.len() >= 12 && sname.starts_with("agent-") && sname.ends_with(".jsonl")) {
                                continue;
                            }
                            let Ok(sst) = fs::metadata(&sf) else {
                                skip();
                                continue;
                            };
                            if sst.is_file() && mtime_f(&sst) >= horizon {
                                sub_files.push((sname, sf, sst));
                            }
                        }
                    }
                    let r = reg.get(&sid);
                    if mtime_f(&st) < horizon && sub_files.is_empty() && r.is_none() {
                        continue;
                    }
                    let Some(info) = self.sess_cache.get(&jf, &st, summarize_session) else { continue };
                    let rget = |k: &str| r.and_then(|r| r.get(k));
                    let entry = or_chain(&[rget("entrypoint"), Some(&info.entry)]).unwrap_or(Value::Null);
                    let Some(src) = srcs.iter().find(|s| entry_match(&s.entry, &entry)) else { continue };

                    let age = now_f() - mtime_f(&st);
                    let mut status: &'static str = if let Some(r) = r {
                        if is_str(r, "status", "busy") {
                            "working"
                        } else {
                            "idle"
                        }
                    } else if age < LIVE_SECS {
                        "working"
                    } else if matches!(info.stop.as_str(), Some("end_turn" | "stop_sequence")) || info.stop.is_null() {
                        "done"
                    } else {
                        "stopped"
                    };

                    let cwd = or_chain(&[rget("cwd"), Some(&info.cwd)]);
                    let cwd_s = cwd.as_ref().and_then(Value::as_str).unwrap_or("");
                    let mut project = if cwd.is_some() { project_of(cwd_s) } else { pdir_name.clone() };
                    if cwd_s.contains("scratch-workspaces") {
                        project = "scratch".into();
                    }
                    // 标题链：registry.name → custom-title → agent-name → project；脱敏时只用 project
                    let title = if self.conf.redact {
                        Value::String(project.clone())
                    } else {
                        or_chain(&[rget("name"), Some(&info.title)]).unwrap_or_else(|| Value::String(project.clone()))
                    };
                    let lead_name = pick(&self.lead_names, &format!("{}{}", src.id, sid)).to_owned();
                    let st_ms = mtime_ms(&st);
                    // 档位设了 model_label 时统一显示该值
                    let fixed_model = src.model_label.as_ref().map(|m| Value::String(m.clone()));
                    let start = match info.start {
                        Some(v) if v != 0 => v,
                        _ => st_ms,
                    };
                    let upd = match rget("updatedAt") {
                        Some(v) if truthy(v) => Num::from_value(Some(v)),
                        _ => Num::I(0),
                    };
                    let mut lead = Lead {
                        id: sid.clone(),
                        src: src.id.clone(),
                        name: lead_name.clone(),
                        title: title.clone(),
                        project,
                        branch: if self.conf.redact { Value::Null } else { info.branch.clone() },
                        model: fixed_model.clone().unwrap_or_else(|| info.model.clone()),
                        pid: rget("pid").cloned().unwrap_or(Value::Null),
                        status,
                        start,
                        last: Num::max(Num::I(st_ms), upd),
                        reply: if self.conf.redact { Value::Null } else { info.reply.clone() },
                        tool: info.tool.clone(),
                        agents: Vec::new(),
                        face: lead_name,
                    };
                    for (sname, sf, sst) in &sub_files {
                        let Some(si) = self.sub_cache.get(sf, sst, summarize_sub) else { continue };
                        let aid = stem(sname).replace("agent-", "");
                        let sage = now_f() - mtime_f(sst);
                        let sstatus: &'static str = if sage < LIVE_SECS {
                            "working"
                        } else if matches!(si.stop.as_str(), Some("end_turn" | "stop_sequence")) {
                            "finished"
                        } else if si.msgs == 0 {
                            "failed"
                        } else {
                            "stopped"
                        };
                        // 任一子 agent 在工作，主会话 idle/done/stopped 都提升为 working
                        if sstatus == "working" && matches!(status, "idle" | "done" | "stopped") {
                            status = "working";
                            lead.status = status;
                        }
                        let a_last = mtime_ms(sst);
                        let a = Agent {
                            id: aid.clone(),
                            name: pick(&self.names, &aid).to_owned(),
                            typ: si.typ.clone(),
                            task: if self.conf.redact { Value::Null } else { si.desc.clone() },
                            model: fixed_model.clone().unwrap_or_else(|| si.model.clone()),
                            bg: si.bg,
                            status: sstatus,
                            start: match si.start {
                                Some(v) if v != 0 => v,
                                _ => a_last,
                            },
                            last: a_last,
                            reply: if self.conf.redact { Value::Null } else { si.reply.clone() },
                            tool: si.tool.clone(),
                        };
                        let ev = |at: Num, kind: &'static str| Event {
                            src: src.id.clone(),
                            lid: sid.clone(),
                            aid: Some(aid.clone()),
                            who: Some(a.name.clone()),
                            job: a.typ.clone(),
                            task: a.task.clone(),
                            at,
                            kind,
                            lead: None,
                        };
                        events.push(ev(Num::I(a.start), "on"));
                        if sstatus != "working" {
                            events.push(ev(Num::I(a.last), sstatus));
                        }
                        lead.agents.push(a);
                    }
                    lead.agents.sort_by(|a, b| (a.status != "working").cmp(&(b.status != "working")).then(b.last.cmp(&a.last)));
                    let lev = |at: Num, kind: &'static str| Event {
                        src: src.id.clone(),
                        lid: sid.clone(),
                        aid: None,
                        who: None,
                        job: Value::String("lead".into()),
                        task: if self.conf.redact { Value::Null } else { title.clone() },
                        at,
                        kind,
                        lead: None,
                    };
                    events.push(lev(Num::I(lead.start), "on"));
                    if matches!(lead.status, "done" | "stopped") {
                        events.push(lev(lead.last, lead.status));
                    }

                    if let Some(ss) = src_stats.iter_mut().find(|x| x.id == src.id) {
                        ss.sessions += 1;
                        ss.agents += lead.agents.len() as u64;
                        if matches!(lead.status, "working" | "idle") {
                            ss.online += 1;
                        }
                        if lead.status == "working" || lead.agents.iter().any(|x| x.status == "working") {
                            ss.working += 1;
                        }
                    }
                    leads.push(lead);
                }
            }
        }
        self.sess_cache.sweep();
        self.sub_cache.sweep();

        // 主会话代号去重：按开始时间先到先得，撞名则顺延
        let ln = &self.lead_names;
        let mut idx: Vec<usize> = (0..leads.len()).collect();
        idx.sort_by_key(|&i| leads[i].start); // 稳定排序，同 Python sorted
        let mut used: HashSet<String> = HashSet::new();
        for i in idx {
            let l = &mut leads[i];
            let base = l.name.clone();
            let bi = ln.iter().position(|x| *x == base).unwrap_or(0);
            let mut n = 0;
            l.face = base.clone();
            while used.contains(&l.name) && n < ln.len() {
                n += 1;
                if let Some(nm) = ln.get((bi + n) % ln.len()) {
                    l.name = nm.clone();
                    l.face = nm.clone();
                }
            }
            if used.contains(&l.name) {
                // 名字池用完：同脸加序号
                let mut k = 2;
                while used.contains(&format!("{base} {k}")) {
                    k += 1;
                }
                l.name = format!("{base} {k}");
                l.face = base.clone();
            }
            used.insert(l.name.clone());
        }
        let names: HashMap<&str, &str> = leads.iter().map(|l| (l.id.as_str(), l.name.as_str())).collect();
        for e in &mut events {
            e.lead = names.get(e.lid.as_str()).map(|s| (*s).to_owned());
            if e.who.is_none() {
                e.who = e.lead.clone();
            }
        }
        leads.sort_by(|a, b| {
            status_order(a.status).cmp(&status_order(b.status)).then(b.last.f().total_cmp(&a.last.f()))
        });
        events.sort_by(|a, b| b.at.f().total_cmp(&a.at.f()));
        events.truncate(400);
        State { now, hours, host: self.conf.host.clone(), sources: src_stats, leads, events }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_ids_from_app_meta() {
        let root = std::env::temp_dir().join(format!("deck-push-test-{}-app", std::process::id()));
        let d = root.join("acct").join("org");
        fs::create_dir_all(&d).unwrap();
        let w = |n: &str, v: &str| fs::write(d.join(n), v).unwrap();
        w("local_a.json", r#"{"isArchived":true,"cliSessionId":"s1","priorCliSessionIds":["s0","shared"]}"#);
        w("local_b.json", r#"{"isArchived":false,"cliSessionId":"s2","priorCliSessionIds":["shared"]}"#);
        w("local_c.json", "not json");
        w("deleted_x", "1");
        let conf = Config { pid_check: false, registry_ttl: 0.0, host: String::new(), redact: false, app_sessions: Some(root.clone()) };
        let mut sc = Scanner::new(conf);
        let mut got: Vec<String> = sc.archived_ids().into_iter().collect();
        got.sort();
        assert_eq!(got, vec!["s0", "s1"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pick_matches_python() {
        // python3 -c 'import hashlib;print(int(hashlib.sha1(b"abc").hexdigest()[:8],16))' → 2845392438
        let seq: Vec<String> = (0..7).map(|i| i.to_string()).collect();
        assert_eq!(pick(&seq, "abc"), (2845392438u64 % 7).to_string());
    }

    use std::io::Write as _;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("deck-push-test-{}-{name}.jsonl", std::process::id()))
    }

    /// 约 n 字节的 tool_result 行（user）与 tool_use 行（assistant）交替
    fn tool_noise(f: &mut File, n: usize) {
        let blob = "x".repeat(8 * 1024);
        let mut w = 0;
        while w < n {
            let u = format!("{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"content\":\"{blob}\"}}]}}}}\n");
            let a = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"hmm\"},{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{}}]}}\n";
            f.write_all(u.as_bytes()).unwrap();
            f.write_all(a.as_bytes()).unwrap();
            w += u.len() + a.len();
        }
    }

    fn text_line(t: &str) -> String {
        format!("{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}]}}}}\n")
    }

    fn size_of(p: &Path) -> u64 {
        fs::metadata(p).unwrap().len()
    }

    #[test]
    fn reply_found_before_tail_window() {
        let p = tmp("back");
        let mut f = File::create(&p).unwrap();
        f.write_all(b"{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();
        tool_noise(&mut f, 100 * 1024);
        f.write_all(text_line("early   reply\\n  text").as_bytes()).unwrap();
        tool_noise(&mut f, 300 * 1024); // 最后 300KiB 都是工具输出
        drop(f);
        let size = size_of(&p);
        let (_, tail, start) = read_head_tail(&p, size).unwrap();
        assert!(start > 0 && last_reply(&tail).is_null(), "text 应在尾窗之外");
        let s = summarize_session(&p, size, None).unwrap();
        assert_eq!(s.reply, Value::String("early reply text".into()));
        assert_eq!(s.tool, Value::String("Bash".into()));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn reply_line_longer_than_chunk() {
        // 回复本身是一行超过 256KiB 的长文本，跨越多个回看块
        let p = tmp("long");
        let mut f = File::create(&p).unwrap();
        tool_noise(&mut f, 50 * 1024);
        let long = format!("head {}", "y".repeat(600 * 1024));
        f.write_all(text_line(&long).as_bytes()).unwrap();
        tool_noise(&mut f, 300 * 1024);
        drop(f);
        let s = summarize_session(&p, size_of(&p), None).unwrap();
        let r = s.reply.as_str().unwrap();
        assert!(r.starts_with("head yyy") && r.chars().count() == 400);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn reply_reused_from_cache_on_append() {
        let p = tmp("cache");
        let mut f = File::create(&p).unwrap();
        tool_noise(&mut f, 100 * 1024);
        f.write_all(text_line("first answer").as_bytes()).unwrap();
        tool_noise(&mut f, 300 * 1024);
        drop(f);
        let mut cache: FileCache<SessInfo> = FileCache::new();
        let m1 = fs::metadata(&p).unwrap();
        let s1 = cache.get(&p, &m1, summarize_session).unwrap();
        assert_eq!(s1.reply, Value::String("first answer".into()));
        // 追加写：尾窗内仍无 text
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        tool_noise(&mut f, 300 * 1024);
        drop(f);
        let size2 = size_of(&p);
        // 上一次摘要（用哨兵值区分“沿用缓存”与“重新回看”）
        let mut prev = s1.clone();
        prev.reply = Value::String("cached".into());
        let s2 = summarize_session(&p, size2, Some((m1.size(), &prev))).unwrap();
        assert_eq!(s2.reply, Value::String("cached".into()), "追加写应沿用上次的 reply");
        // 文件变小（被重写）时不沿用，改为回看
        let s3 = summarize_session(&p, size2, Some((size2 + 1, &prev))).unwrap();
        assert_eq!(s3.reply, Value::String("first answer".into()));
        // 走 FileCache：签名变化后拿到沿用的 reply
        let m2 = fs::metadata(&p).unwrap();
        let s4 = cache.get(&p, &m2, summarize_session).unwrap();
        assert_eq!(s4.reply, Value::String("first answer".into()));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn tool_rules() {
        let parse = |s: &str| parse_lines(s.as_bytes(), false);
        let l = parse("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\"}]}}\n{\"type\":\"user\"}\n");
        assert_eq!(last_tool(&l), Value::String("Read".into()));
        let l = parse("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"SubagentHandback\"}]}}\n");
        assert!(last_tool(&l).is_null());
        let l = parse("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\"},{\"type\":\"text\",\"text\":\"ok\"}]}}\n");
        assert!(last_tool(&l).is_null());
        let long = format!("{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"mcp\\u202e{}\"}}]}}}}\n", "z".repeat(60));
        let t = last_tool(&parse(&long));
        assert_eq!(t.as_str().map(|s| s.chars().count()), Some(40));
        assert!(!t.as_str().unwrap().contains('\u{202e}'));
    }

    #[test]
    fn helpers() {
        assert_eq!(stem("abc.jsonl"), "abc");
        assert_eq!(stem(".jsonl"), ".jsonl");
        assert_eq!(path_name("/a/b/"), "b");
        assert_eq!(path_name("/a/./"), "a");
        assert_eq!(path_name("/"), "");
        assert_eq!(project_of("/x/plinth/.claude/worktrees/review-4e223e"), "plinth");
        assert_eq!(project_of("/x/plinth/.claude/worktrees/review-4e223e/sub"), "plinth");
        assert_eq!(project_of("/x/Oh_My_Agent"), "Oh_My_Agent");
        assert_eq!(project_of("/.claude/worktrees/a"), "a");
        assert_eq!(parse_lines(b"{\"a\":1}\n[1]\n{bad\n{\"b\":", true).len(), 1);
        assert_eq!(parse_lines(b"{\"a\":1}\n{\"b\":2}", false).len(), 2);
    }
}
