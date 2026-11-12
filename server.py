#!/usr/bin/env python3
"""Agent Deck — 本机 Claude Code 各档位 agent 值班面板。

只读扫描各档位配置目录（~/.claude、~/.claude-deepseek、~/.claude-qwen …）里的
sessions/<pid>.json（实时 busy/idle）与 projects/**.jsonl（会话与子 agent 记录），
汇总成 /api/state 给前端轮询。零依赖，仅用 Python 标准库。

用法：python3 server.py [--port 7788] [--hours 12]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HOME = Path.home()
HERE = Path(__file__).resolve().parent

# 档位定义：id、显示名、配置目录、入口过滤（None = 不过滤）、model_label（可选：该档位统一显示的模型名）
DEFAULT_SOURCES = [
    {"id": "app", "label": "Claude App", "dir": "~/.claude", "entry": "claude-desktop"},
    {"id": "cli", "label": "claude", "dir": "~/.claude", "entry": "!claude-desktop"},
    {"id": "ds", "label": "claude-ds", "dir": "~/.claude-deepseek", "entry": None, "model_label": "DeepSeek"},
    {"id": "qwen", "label": "claude-qwen", "dir": "~/.claude-qwen", "entry": None, "model_label": "Qwen4"},
]

LIVE_SECS = 90  # 记录文件在这段时间内有写入，视为仍在工作
HEAD_BYTES = 96 * 1024
TAIL_BYTES = 256 * 1024

NAMES = (
    "Ada Alma Amber Arlo Basil Bea Bram Bruno Cass Cleo Clyde Cora Dax Dora Eli Elsa Emmet Esme "
    "Ezra Faye Finn Flora Freya Gus Greta Hazel Hugo Ida Ines Ivan Ivy Jade Juno Kai Kit Lars Leo "
    "Levi Lina Lou Mabel Mae Milo Mira Nadia Nell Nico Noor Olive Omar Otto Pearl Pip Quinn Rex "
    "Rhea Rory Ruby Sage Sol Suki Tess Theo Toby Uma Vera Wren Yara Zane Zora"
).split()
_ROSTER = HERE / "avatars" / "roster.json"
_LEAD_FALLBACK = ("Atlas Beacon Comet Delta Echo Falcon Gemini Halo Ion Jet Kestrel Lumen Nova Orion Pulsar Quasar Rook "
              "Sirius Tango Vega Aurora Boreal Cobalt Dune Ember Fjord Granite Harbor Indigo Juniper Kite Lynx "
              "Meteor Nimbus Onyx Pike Raven Summit Tundra Umber Vortex Willow Zenith").split()
LEAD_NAMES = _LEAD_FALLBACK
try:  # 有角色表时，名字池 = 角色表（名字与头像一一对应）
    _r = json.loads(_ROSTER.read_text())
    NAMES = [p["name"] for p in _r["crew"]] or NAMES
    LEAD_NAMES = [p["name"] for p in _r["captains"]] or LEAD_NAMES
except Exception:
    pass


def avatar_names() -> list[str]:
    d = HERE / "avatars"
    return sorted(p.stem for p in d.glob("*.png")) if d.is_dir() else []


def pick(seq, key: str) -> str:
    h = int(hashlib.sha1(key.encode()).hexdigest()[:8], 16)
    return seq[h % len(seq)]


def iso_ms(ts: str | None) -> int | None:
    if not ts:
        return None
    try:
        from datetime import datetime

        return int(datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() * 1000)
    except Exception:
        return None


def read_head_tail(path: Path, size: int) -> tuple[list[dict], list[dict]]:
    """读文件头尾若干行 JSON（大文件不全读）。"""
    with path.open("rb") as f:
        head = f.read(HEAD_BYTES)
        if size > HEAD_BYTES + TAIL_BYTES:
            f.seek(size - TAIL_BYTES)
            tail = f.read()
            tail = tail[tail.find(b"\n") + 1 :]
        elif size > HEAD_BYTES:
            tail = head + f.read()
            head = tail
        else:
            tail = head

    def parse(buf: bytes, drop_last_partial: bool) -> list[dict]:
        lines = buf.split(b"\n")
        if drop_last_partial and not buf.endswith(b"\n"):
            lines = lines[:-1]
        out = []
        for ln in lines:
            if not ln.strip():
                continue
            try:
                o = json.loads(ln)
            except Exception:
                continue
            if isinstance(o, dict):
                out.append(o)
        return out

    return parse(head, size > HEAD_BYTES), parse(tail, False)


MODEL_RE = re.compile(rb'"model":"([^"]+)"')


class FileCache:
    """按 (mtime, size) 缓存单个 jsonl 的解析摘要。"""

    def __init__(self):
        self.d: dict[str, tuple[tuple, dict]] = {}
        self.seen: set[str] = set()

    def get(self, path: Path, st: os.stat_result, fn):
        """解析失败（文件中途消失、权限不足等）返回 None，由调用方跳过。"""
        key = str(path)
        self.seen.add(key)
        sig = (st.st_mtime_ns, st.st_size)
        hit = self.d.get(key)
        if hit and hit[0] == sig:
            return hit[1]
        # 文件只会追加写：把上一次的摘要交给解析函数，尾窗里找不到回复时可以沿用
        prev = hit[1] if hit and st.st_size >= hit[0][1] else None
        try:
            val = fn(path, st, prev)
        except Exception:  # noqa: BLE001
            return None
        self.d[key] = (sig, val)
        return val

    def prune(self):
        """删除本轮没访问到的项，避免常驻进程内存缓慢增长。"""
        for k in list(self.d):
            if k not in self.seen:
                del self.d[k]
        self.seen = set()


CACHE = FileCache()


def models_in(lines: list[dict]) -> list[str]:
    out = []
    for o in lines:
        if o.get("type") == "assistant":
            m = (o.get("message") or {}).get("model")
            if m and m != "<synthetic>":
                out.append(m)
    return out


def last_assistant_stop(lines: list[dict]):
    """最后一条 assistant 的结束方式；子 agent 以 SubagentHandback 工具交回结果视为正常完成。"""
    for o in reversed(lines):
        if o.get("type") == "assistant":
            m = o.get("message") or {}
            for c in m.get("content") or []:
                if isinstance(c, dict) and c.get("type") == "tool_use" and c.get("name") in ("SubagentHandback", "StructuredOutput"):
                    return "end_turn"
            return m.get("stop_reason")
    return None


def last_reply(lines: list[dict]):
    """最后一条回复：子 agent 交回的结果（SubagentHandback/StructuredOutput 的 message），否则最后一段文字。"""
    for o in reversed(lines):
        if o.get("type") != "assistant":
            continue
        content = (o.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        text = None
        for c in content:
            if not isinstance(c, dict):
                continue
            if c.get("type") == "tool_use" and c.get("name") in ("SubagentHandback", "StructuredOutput"):
                m = c.get("input", {}).get("message") if isinstance(c.get("input"), dict) else None
                if isinstance(m, str) and m.strip():
                    text = m
        if text is None:
            for c in reversed(content):
                if isinstance(c, dict) and c.get("type") == "text" and isinstance(c.get("text"), str) and c["text"].strip():
                    text = c["text"]
                    break
        if text:
            return " ".join(text.split())[:400]
    return None


LOOKBACK_BYTES = 4 * 1024 * 1024
TOOL_REPORT = ("SubagentHandback", "StructuredOutput")


def last_tool(lines: list[dict]):
    """最后一条 assistant 消息若以工具调用结尾，返回工具名（交回结果的工具不算）。"""
    for o in reversed(lines):
        if o.get("type") != "assistant":
            continue
        content = (o.get("message") or {}).get("content")
        if isinstance(content, list) and content and isinstance(content[-1], dict) and content[-1].get("type") == "tool_use":
            name = content[-1].get("name")
            if isinstance(name, str) and name not in TOOL_REPORT:
                return name[:40]
        return None
    return None


def reply_with_lookback(path: Path, size: int, tail: list[dict], prev):
    """回复只看文字。尾窗里没有时：追加写的文件沿用上一次的回复；首次见到则往前最多回看 4MiB。"""
    r = last_reply(tail)
    if r or size <= HEAD_BYTES + TAIL_BYTES:
        return r
    if prev is not None:
        return prev.get("reply")
    end = size - TAIL_BYTES
    start = max(0, end - LOOKBACK_BYTES)
    with path.open("rb") as f:
        f.seek(start)
        buf = f.read(end - start)
    lines = buf.split(b"\n")
    if start > 0:
        lines = lines[1:]  # 丢掉开头的半行
    parsed = []
    for ln in lines:
        try:
            o = json.loads(ln)
        except Exception:
            continue
        if isinstance(o, dict):
            parsed.append(o)
    return last_reply(parsed)


def summarize_session(path: Path, st: os.stat_result, prev=None) -> dict:
    head, tail = read_head_tail(path, st.st_size)
    first_ts = next((iso_ms(o.get("timestamp")) for o in head if o.get("timestamp")), None)
    last_ts = next((iso_ms(o.get("timestamp")) for o in reversed(tail) if o.get("timestamp")), None)
    entry = next((o.get("entrypoint") for o in head + tail if o.get("entrypoint")), None)
    cwd = next((o.get("cwd") for o in reversed(tail) if o.get("cwd")), None) or next(
        (o.get("cwd") for o in head if o.get("cwd")), None
    )
    title = None
    for o in reversed(tail):
        if o.get("type") == "custom-title" and o.get("customTitle"):
            title = o["customTitle"]
            break
        if o.get("type") == "agent-name" and o.get("agentName") and not title:
            title = o["agentName"]
    ms = models_in(tail) or models_in(head)
    branch = next((o.get("gitBranch") for o in reversed(tail) if o.get("gitBranch")), None)
    return {
        "start": first_ts,
        "last": last_ts,
        "entry": entry,
        "cwd": cwd,
        "title": title,
        "model": ms[-1] if ms else None,
        "branch": branch,
        "stop": last_assistant_stop(tail),
        "reply": reply_with_lookback(path, st.st_size, tail, prev),
        "tool": last_tool(tail),
    }


def summarize_sub(path: Path, st: os.stat_result, prev=None) -> dict:
    head, tail = read_head_tail(path, st.st_size)
    first_ts = next((iso_ms(o.get("timestamp")) for o in head if o.get("timestamp")), None)
    last_ts = next((iso_ms(o.get("timestamp")) for o in reversed(tail) if o.get("timestamp")), None)
    ms = models_in(tail) or models_in(head)
    meta = {}
    mp = path.with_name(path.name[: -len(".jsonl")] + ".meta.json")
    try:
        meta = json.loads(mp.read_text())
    except Exception:
        pass
    return {
        "start": first_ts,
        "last": last_ts,
        "model": ms[-1] if ms else meta.get("pickedEffortModel"),
        "type": meta.get("agentType") or "agent",
        "desc": meta.get("description"),
        "bg": meta.get("requestShape") == "background",
        "stop": last_assistant_stop(tail),
        "msgs": sum(1 for o in tail if o.get("type") == "assistant"),
        "reply": reply_with_lookback(path, st.st_size, tail, prev),
        "tool": last_tool(tail),
    }


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def env_flag(name: str, default: bool) -> bool:
    v = os.environ.get(name)
    return default if v is None else v.strip().lower() not in ("0", "false", "no", "off", "")


# 容器里看不到宿主机进程，需关掉 PID 检查，改用登记文件新鲜度判断会话是否还在
PID_CHECK = env_flag("DECK_PID_CHECK", True)
REGISTRY_TTL = float(os.environ.get("DECK_REGISTRY_TTL_HOURS", "12")) * 3600


def load_registry(cfg: Path) -> dict[str, dict]:
    reg = {}
    d = cfg / "sessions"
    if not d.is_dir():
        return reg
    for f in d.glob("*.json"):
        try:
            r = json.loads(f.read_text())
        except Exception:
            continue
        if not isinstance(r, dict):
            continue
        sid = r.get("sessionId")
        if not isinstance(sid, str) or not sid or not isinstance(r.get("pid"), int) or isinstance(r.get("pid"), bool) or r["pid"] <= 0:
            continue
        if PID_CHECK:
            if not pid_alive(r["pid"]):
                continue
        else:
            try:
                upd = r.get("updatedAt")
                fresh = max(f.stat().st_mtime, upd / 1000 if isinstance(upd, (int, float)) else 0)
            except OSError:
                continue
            if time.time() - fresh > REGISTRY_TTL:
                continue
        reg[sid] = r
    return reg


# Claude App 的会话元数据目录：其中 isArchived=true 的会话不再显示；DECK_APP_SESSIONS_DIR 设为空串可关闭
_APP_DIR = os.environ.get("DECK_APP_SESSIONS_DIR", "~/Library/Application Support/Claude/claude-code-sessions").strip()
APP_SESSIONS = Path(os.path.expanduser(_APP_DIR)) if _APP_DIR else None
_APP_CACHE: dict[Path, tuple[tuple, bool, list[str]]] = {}


def archived_ids() -> set[str]:
    """在 Claude App 里已归档的 CLI 会话 id（同一 id 也出现在未归档会话里时不算）。按 mtime/size 缓存。"""
    if not APP_SESSIONS or not APP_SESSIONS.is_dir():
        return set()
    out, keep, seen = set(), set(), set()
    for depth in ("local_*.json", "*/local_*.json", "*/*/local_*.json", "*/*/*/local_*.json"):
        try:
            files = list(APP_SESSIONS.glob(depth))
        except OSError:
            continue
        for f in files:
            try:
                st = f.stat()
            except OSError:
                continue
            sig = (st.st_mtime_ns, st.st_size)
            seen.add(f)
            c = _APP_CACHE.get(f)
            if not c or c[0] != sig:
                try:
                    d = json.loads(f.read_bytes())
                except Exception:
                    continue
                if not isinstance(d, dict):
                    continue
                ids = [d.get("cliSessionId"), d.get("lastAssistantCliSessionId")]
                prior = d.get("priorCliSessionIds")
                ids += prior if isinstance(prior, list) else []
                c = _APP_CACHE[f] = (sig, d.get("isArchived") is True, [x for x in ids if isinstance(x, str) and x])
            (out if c[1] else keep).update(c[2])
    for f in list(_APP_CACHE):
        if f not in seen:
            del _APP_CACHE[f]
    return out - keep


def project_of(cwd: str) -> str:
    """项目名：cwd 最后一段目录；工作树（<仓库>/.claude/worktrees/<名字>[/...]）归到所属仓库名，与 Claude App 侧边栏一致。"""
    i = cwd.find("/.claude/worktrees/")
    if i > 0 and Path(cwd[:i]).name:
        return Path(cwd[:i]).name
    return Path(cwd).name


def entry_match(rule, entry) -> bool:
    if rule is None:
        return True
    if rule.startswith("!"):
        return entry != rule[1:]
    return entry == rule


DEVICE_NAME = os.environ.get("DECK_NAME") or os.uname().nodename.split(".")[0]


def scan(sources: list[dict], hours: float) -> dict:
    now = int(time.time() * 1000)
    horizon = time.time() - hours * 3600
    leads, events = [], []
    src_stats = {}
    archived = archived_ids()

    by_dir: dict[str, list[dict]] = {}
    for s in sources:
        by_dir.setdefault(s["dir"], []).append(s)

    for dir_, srcs in by_dir.items():
        cfg = Path(os.path.expanduser(dir_))
        reg = load_registry(cfg)
        proj = cfg / "projects"
        for s in srcs:
            src_stats[s["id"]] = {"id": s["id"], "label": s["label"], "dir": dir_, "online": 0, "working": 0, "sessions": 0, "agents": 0}
        if not proj.is_dir():
            continue
        try:
            pdirs = [d for d in proj.iterdir() if d.is_dir()]
        except OSError:
            continue
        for pdir in pdirs:
            try:
                jfiles = list(pdir.glob("*.jsonl"))
            except OSError:
                continue
            for jf in jfiles:
                try:
                    st = jf.stat()
                except OSError:
                    continue
                sid = jf.stem
                if sid in archived:
                    continue  # 已在 Claude App 里归档：不再显示
                subdir = pdir / sid / "subagents"
                sub_files = []
                try:
                    sub_list = list(subdir.glob("agent-*.jsonl")) if subdir.is_dir() else []
                except OSError:
                    sub_list = []
                if sub_list:
                    for sf in sub_list:
                        try:
                            sst = sf.stat()
                        except OSError:
                            continue
                        if sst.st_mtime >= horizon:
                            sub_files.append((sf, sst))
                r = reg.get(sid)
                if st.st_mtime < horizon and not sub_files and not r:
                    continue
                info = CACHE.get(jf, st, summarize_session)
                if info is None:
                    continue
                entry = (r or {}).get("entrypoint") or info["entry"]
                src = next((s for s in srcs if entry_match(s["entry"], entry)), None)
                if not src:
                    continue

                age = time.time() - st.st_mtime
                if r:
                    status = "working" if r.get("status") == "busy" else "idle"
                elif age < LIVE_SECS:
                    status = "working"
                else:
                    status = "done" if info["stop"] in ("end_turn", "stop_sequence", None) else "stopped"

                cwd = (r or {}).get("cwd") or info["cwd"] or ""
                project = project_of(cwd) if cwd else pdir.name
                if "scratch-workspaces" in cwd:
                    project = "scratch"
                title = (r or {}).get("name") or info["title"] or project
                lead = {
                    "id": sid,
                    "src": src["id"],
                    "name": pick(LEAD_NAMES, src["id"] + sid),
                    "title": title,
                    "project": project,
                    "branch": info["branch"],
                    "model": src.get("model_label") or info["model"],
                    "pid": (r or {}).get("pid"),
                    "status": status,
                    "start": info["start"] or int(st.st_mtime * 1000),
                    "last": max(int(st.st_mtime * 1000), (r or {}).get("updatedAt") or 0),
                    "reply": info.get("reply"),
                    "tool": info.get("tool"),
                    "agents": [],
                }
                for sf, sst in sub_files:
                    si = CACHE.get(sf, sst, summarize_sub)
                    if si is None:
                        continue
                    aid = sf.stem.replace("agent-", "")
                    sage = time.time() - sst.st_mtime
                    if sage < LIVE_SECS:
                        sstatus = "working"
                    elif si["stop"] in ("end_turn", "stop_sequence"):
                        sstatus = "finished"
                    elif si["msgs"] == 0:
                        sstatus = "failed"
                    else:
                        sstatus = "stopped"
                    if sstatus == "working" and lead["status"] in ("idle", "done", "stopped"):
                        lead["status"] = "working"
                    a = {
                        "id": aid,
                        "name": pick(NAMES, aid),
                        "type": si["type"],
                        "task": si["desc"],
                        "model": src.get("model_label") or si["model"],
                        "bg": si["bg"],
                        "status": sstatus,
                        "start": si["start"] or int(sst.st_mtime * 1000),
                        "last": int(sst.st_mtime * 1000),
                        "reply": si.get("reply"),
                        "tool": si.get("tool"),
                    }
                    lead["agents"].append(a)
                    base = {"src": src["id"], "lid": sid, "aid": aid, "who": a["name"], "job": a["type"], "task": a["task"]}
                    events.append({**base, "at": a["start"], "kind": "on"})
                    if sstatus != "working":
                        events.append({**base, "at": a["last"], "kind": sstatus})
                lead["agents"].sort(key=lambda a: (a["status"] != "working", -a["last"]))
                lb = {"src": src["id"], "lid": sid, "aid": None, "who": None, "job": "lead", "task": title}
                events.append({**lb, "at": lead["start"], "kind": "on"})
                if lead["status"] in ("done", "stopped"):
                    events.append({**lb, "at": lead["last"], "kind": lead["status"]})
                leads.append(lead)

                ss = src_stats[src["id"]]
                ss["sessions"] += 1
                ss["agents"] += len(lead["agents"])
                if lead["status"] in ("working", "idle"):
                    ss["online"] += 1
                if lead["status"] == "working" or any(x["status"] == "working" for x in lead["agents"]):
                    ss["working"] += 1

    # 主会话代号去重：按开始时间先到先得，撞名则顺延
    used: set[str] = set()
    for l in sorted(leads, key=lambda l: l["start"]):
        base = l["name"]
        i = LEAD_NAMES.index(base) if base in LEAD_NAMES else 0
        n = 0
        l["face"] = base
        while l["name"] in used and n < len(LEAD_NAMES):
            n += 1
            l["name"] = l["face"] = LEAD_NAMES[(i + n) % len(LEAD_NAMES)]
        if l["name"] in used:  # 名字池用完：同脸加序号
            k = 2
            while f"{base} {k}" in used:
                k += 1
            l["name"], l["face"] = f"{base} {k}", base
        used.add(l["name"])
    names = {l["id"]: l["name"] for l in leads}
    for e in events:
        e["lead"] = names.get(e["lid"])
        if e["who"] is None:
            e["who"] = e["lead"]
    CACHE.prune()
    order = {"working": 0, "idle": 1, "stopped": 2, "done": 3}
    leads.sort(key=lambda l: (order.get(l["status"], 9), -l["last"]))
    events.sort(key=lambda e: -e["at"])
    return {
        "now": now,
        "hours": hours,
        "host": DEVICE_NAME,
        "sources": list(src_stats.values()),
        "leads": leads,
        "events": events[:400],
    }


# ---------------------------------------------------------------- 任务类型分类（按标题，交给大模型）
# 只把标题文本发给 OpenAI 兼容接口，要求返回结构化 JSON；结果按标题缓存在内存里。
# 未配置 DECK_LLM_URL / DECK_LLM_KEY、接口出错或尚未分类时，kind 为 null，前端退回本地规则。
TASK_KINDS = {
    "ci": "持续集成：跑/修 CI、流水线、构建、自动化测试任务",
    "ops": "运维部署：发布、部署、上线、服务器、容器、监控、数据库运维",
    "review": "评审核对：代码评审、审查、复核、验收、对抗性检查",
    "plan": "方案讨论：架构/方案设计、规划、需求取舍、咨询决策",
    "research": "调研探索：查资料、联网调研、阅读/探索代码、定位问题",
    "code": "编码实现：写代码、修 bug、重构、补测试",
    "docs": "文档写作：README、CHANGELOG、说明、翻译润色",
    "other": "以上都不是",
}


class TaskClassifier:
    BATCH, CAP = 20, 4000

    def __init__(self):
        env = os.environ.get
        self.url = (env("DECK_LLM_URL") or "").rstrip("/")
        self.key = env("DECK_LLM_KEY") or ""
        self.model = env("DECK_LLM_MODEL") or "deepseek"
        self.effort = env("DECK_LLM_EFFORT") or "low"
        self.on = bool(self.url and self.key)
        self.lock = threading.Lock()
        self.cache: dict[str, str] = {}
        self.pending: dict[str, None] = {}  # 有序去重队列
        self.miss: dict[str, int] = {}      # 标题被模型漏掉的次数
        self.wake = threading.Event()
        self.err = None
        if self.on:
            threading.Thread(target=self._loop, daemon=True, name="task-classifier").start()

    def kind(self, title):
        """取分类结果；没有就排队，本次返回 None。"""
        if not self.on or not isinstance(title, str) or not title.strip():
            return None
        t = title.strip()[:160]
        with self.lock:
            k = self.cache.get(t)
            if k is None and t not in self.pending:
                self.pending[t] = None
                self.wake.set()
        return k

    def annotate(self, leads):
        """给主 agent（会话标题）和子 agent（子任务标题）各自标上 kind，返回新列表，不改原数据。"""
        return [{**l, "kind": self.kind(l.get("title")),
                 "agents": [{**a, "kind": self.kind(a.get("task"))} for a in l.get("agents", [])]} for l in leads]

    def _loop(self):
        backoff = 0.0
        while True:
            self.wake.wait(timeout=30)
            self.wake.clear()
            with self.lock:
                batch = list(self.pending)[: self.BATCH]
            if not batch:
                continue
            try:
                got = self._ask(batch)
                backoff = 0.0
                if self.err:
                    print("任务分类恢复正常", flush=True)
                self.err = None
            except Exception as e:  # 网络/鉴权/格式错误：保留队列，退避后重试
                msg = f"{type(e).__name__}: {e}"[:200]
                if msg != self.err:
                    print(f"任务分类失败（{msg}），稍后重试", flush=True)
                self.err = msg
                backoff = min(300.0, backoff * 2 or 15.0)
                time.sleep(backoff)
                self.wake.set()
                continue
            with self.lock:
                for t in batch:
                    if t in got:
                        self.pending.pop(t, None)
                        self.miss.pop(t, None)
                        self.cache[t] = got[t]
                    elif self.miss.get(t, 0) + 1 >= self.TRIES:  # 模型屡次漏掉这条：归为 other，不再排队
                        self.pending.pop(t, None)
                        self.miss.pop(t, None)
                        self.cache[t] = "other"
                    else:  # 漏掉的移到队尾，下一批再问
                        self.miss[t] = self.miss.get(t, 0) + 1
                        self.pending.pop(t, None)
                        self.pending[t] = None
                while len(self.cache) > self.CAP:
                    self.cache.pop(next(iter(self.cache)))
                if self.pending:
                    self.wake.set()

    SYSTEM_PROMPT = """你是任务分类器。用户会给出若干行任务标题，每行格式为「序号. 标题」。
