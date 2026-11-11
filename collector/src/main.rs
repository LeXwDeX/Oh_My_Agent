//! Agent Deck 推送采集器（deck-push）：替代 server.py 的 push 模式。
//!
//! 只读扫描本机 Claude Code 各档位目录，每 N 秒把 `{"device": 名字, "state": 扫描结果}`
//! POST 到 `<hub>/api/ingest`。`--once` 只扫描一次并把 state 打印到 stdout。

#[macro_use]
mod logging;
mod scan;
mod text;
mod timefmt;
mod trim;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use scan::{Config, Scanner, Source};
use trim::trim_and_encode;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 序列化后的请求体上限（hub 与 nginx 允许 1MiB，这里留足余量）
const MAX_BODY: usize = 900 * 1024;

/// 失败退避上限（秒）
const BACKOFF_MAX: f64 = 300.0;

/// 心跳汇总间隔
const HEARTBEAT: Duration = Duration::from_secs(600);

/// 收到 SIGTERM/SIGINT 后置位，主循环写一行"退出"后结束
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: 处理函数只写一个原子变量，属于异步信号安全操作
    unsafe {
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
}

/// panic 信息先写进日志，再交给默认 hook
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!("panic: {info}");
        default(info);
    }));
}

fn usage() -> ! {
    eprintln!(
        "deck-push {VERSION} — Agent Deck 推送采集器\n\
         用法：deck-push [选项]\n\
         \x20 --push-url <url>    hub 地址（推送时必填；环境变量 DECK_PUSH_URL）\n\
         \x20 --name <名字>       设备名（DECK_NAME，缺省为主机名第一段）\n\
         \x20 --interval <秒>     推送间隔，1–3600（DECK_PUSH_INTERVAL，缺省 3）\n\
         \x20 --hours <小时>      回看多少小时的会话，(0, 168]（DECK_HOURS，缺省 12）\n\
         \x20 --sources <文件>    自定义档位 JSON 文件；也可用 DECK_SOURCES 直接给 JSON 字符串\n\
         \x20 --redact            脱敏：标题只用项目名，不上报分支与任务描述（DECK_REDACT=1）\n\
         \x20 --log-file <路径>   日志写入文件并按大小轮转（DECK_LOG_FILE；缺省写 stderr）\n\
         \x20 --once              扫描一次，把 state JSON 打印到 stdout 后退出（不推送）\n\
         其他环境变量：DECK_PID_CHECK、DECK_REGISTRY_TTL_HOURS（同 server.py）、\n\
         \x20 DECK_LOG_MAX_BYTES（缺省 1048576）、DECK_LOG_KEEP（缺省 3）"
    );
    exit(2)
}

/// 启动错误：已设置日志文件时写进日志（launchd 下 stderr 指向 /dev/null），否则写 stderr
fn die(msg: &str) -> ! {
    error!("deck-push: {msg}");
    exit(2)
}

/// 同 server.py env_flag()
fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "no" | "off" | ""),
        Err(_) => default,
    }
}

/// 取非空环境变量
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// 同 Python float()：允许首尾空白
fn parse_f64(what: &str, s: &str) -> f64 {
    match s.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => v,
        _ => die(&format!("{what} 不是有效数字: {s:?}")),
    }
}

/// os.uname().nodename.split(".")[0]
fn hostname() -> String {
    // SAFETY: utsname 为纯数据结构，uname 只写入传入的结构体
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return String::new();
    }
    let bytes: Vec<u8> = u.nodename.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    let s = String::from_utf8_lossy(&bytes);
    s.split('.').next().unwrap_or("").to_owned()
}

/// 设备名清洗：去掉 Cf 类字符与 <0x20、0x7f 控制字符；长度须为 1–40 个字符
fn clean_device(raw: &str) -> Result<String, String> {
    let s: String = raw.chars().filter(|&c| !text::is_banned(c)).collect();
    let n = s.chars().count();
    if (1..=40).contains(&n) {
        Ok(s)
    } else {
        Err(format!("设备名清洗后长度为 {n}，须为 1–40 个字符: {raw:?}"))
    }
}

