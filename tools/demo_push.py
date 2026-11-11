#!/usr/bin/env python3
"""向 hub 推送一份仿真演示数据（两台虚构设备），用于截图或演示，不读取任何真实会话。

用法：
  python3 server.py --mode hub --port 7799 &
  python3 tools/demo_push.py http://127.0.0.1:7799
"""
from __future__ import annotations

import json
import sys
import time
import urllib.request

MIN = 60_000


def lead(src, name, model, status, project, title, start_min, last_min, reply, agents):
    return {"id": f"demo-{name.lower()}", "src": src, "name": name, "face": name, "title": title, "project": project,
            "branch": None, "model": model, "status": status, "start": NOW - start_min * MIN,
            "last": NOW - last_min * MIN, "reply": reply, "agents": agents}


def agent(name, typ, model, status, task, start_min, last_min, reply=None, bg=False, tool=None):
    return {"id": f"demo-{name.lower()}-{typ}", "name": name, "type": typ, "task": task, "model": model, "bg": bg,
            "status": status, "start": NOW - start_min * MIN, "last": NOW - last_min * MIN, "reply": reply, "tool": tool}


def events(src, leads):
    out = []
    for l in leads:
        out.append({"src": src, "lid": l["id"], "aid": None, "who": l["name"], "lead": l["name"], "job": "lead",
                    "task": l["title"], "at": l["start"], "kind": "on"})
        if l["status"] in ("done", "stopped"):
            out.append({"src": src, "lid": l["id"], "aid": None, "who": l["name"], "lead": l["name"], "job": "lead",
                        "task": l["title"], "at": l["last"], "kind": l["status"]})
        for a in l["agents"]:
            base = {"src": src, "lid": l["id"], "aid": a["id"], "who": a["name"], "lead": l["name"], "job": a["type"], "task": a["task"]}
            out.append({**base, "at": a["start"], "kind": "on"})
            if a["status"] != "working":
                out.append({**base, "at": a["last"], "kind": a["status"]})
    return sorted(out, key=lambda e: -e["at"])


def state(sources, leads):
    for s in sources:
        mine = [l for l in leads if l["src"] == s["id"]]
        s.update(sessions=len(mine), online=sum(l["status"] in ("working", "idle") for l in mine),
                 working=sum(l["status"] == "working" or any(a["status"] == "working" for a in l["agents"]) for l in mine),
                 agents=sum(len(l["agents"]) for l in mine))
    return {"now": NOW, "hours": 12, "sources": sources, "leads": leads,
            "events": [e for s in sources for e in events(s["id"], [l for l in leads if l["src"] == s["id"]])]}


NOW = int(time.time() * 1000)
OPUS, DS, QW = "claude-opus-5-5", "DeepSeek", "Qwen4"

MAC = state(
    [{"id": "app", "label": "Claude App"}, {"id": "cli", "label": "claude"},
     {"id": "ds", "label": "claude-ds"}, {"id": "qwen", "label": "claude-qwen"}],
    [
        lead("app", "Aurora", OPUS, "working", "shop-api", "订单服务库存锁重构", 52, 0,
             "库存扣减改为乐观锁 + 重试，压测 P99 从 420ms 降到 95ms；正在等 Hazel 补完回滚路径的单测。", [
                 agent("Hazel", "implementer", OPUS, "working", "补齐库存回滚路径单测", 14, 0,
                       "已覆盖超卖、重复回调两种场景，还剩并发取消订单的用例。", tool="Edit"),
                 agent("Sage", "researcher", OPUS, "finished", "调研分布式锁方案取舍", 41, 33,
                       "结论：库存场景用数据库乐观锁足够，Redis 锁只在跨库编排时考虑。"),
                 agent("Nadia", "advisor", OPUS, "finished", "评审锁粒度与重试上限", 31, 26,
                       "建议按 SKU 粒度加锁，重试 3 次后转入异步补偿队列。"),
                 agent("Ada", "code-explorer", OPUS, "finished", "梳理库存相关调用链", 49, 44,
                       "共 7 个写入口，其中 2 个绕过了库存服务，需要一起收口。"),
             ]),
        lead("ds", "Nova", DS, "working", "docs-site", "文档站迁移到新主题", 38, 0,
             "新主题的导航和搜索已接好，正在批量修正旧文档里的相对链接。", [
                 agent("Esme", "docs-writer", DS, "working", "改写快速开始与安装章节", 9, 0,
                       "快速开始已缩到 5 步，正在补 Windows 安装说明。"),
                 agent("Juno", "helper", DS, "finished", "批量替换失效链接", 30, 22,
                       "替换 132 处相对链接，剩 4 处外链需要人工确认。"),
             ]),
        lead("qwen", "Vega", QW, "idle", "data-pipeline", "夜间任务超时排查", 70, 6,
             "超时来自上游分区表扫描，已加分区裁剪；等今晚跑批验证。", []),
        lead("app", "Ember", OPUS, "done", "mobile-app", "修复登录页闪退", 150, 104,
             "根因是弱网下 token 刷新与页面销毁竞争，已加取消逻辑并补了回归用例。", [
                 agent("Freya", "implementer", OPUS, "finished", "修复 token 刷新竞态", 140, 118,
                       "改为可取消的刷新任务，页面销毁时统一取消。"),
                 agent("Olive", "reviewer", OPUS, "finished", "审查修复补丁", 117, 110, "通过，补充了两处空值判断。"),
                 agent("Ida", "tester", OPUS, "stopped", "弱网回归测试", 112, 106),
             ]),
        lead("ds", "Juniper", DS, "done", "shop-api", "补充支付回调测试", 200, 171,
             "支付回调的幂等、验签失败、重复通知三类用例已补齐，覆盖率 71% → 88%。", [
                 agent("Cleo", "implementer", DS, "finished", "编写支付回调用例", 195, 175, "新增 18 个用例，全部通过。"),
             ]),
        lead("app", "Zenith", OPUS, "stopped", "shop-api", "评估多级缓存方案", 260, 231, None, [
            agent("Pearl", "advisor", OPUS, "finished", "对比本地缓存与集中缓存", 255, 240,
                  "热点商品用本地缓存 + 短 TTL，其他走集中缓存。"),
            agent("Tess", "researcher", OPUS, "failed", "收集线上命中率数据", 250, 249),
        ]),
    ])

LINUX = state(
    [{"id": "cli", "label": "claude", "model_label": DS}],
    [
        lead("cli", "Lumen", DS, "working", "infra", "CI 镜像缓存升级", 44, 0,
             "构建缓存改为按锁文件哈希分层，冷启动从 9 分钟降到 3 分钟。", [
                 agent("Greta", "implementer", DS, "working", "改写 CI 缓存分层", 18, 0, "正在调整依赖层的缓存键。", tool="Bash"),
                 agent("Ivy", "researcher", DS, "finished", "对比各缓存后端", 40, 31, "本地 registry 缓存最稳，命中率最高。"),
             ]),
        lead("cli", "Comet", DS, "done", "infra", "清理过期构建产物", 320, 300,
             "清理 41GB 过期产物，并加了每周自动清理任务。", []),
    ])


def push(hub: str, device: str, st: dict):
    body = json.dumps({"device": device, "state": st}, ensure_ascii=False).encode()
    req = urllib.request.Request(hub.rstrip("/") + "/api/ingest", data=body, method="POST",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        print(device, r.status)


if __name__ == "__main__":
    hub = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:7799"
    push(hub, "CatWork🐱", MAC)
    push(hub, "Linux-Box🐧", LINUX)
