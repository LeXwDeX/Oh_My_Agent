#!/usr/bin/env python3
"""按 avatars/roster.json 生成二次元头像到 avatars/<name>.png（已存在则跳过）。

接口为 OpenAI 兼容的 /v1/images/generations。连接信息只从环境变量读取，不写入仓库：
  IMG_BASE_URL  （缺省回退 LLM_BASE_URL）
  IMG_API_KEY   （缺省回退 LLM_API_KEY）
  IMG_MODEL     （缺省 gpt-image）
  IMG_QUALITY   （可选，如 high；不设则用接口默认档）
用法：python3 tools/gen_avatars.py [--only Ada,Bram] [--force] [-j 4] [--size 192]
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import tempfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
AV = ROOT / "avatars"


def gen(base: str, key: str, model: str, prompt: str) -> bytes:
    payload = {"model": model, "prompt": prompt, "size": "1024x1024", "n": 1}
    if os.environ.get("IMG_QUALITY"):
        payload["quality"] = os.environ["IMG_QUALITY"]
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        base.rstrip("/") + "/v1/images/generations",
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=300) as r:
        d = json.load(r)
    item = d["data"][0]
    if item.get("b64_json"):
        return base64.b64decode(item["b64_json"])
    with urllib.request.urlopen(item["url"], timeout=120) as r:
        return r.read()


def shrink(png: bytes, out: Path, size: int) -> None:
    """用 macOS sips 缩放；没有 sips 时原图落盘。"""
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as t:
        t.write(png)
        tmp = t.name
    try:
        subprocess.run(["sips", "-Z", str(size), tmp, "--out", str(out)], check=True, capture_output=True)
    except (FileNotFoundError, subprocess.CalledProcessError):
        out.write_bytes(png)
    finally:
        os.unlink(tmp)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only")
    ap.add_argument("--force", action="store_true")
    ap.add_argument("-j", type=int, default=4)
    ap.add_argument("--size", type=int, default=192)
    a = ap.parse_args()

    base = os.environ.get("IMG_BASE_URL") or os.environ.get("LLM_BASE_URL")
    key = os.environ.get("IMG_API_KEY") or os.environ.get("LLM_API_KEY")
    model = os.environ.get("IMG_MODEL", "gpt-image")
    if not base or not key:
        print("缺少 IMG_BASE_URL / IMG_API_KEY（或 LLM_BASE_URL / LLM_API_KEY）", file=sys.stderr)
        return 2

    roster = json.loads((AV / "roster.json").read_text())
    style = roster["style"]
    jobs = []
    only = set(a.only.split(",")) if a.only else None
    for role, people in (("captain", roster["captains"]), ("crew", roster["crew"])):
        for p in people:
            if only and p["name"] not in only:
                continue
            out = AV / f"{p['name'].lower()}.png"
            if out.exists() and not a.force:
                continue
            extra = " Leader aura, slightly more mature, still cute and genki." if role == "captain" else ""
            jobs.append((p["name"], out, f"{style}. Character: {p['look']}.{extra}"))

    print(f"待生成 {len(jobs)} 张，模型 {model}，并发 {a.j}", flush=True)
    fails = 0
    with ThreadPoolExecutor(max_workers=a.j) as ex:
        futs = {ex.submit(gen, base, key, model, pr): (n, out) for n, out, pr in jobs}
        for f in as_completed(futs):
            n, out = futs[f]
            try:
                shrink(f.result(), out, a.size)
                print(f"  ✓ {n}", flush=True)
            except Exception as e:  # noqa: BLE001
                fails += 1
                print(f"  ✗ {n}: {e}", flush=True)
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