/// Python 风格的浮点显示（3 → "3.0"）
fn fmt_f(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

struct Opts {
    push_url: Option<String>,
    name: Option<String>,
    interval: f64,
    hours: f64,
    sources: Vec<Source>,
    redact: bool,
    once: bool,
}

/// 同 hub 的 SRC_ID_RE：^[a-z0-9_-]{1,24}$
fn valid_src_id(id: &str) -> bool {
    (1..=24).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// 先于其他参数解析：从 argv（--log-file / --log-file=）或 DECK_LOG_FILE 取日志文件并初始化，
/// 保证之后的参数错误（die）也能写进日志文件。
fn init_log_early() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--log-file" {
            path = args.get(i + 1).cloned();
            i += 1;
        } else if let Some(v) = args[i].strip_prefix("--log-file=") {
            path = Some(v.to_owned());
        }
        i += 1;
    }
    let Some(f) = path.filter(|s| !s.is_empty()).or_else(|| env_nonempty("DECK_LOG_FILE")) else { return };
    let max = std::env::var("DECK_LOG_MAX_BYTES").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1_048_576).max(1);
    let keep = std::env::var("DECK_LOG_KEEP").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(3);
    logging::init_file(PathBuf::from(f), max, keep);
}

fn parse_args() -> Opts {
    let mut push_url = None;
    let mut name = None;
    let mut interval = None;
    let mut hours = None;
    let mut sources_file = None;
    let mut redact = false;
    let mut once = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        };
        let mut val = |k: &str| inline.clone().or_else(|| it.next()).unwrap_or_else(|| die(&format!("{k} 需要参数值")));
        match key.as_str() {
            "--push-url" => push_url = Some(val("--push-url")),
            "--name" => name = Some(val("--name")),
            "--interval" => interval = Some(val("--interval")),
            "--hours" => hours = Some(val("--hours")),
            "--sources" => sources_file = Some(val("--sources")),
            "--log-file" => {
                let _ = val("--log-file"); // 已在 init_log_early() 中处理
            }
            "--redact" => redact = true,
            "--once" => once = true,
            "-V" | "--version" => {
                println!("deck-push {VERSION}");
                exit(0)
            }
            "-h" | "--help" => usage(),
            _ => {
                error!("deck-push: 未知参数: {a}");
                usage()
            }
        }
    }
    // 命令行优先，其次环境变量
    // interval 夹在 [1, 3600] 秒；hours 须在 (0, 168]，超过 168 夹到 168
    let interval_raw = parse_f64("interval", &interval.or_else(|| env_nonempty("DECK_PUSH_INTERVAL")).unwrap_or_else(|| "3".into()));
    let interval = interval_raw.clamp(1.0, 3600.0);
    if interval != interval_raw {
        warn!("interval {interval_raw} 超出 [1, 3600]，按 {interval} 秒处理");
    }
    let hours_raw = parse_f64("hours", &hours.or_else(|| env_nonempty("DECK_HOURS")).unwrap_or_else(|| "12".into()));
    if hours_raw <= 0.0 {
        die(&format!("hours 须大于 0: {hours_raw}"));
    }
    let hours = hours_raw.min(168.0);
    if hours != hours_raw {
        warn!("hours {hours_raw} 超过 168，按 168 处理");
    }
    let sources = if let Some(f) = sources_file.filter(|s| !s.is_empty()) {
        let text = std::fs::read_to_string(&f).unwrap_or_else(|e| die(&format!("读取档位文件 {f} 失败: {e}")));
        scan::parse_sources(&text).unwrap_or_else(|e| die(&e))
    } else if let Some(j) = env_nonempty("DECK_SOURCES") {
        scan::parse_sources(&j).unwrap_or_else(|e| die(&format!("DECK_SOURCES: {e}")))
    } else {
        scan::default_sources()
    };
    for s in &sources {
        if !valid_src_id(&s.id) {
            die(&format!("档位 id 不合法（须匹配 ^[a-z0-9_-]{{1,24}}$）: {:?}", s.id));
        }
    }
    Opts {
        push_url: push_url.filter(|s| !s.is_empty()).or_else(|| env_nonempty("DECK_PUSH_URL")),
        name: name.filter(|s| !s.is_empty()).or_else(|| env_nonempty("DECK_NAME")),
        interval,
        hours,
        sources,
        redact: redact || env_flag("DECK_REDACT", false),
        once,
    }
}

/// 推送失败：HTTP 状态码（含响应体摘要）或其他错误
enum PostErr {
    Status(u16, String),
    Other(String),
}

impl std::fmt::Display for PostErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostErr::Status(code, body) => write!(f, "HTTP {code}: {body}"),
            PostErr::Other(e) => f.write_str(e),
        }
    }
}