请只依据标题文字判断每个任务属于哪一类，不要猜测标题以外的信息，并用 JSON 格式输出。
可选类别（kind 只能取下面的英文值）：
{kinds}

EXAMPLE INPUT:
0. 修复 GitHub CI 缓存失效
1. 调研上游 3D API 的质量参数
2. 实现 Blender 实时建模工具集
3. 部署 v1.4.0 到生产并验收

EXAMPLE JSON OUTPUT:
{{
    "items": [
        {{"i": 0, "kind": "ci"}},
        {{"i": 1, "kind": "research"}},
        {{"i": 2, "kind": "code"}},
        {{"i": 3, "kind": "ops"}}
    ]
}}

每个标题输出一项，i 与输入序号一致；只输出这个 JSON 对象，不要输出其他文字。"""
    TRIES = 3  # 接口返回空内容或 JSON 解析失败时，立即重试的次数

    @staticmethod
    def extract_json(text: str):
        """从模型输出里取出 JSON 对象：先整段解析，再去掉 ``` 代码块，最后逐个尝试从每个 { 起解析出第一个完整对象。"""
        text = (text or "").strip()
        if not text:
            raise ValueError("模型返回了空内容")
        cands = [text]
        fence = re.search(r"```(?:json)?\s*(.*?)```", text, re.S | re.I)
        if fence:
            cands.append(fence.group(1).strip())
        for c in cands:
            try:
                v = json.loads(c)
                if isinstance(v, dict):
                    return v
            except ValueError:
                pass
        dec = json.JSONDecoder()
        for m in re.finditer(r"\{", text):
            try:
                v, _ = dec.raw_decode(text, m.start())
                if isinstance(v, dict):
                    return v
            except ValueError:
                continue
        raise ValueError("模型输出里找不到合法的 JSON 对象")

    def parse_items(self, obj: dict, titles: list[str]) -> dict[str, str]:
        """校验结构：items 是列表，每项 i 为有效序号、kind 为已知类别（未知类别归 other）。"""
        items = obj.get("items") if isinstance(obj, dict) else None
        if not isinstance(items, list):
            raise ValueError("JSON 里缺少 items 列表")
        out = {}
        for it in items:
            if not isinstance(it, dict):
                continue
            i = it.get("i")
            if isinstance(i, str) and i.strip().isdigit():
                i = int(i)
            if isinstance(i, int) and not isinstance(i, bool) and 0 <= i < len(titles):
                k = str(it.get("kind") or "").strip().lower()
                out[titles[i]] = k if k in TASK_KINDS else "other"
        if not out:
            raise ValueError("items 里没有可用的分类结果")
        return out

    def _ask(self, titles):
        import urllib.request
        kinds = "\n".join(f"- {k}：{v}" for k, v in TASK_KINDS.items())
        user_p = "\n".join(f"{i}. {t}" for i, t in enumerate(titles))
        # 思考过程也占 max_tokens，预留充足
        body = {"model": self.model, "temperature": 0, "max_tokens": 1200 + 30 * len(titles),
                "response_format": {"type": "json_object"},
                "messages": [{"role": "system", "content": self.SYSTEM_PROMPT.format(kinds=kinds)},
                             {"role": "user", "content": user_p}]}
        if self.effort and self.effort != "off":
            body["reasoning_effort"] = self.effort
        data = json.dumps(body).encode()
        last = None
        for _ in range(self.TRIES):
            req = urllib.request.Request(self.url + "/chat/completions", data=data,
                                         headers={"Content-Type": "application/json", "Authorization": "Bearer " + self.key})
            with urllib.request.urlopen(req, timeout=60) as r:  # 网络 / HTTP 错误直接抛出，由 _loop 退避重试
                resp = json.loads(r.read())
            try:
                text = resp["choices"][0]["message"].get("content")
                got = self.parse_items(self.extract_json(text), titles)
            except (ValueError, KeyError, IndexError, TypeError, AttributeError) as e:
                last = e  # 空内容 / JSON 解析失败 / 结构不对：立即重试
                continue
            # 漏掉的标题本轮不写缓存，留在队列里下次再分
            return got
        raise ValueError(f"连续 {self.TRIES} 次没拿到合法 JSON：{last}")


CLASSIFIER = TaskClassifier()


class State:
    """本地模式：直接扫描本机文件（2 秒缓存）。"""

    def __init__(self, sources, hours):
        self.sources, self.hours = sources, hours
        self.lock = threading.Lock()
        self.data, self.at = None, 0.0

    def get(self):
        with self.lock:
            if self.data is None or time.time() - self.at > 2.0:
                self.data = scan(self.sources, self.hours)
                self.data["leads"] = CLASSIFIER.annotate(self.data["leads"])
                self.data["avatars"] = avatar_names()
                self.at = time.time()
            return self.data


# ---------------------------------------------------------------- hub 模式
# 推送接口不设鉴权（按需求公开），因此对写入数据做严格白名单清洗，防止脏数据或注入内容进到页面。
import ipaddress

# 允许写入 /api/ingest 的来源网段（逗号分隔 CIDR）。直接暴露端口、不经 nginx 时靠它挡住非内网写入。
INGEST_ALLOW = [ipaddress.ip_network(x.strip(), strict=False)
                for x in os.environ.get("DECK_INGEST_ALLOW", "127.0.0.0/8,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16").split(",")
                if x.strip()]


def ingest_allowed(addr: str) -> bool:
    try:
        ip = ipaddress.ip_address(addr)
    except ValueError:
        return False
    if ip.version == 6 and ip.ipv4_mapped:
        ip = ip.ipv4_mapped
    return any(ip in n for n in INGEST_ALLOW)


MAX_INGEST = 1024 * 1024          # 与 nginx 的 client_max_body_size 1m 一致
MAX_DEVICES = 8
MAX_AGENTS_PER_DEVICE = 2000
MAX_SKEW_MS = 2000                 # 设备时钟偏差超过这个值就校正到 hub 时间
LEAD_STATUS = {"working", "idle", "done", "stopped"}
AGENT_STATUS = {"working", "finished", "stopped", "failed"}
EVENT_KINDS = {"on", "finished", "done", "stopped", "failed"}
SRC_ID_RE = re.compile(r"^[a-z0-9_-]{1,24}$")


_CTRL = {chr(c) for c in range(32)} | {"\x7f"}


def clean_text(v: str) -> str:
    """去掉控制字符与 Unicode 格式字符（如 U+202E 方向覆盖、零宽字符），防止在公开页面上伪装显示。"""
    import unicodedata
    return "".join(ch for ch in v if ch not in _CTRL and unicodedata.category(ch) != "Cf")


def _s(v, n=160):
    return clean_text(v)[:n] if isinstance(v, str) else None


def _list(v, n):
    return v[:n] if isinstance(v, list) else []


def _i(v):
    return int(v) if isinstance(v, (int, float)) and not isinstance(v, bool) and 0 <= v < 1e14 else None


def _id(v):
    return re.sub(r"[^A-Za-z0-9_-]", "", v)[:64] if isinstance(v, str) else ""


def sanitize(state: dict) -> dict:
    if not isinstance(state, dict):
        raise ValueError("state 必须是对象")
    srcs = []
    for x in _list(state.get("sources"), 16):
        if isinstance(x, dict) and isinstance(x.get("id"), str) and SRC_ID_RE.match(x["id"]):
            srcs.append({"id": x["id"], "label": _s(x.get("label"), 40) or x["id"],
                         **{k: _i(x.get(k)) or 0 for k in ("online", "working", "sessions", "agents")}})
    ids = {x["id"] for x in srcs}
    leads = []
    budget = MAX_AGENTS_PER_DEVICE
    for l in _list(state.get("leads"), 300):
        if not isinstance(l, dict) or l.get("src") not in ids or l.get("status") not in LEAD_STATUS:
            continue
        agents = []
        for a in _list(l.get("agents"), min(200, budget)):
            if isinstance(a, dict) and a.get("status") in AGENT_STATUS and _i(a.get("start")) and _i(a.get("last")):
                agents.append({"id": _id(a.get("id")), "name": _s(a.get("name"), 32) or "?", "type": _s(a.get("type"), 40) or "agent",
                               "task": _s(a.get("task")), "model": _s(a.get("model"), 60), "bg": bool(a.get("bg")),
                               "status": a["status"], "start": _i(a["start"]), "last": _i(a["last"]),
                               "reply": _s(a.get("reply"), 400), "tool": _s(a.get("tool"), 40)})
        if not (_i(l.get("start")) and _i(l.get("last"))):
            continue
        budget -= len(agents)
        leads.append({"id": _id(l.get("id")), "src": l["src"], "name": _s(l.get("name"), 32) or "?", "face": _s(l.get("face"), 32),
                      "title": _s(l.get("title")), "project": _s(l.get("project"), 80), "branch": _s(l.get("branch"), 80),
                      "model": _s(l.get("model"), 60), "status": l["status"], "start": _i(l["start"]), "last": _i(l["last"]),
                      "reply": _s(l.get("reply"), 400), "tool": _s(l.get("tool"), 40),
                      "agents": agents})
    events = []
    for e in _list(state.get("events"), 400):
        if isinstance(e, dict) and e.get("src") in ids and e.get("kind") in EVENT_KINDS and _i(e.get("at")):
            events.append({"src": e["src"], "lid": _id(e.get("lid")), "aid": _id(e.get("aid")), "who": _s(e.get("who"), 32) or "?",
                           "lead": _s(e.get("lead"), 32), "job": _s(e.get("job"), 40) or "agent", "task": _s(e.get("task")),
                           "at": _i(e["at"]), "kind": e["kind"]})
    hours = state.get("hours")
    return {"now": _i(state.get("now")) or int(time.time() * 1000),
            "hours": float(hours) if isinstance(hours, (int, float)) and 0 < hours <= 168 else 12.0,
            "sources": srcs, "leads": leads, "events": events}


class Hub:
    """hub 模式：接收各设备推送的状态，合并后给前端。"""

    def __init__(self, name: str, ttl: float):
        self.name, self.ttl = name, ttl
        self.lock = threading.Lock()
        self.devices: dict[str, dict] = {}
        self.cache: tuple[float, bytes] | None = None

    def get_bytes(self) -> bytes:
        """序列化结果缓存 1 秒，避免高并发轮询时重复序列化。"""
        with self.lock:
            c = self.cache
        if c and time.time() - c[0] < 1.0:
            return c[1]
        body = json.dumps(self.get(), ensure_ascii=False).encode()
        with self.lock:
            self.cache = (time.time(), body)
        return body

    def ingest(self, device: str, state: dict):
        clean = sanitize(state)
        now_ms = int(time.time() * 1000)
        off = now_ms - clean["now"]
        if abs(off) > MAX_SKEW_MS:  # 设备时钟与 hub 不一致：把该设备所有时间戳平移到 hub 时间
            for l in clean["leads"]:
                l["start"] += off
                l["last"] += off
                for a in l["agents"]:
                    a["start"] += off
                    a["last"] += off
            for e in clean["events"]:
                e["at"] += off
        with self.lock:
            self.devices[device] = {"state": clean, "at": time.time(), "skew": off}
            while len(self.devices) > MAX_DEVICES:  # 防止被灌入大量设备名
                oldest = min(self.devices, key=lambda k: self.devices[k]["at"])
                self.devices.pop(oldest, None)
            self.cache = None

    def get(self):
        now = time.time()
        with self.lock:
            live = {k: v for k, v in self.devices.items() if now - v["at"] <= self.ttl}
        multi = len(live) > 1  # 只影响显示标签；id 一律带 @设备，保证前端显隐状态稳定
        out = {"now": int(now * 1000), "hours": 12.0, "sources": [], "leads": [], "events": [],
               "avatars": avatar_names(), "devices": [{"name": k, "age": round(now - v["at"], 1)} for k, v in sorted(live.items())]}
        for dev, v in sorted(live.items()):
            st = v["state"]
            out["hours"] = st["hours"]
            ren = lambda sid, dev=dev: f"{sid}@{dev}"  # noqa: E731
            for x in st["sources"]:
                out["sources"].append({**x, "id": ren(x["id"]), "label": f"{x['label']} · {dev}" if multi else x["label"]})
            out["leads"] += [{**l, "src": ren(l["src"])} for l in st["leads"]]
            out["events"] += [{**e, "src": ren(e["src"])} for e in st["events"]]
        order = {"working": 0, "idle": 1, "stopped": 2, "done": 3}
        out["leads"].sort(key=lambda l: (order.get(l["status"], 9), -l["last"]))
        out["events"].sort(key=lambda e: -e["at"])
        out["events"] = out["events"][:400]
        out["leads"] = CLASSIFIER.annotate(out["leads"])
        out["host"] = next(iter(live)) if len(live) == 1 else (self.name if not live else f"{self.name} · {len(live)} 台设备")
        return out


def make_handler(state, hub: "Hub | None" = None):
    class H(BaseHTTPRequestHandler):
        server_version = "AgentDeck"
        sys_version = ""
        timeout = 15  # 慢请求不无限占用线程

        def log_message(self, *a):
            pass

        def send(self, code, body: bytes, ctype: str, cache: str = "no-store"):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", cache)
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("X-Frame-Options", "DENY")
            if ctype.startswith("text/html"):
                self.send_header("Content-Security-Policy",
                                 "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; "
                                 "script-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def do_HEAD(self):
            self.do_GET()

        def jerr(self, code, msg):
            self.send(code, json.dumps({"error": msg}, ensure_ascii=False).encode(), "application/json; charset=utf-8")

        def do_GET(self):
            p = self.path.split("?")[0]
            if p == "/api/state":
                try:
                    body = state.get_bytes() if hasattr(state, "get_bytes") else json.dumps(state.get(), ensure_ascii=False).encode()
                    self.send(200, body, "application/json; charset=utf-8")
                except Exception:  # noqa: BLE001
                    self.jerr(500, "internal error")
            elif p == "/healthz":
                self.send(200, b"ok", "text/plain")
            elif re.fullmatch(r"/avatars/[a-z0-9_-]{1,40}\.png", p):
                f = HERE / "avatars" / p[9:]
                if f.is_file():
                    self.send(200, f.read_bytes(), "image/png", "public, max-age=86400")
                else:
                    self.send(404, b"not found", "text/plain")
            elif p in ("/", "/index.html", "/voxel", "/voxel.html"):  # /voxel：主页面的全屏办公室视图
                self.send(200, (HERE / "index.html").read_bytes(), "text/html; charset=utf-8")
            elif p in ("/classic", "/classic.html"):
                self.send(200, (HERE / "classic.html").read_bytes(), "text/html; charset=utf-8")
            elif p in ("/office", "/office.html"):
                self.send(200, (HERE / "office.html").read_bytes(), "text/html; charset=utf-8")
            elif p in ("/city", "/city.html"):
                self.send(200, (HERE / "city.html").read_bytes(), "text/html; charset=utf-8")
            else:
                self.send(404, b"not found", "text/plain")

        def do_POST(self):
            if self.path.split("?")[0] != "/api/ingest" or hub is None:
                return self.jerr(404, "not found")
            if not ingest_allowed(self.client_address[0]):
                return self.jerr(403, "forbidden")
            try:
                n = int(self.headers.get("Content-Length") or 0)
            except ValueError:
                n = -1
            if n <= 0 or n > MAX_INGEST:
                return self.jerr(413, "body too large or empty")
            try:
                d = json.loads(self.rfile.read(n))
                dev = d.get("device") if isinstance(d, dict) else None
                if not isinstance(dev, str) or clean_text(dev) != dev or not (1 <= len(dev) <= 40):
                    return self.jerr(400, "device 无效")
                hub.ingest(dev, d.get("state"))
            except Exception:  # noqa: BLE001  任何畸形输入都只返回 400，不打断连接
                return self.jerr(400, "bad payload")
            self.send(204, b"", "text/plain")

    return H


def main():
    # 每个参数都可用环境变量给缺省值（便于 docker compose 配置），命令行优先
    env = os.environ.get
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["local", "hub"], default=env("DECK_MODE", "local"),
                    help="local=扫描本机并提供页面；hub=只接收各设备推送并提供页面（推送端见 collector/，Rust 实现）")
    ap.add_argument("--port", type=int, default=int(env("DECK_PORT", "7788")))
    ap.add_argument("--host", default=env("DECK_HOST", "127.0.0.1"))
    ap.add_argument("--hours", type=float, default=float(env("DECK_HOURS", "12")), help="回看多少小时的会话")
    ap.add_argument("--sources", help="自定义档位 JSON 文件（格式同 DEFAULT_SOURCES）；也可用 DECK_SOURCES 直接给 JSON 字符串")
    ap.add_argument("--name", help="设备名（缺省读 DECK_NAME，再缺省用主机名）")
    ap.add_argument("--device-ttl", type=float, default=float(env("DECK_DEVICE_TTL", "90")), help="hub 模式：设备多久没推送就下线（秒）")
    a = ap.parse_args()
    global DEVICE_NAME
    if a.name:
        DEVICE_NAME = a.name
    if a.sources:
        sources = json.loads(Path(a.sources).read_text())
    elif env("DECK_SOURCES"):
        sources = json.loads(env("DECK_SOURCES"))
    else:
        sources = DEFAULT_SOURCES

    if a.mode == "hub":
        hub = Hub(DEVICE_NAME, a.device_ttl)
        handler = make_handler(hub, hub)
        desc = f"hub 模式，设备离线阈值 {a.device_ttl}s"
    else:
        handler = make_handler(State(sources, a.hours))
        desc = f"local 模式，回看 {a.hours}h，档位 {[s['label'] for s in sources]}，PID 检查 {'开' if PID_CHECK else '关'}"
    srv = ThreadingHTTPServer((a.host, a.port), handler)
    print(f"Agent Deck → http://{a.host}:{a.port}  ({desc})", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
