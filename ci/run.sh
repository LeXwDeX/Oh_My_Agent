#!/usr/bin/env bash
# Agent Deck 本地 Linux CI：只依赖 docker。提交 / 开 PR 前在 Linux 主机的仓库根目录运行：bash ci/run.sh
# 结束时自动清理本脚本创建的容器与镜像（只删带 agent-deck-ci 前缀的对象）。
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="agent-deck-ci-$$"
PY_IMG="python:3.13-alpine"
RS_IMG="rust:1.97-alpine"
NODE_IMG="node:22-alpine"

cleanup() {
  docker rm -f "${TAG}-hub" >/dev/null 2>&1 || true
  docker rmi -f "${TAG}-hub:latest" "${TAG}-push:latest" >/dev/null 2>&1 || true
}
trap cleanup EXIT
step() { printf '\n==> %s\n' "$*"; }

step "Python 语法检查"
docker run --rm -v "$PWD":/src:ro -w /src -e PYTHONDONTWRITEBYTECODE=1 "$PY_IMG" \
  python -c "import ast,sys; [ast.parse(open(f).read(), f) for f in ('server.py','tools/gen_avatars.py','tools/demo_push.py')]; print('ok')"

step "任务分类器单测（unittest，不访问网络）"
docker run --rm -v "$PWD":/src:ro -w /src -e PYTHONDONTWRITEBYTECODE=1 "$PY_IMG" \
  python -m unittest discover -s tests -v

step "前端：去处规则单测（node --test）"
docker run --rm -v "$PWD":/src:ro -w /src "$NODE_IMG" node --test tests/*.test.mjs

step "compose 配置校验"
for f in docker-compose.yml docker-compose.local.yml deploy/linux/docker-compose.push.yml; do
  HOME=/tmp docker compose -f "$f" config -q && echo "ok  $f"
done

step "Rust：clippy（警告即失败）+ 单元测试"
docker run --rm -v "$PWD":/src:ro -w /src/collector -e CARGO_TARGET_DIR=/tmp/target "$RS_IMG" sh -ec '
  apk add --no-cache musl-dev >/dev/null
  rustup component add clippy >/dev/null 2>&1 || true
  cargo clippy --locked --release --all-targets -- -D warnings
  cargo test --locked --release'

step "构建 hub 与采集器镜像"
docker build -q -t "${TAG}-hub:latest" . >/dev/null && echo "ok  hub"
docker build -q -t "${TAG}-push:latest" -f deploy/collector.Dockerfile . >/dev/null && echo "ok  push"

step "冒烟：hub 启动 → 采集器 --once 生成状态 → 推送 → 读回"
docker run -d --name "${TAG}-hub" --read-only --cap-drop ALL -e DECK_MODE=hub -e DECK_HOST=0.0.0.0 "${TAG}-hub:latest" >/dev/null
for _ in $(seq 1 20); do
  docker exec "${TAG}-hub" wget -qO- http://127.0.0.1:7788/healthz >/dev/null 2>&1 && break; sleep 0.5
done
STATE=$(docker run --rm --entrypoint /usr/local/bin/deck-push -e DECK_SOURCES='[{"id":"cli","label":"claude","dir":"/nonexistent","entry":null}]' \
  "${TAG}-push:latest" --once --name ci 2>/dev/null)
printf '{"device":"ci","state":%s}' "$STATE" | docker exec -i "${TAG}-hub" sh -c \
  'wget -qO- --header "Content-Type: application/json" --post-file=/dev/stdin http://127.0.0.1:7788/api/ingest >/dev/null'
docker exec "${TAG}-hub" wget -qO- http://127.0.0.1:7788/api/state | grep -q '"name": "ci"' && echo "ok  设备 ci 已出现在 /api/state"
CODE=$(docker exec "${TAG}-hub" sh -c 'wget -S -qO- --post-data="notjson" http://127.0.0.1:7788/api/ingest 2>&1 | grep -o "HTTP/1.[01] [0-9]*" | tail -1' || true)
case "$CODE" in *400*) echo "ok  非法请求返回 400";; *) echo "FAIL 非法请求应返回 400，实际：$CODE"; exit 1;; esac

printf '\n全部通过 ✅\n'