/// 先走系统解析；失败时只查 IPv4 再试一次。
/// 原因：有的内网 DNS 对 AAAA 查询回 NXDOMAIN（而不是空的 NOERROR），
/// musl 的 getaddrinfo(AF_UNSPEC) 只要有一个查询是 NXDOMAIN 就整体失败，导致 alpine 容器里推送永远解析不了。
fn resolve_with_ipv4_fallback(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    match netloc.to_socket_addrs() {
        Ok(it) => {
            let v: Vec<_> = it.collect();
            if !v.is_empty() {
                return Ok(v);
            }
        }
        Err(e) => {
            if let Some(v) = getaddrinfo_v4(netloc) {
                return Ok(v);
            }
            return Err(e);
        }
    }
    getaddrinfo_v4(netloc).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))
}

fn getaddrinfo_v4(netloc: &str) -> Option<Vec<std::net::SocketAddr>> {
    use std::ffi::CString;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    let (host, port) = netloc.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.starts_with('[') {
        return None; // IPv6 字面量不走这个兜底
    }
    let c_host = CString::new(host).ok()?;
    // SAFETY: hints 全零初始化后只设置 family/socktype；res 由 getaddrinfo 分配，遍历后用 freeaddrinfo 释放。
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        if libc::getaddrinfo(c_host.as_ptr(), std::ptr::null(), &hints, &mut res) != 0 || res.is_null() {
            return None;
        }
        let mut out = Vec::new();
        let mut p = res;
        while !p.is_null() {
            let ai = &*p;
            if ai.ai_family == libc::AF_INET && !ai.ai_addr.is_null() {
                let sin = &*(ai.ai_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let sa = SocketAddr::V4(SocketAddrV4::new(ip, port));
                if !out.contains(&sa) {
                    out.push(sa);
                }
            }
            p = ai.ai_next;
        }
        libc::freeaddrinfo(res);
        if out.is_empty() { None } else { Some(out) }
    }
}

fn post(agent: &ureq::Agent, url: &str, body: &[u8]) -> Result<(), PostErr> {
    let resp = agent.post(url).set("Content-Type", "application/json").send_bytes(body).map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let mut buf = Vec::new();
            let _ = r.into_reader().take(4096).read_to_end(&mut buf);
            let text: String = String::from_utf8_lossy(&buf).chars().filter(|&c| !text::is_banned(c)).take(200).collect();
            PostErr::Status(code, text.trim().to_owned())
        }
        ureq::Error::Transport(t) => PostErr::Other(format!("网络错误: {t}")),
    })?;
    let mut sink = Vec::new();
    resp.into_reader().take(64 * 1024).read_to_end(&mut sink).map_err(|e| PostErr::Other(format!("读取响应失败: {e}")))?;
    Ok(())
}

/// 0–1 之间的伪随机数（退避抖动用，不需要密码学强度）
fn rand01() -> f64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut x = t ^ ((std::process::id() as u64) << 32) ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// 指数退避：失败 n 次后等待 min(300, interval·2^n)，再加 0–20% 抖动；未失败时等 interval
fn backoff(interval: f64, fails: u64) -> f64 {
    if fails == 0 {
        return interval;
    }
    let base = (interval * 2f64.powi(fails.min(30) as i32)).min(BACKOFF_MAX);
    base * (1.0 + 0.2 * rand01())
}

/// 心跳统计
#[derive(Default)]
struct Stats {
    ok: u64,
    fail: u64,
    scans: u64,
    scan_ms: f64,
}

/// 可被 SIGTERM/SIGINT 打断的 sleep
fn nap(d: Duration) {
    let until = Instant::now() + d;
    while !STOP.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        sleep(left.min(Duration::from_millis(200)));
    }
}

fn push_loop(mut scanner: Scanner, url: &str, device: &str, sources: &[Source], hours: f64, interval: f64) {
    let target = format!("{}/api/ingest", url.trim_end_matches('/'));
    let labels: Vec<String> = sources
        .iter()
        .map(|s| s.label.as_str().map(str::to_owned).unwrap_or_else(|| s.label.to_string()))
        .collect();
    info!(
        "启动 deck-push {VERSION}：设备 {device}，hub {target}，档位 {labels:?}，每 {}s，回看 {}h，PID 检查 {}，脱敏 {}",
        fmt_f(interval),
        fmt_f(hours),
        if scanner.conf.pid_check { "开" } else { "关" },
        if scanner.conf.redact { "开" } else { "关" }
    );
    let agent = ureq::AgentBuilder::new()
        .resolver(resolve_with_ipv4_fallback)
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .user_agent(&format!("agent-deck-push/{VERSION}"))
        .build();
    let mut fails: u64 = 0;
    let mut first_ok = false;
    let mut stats = Stats::default();
    let mut beat_at = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        let t0 = Instant::now();
        let state = scanner.scan(sources, hours);
        stats.scans += 1;
        stats.scan_ms += t0.elapsed().as_secs_f64() * 1000.0;
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        let (body, sizes) = trim_and_encode(device, state, MAX_BODY);
        match post(&agent, &target, &body) {
            Ok(()) => {
                stats.ok += 1;
                if !first_ok {
                    first_ok = true;
                    info!(
                        "首次推送成功：payload {} 字节，leads {}，agents {}，events {}",
                        sizes.bytes, sizes.leads, sizes.agents, sizes.events
                    );
                }
                if fails > 0 {
                    info!("恢复推送（之前失败 {fails} 次）");
                }
                fails = 0;
            }
            Err(e) => {
                stats.fail += 1;
                fails += 1;
                if fails == 1 || fails == 10 || fails.is_multiple_of(100) {
                    match &e {
                        PostErr::Status(code, _) if (400..500).contains(code) => warn!(
                            "推送失败 #{fails}: {e}（payload {} 字节，leads {}，agents {}，events {}）",
                            sizes.bytes, sizes.leads, sizes.agents, sizes.events
                        ),
                        _ => warn!("推送失败 #{fails}: {e}"),
                    }
                }
            }
        }
        if beat_at.elapsed() >= HEARTBEAT {
            let avg = if stats.scans > 0 { stats.scan_ms / stats.scans as f64 } else { 0.0 };
            let skipped = scan::SKIPPED.swap(0, Ordering::Relaxed);
            info!(
                "心跳：近 {} 分钟推送成功 {} 次、失败 {} 次；最近 payload {} 字节，leads {}，agents {}，events {}；扫描 {} 次平均 {:.1}ms；跳过文件 {}",
                beat_at.elapsed().as_secs() / 60,
                stats.ok,
                stats.fail,
                sizes.bytes,
                sizes.leads,
                sizes.agents,
                sizes.events,
                stats.scans,
                avg,
                skipped
            );
            stats = Stats::default();
            beat_at = Instant::now();
        }
        nap(Duration::from_secs_f64(backoff(interval, fails)));
    }
    info!("收到退出信号，退出");
}

fn main() {
    init_log_early();
    install_panic_hook();
    install_signal_handlers();
    let o = parse_args();
    let raw_name = o.name.clone().unwrap_or_else(hostname);
    let device = clean_device(&raw_name).unwrap_or_else(|e| die(&e));
    let registry_ttl = parse_f64(
        "DECK_REGISTRY_TTL_HOURS",
        &std::env::var("DECK_REGISTRY_TTL_HOURS").unwrap_or_else(|_| "12".into()),
    ) * 3600.0;
    let conf = Config { pid_check: env_flag("DECK_PID_CHECK", true), registry_ttl, host: device.clone(), redact: o.redact };
    let mut scanner = Scanner::new(conf);

    if o.once {
        let t0 = Instant::now();
        let st = scanner.scan(&o.sources, o.hours);
        info!(
            "单次扫描完成：设备 {device}，leads {}，events {}，耗时 {:.1}ms，跳过文件 {}",
            st.leads.len(),
            st.events.len(),
            t0.elapsed().as_secs_f64() * 1000.0,
            scan::SKIPPED.load(Ordering::Relaxed)
        );
        let mut out = serde_json::to_vec(&st).unwrap_or_default();
        out.push(b'\n');
        let _ = std::io::stdout().write_all(&out);
        return;
    }
    let Some(url) = o.push_url.clone() else { die("推送需要 --push-url 或 DECK_PUSH_URL（仅扫描请用 --once）") };
    push_loop(scanner, &url, &device, &o.sources, o.hours, o.interval);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_name() {
        assert_eq!(clean_device("mac\u{202E}mini\u{200B}").as_deref(), Ok("macmini"));
        assert!(clean_device("\u{200B}\x01").is_err());
        assert!(clean_device(&"a".repeat(41)).is_err());
        assert_eq!(clean_device("设备一").as_deref(), Ok("设备一"));
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn backoff_and_ids() {
        assert_eq!(backoff(3.0, 0), 3.0);
        let b1 = backoff(3.0, 1);
        assert!((6.0..=7.2).contains(&b1));
        let b9 = backoff(3.0, 9);
        assert!((300.0..=360.0).contains(&b9));
        assert!(valid_src_id("cli") && valid_src_id("my_src-2"));
        assert!(!valid_src_id("CLI") && !valid_src_id("") && !valid_src_id(&"a".repeat(25)));
    }
}
