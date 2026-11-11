#!/bin/zsh
# 在 macOS 上安装 Agent Deck 推送采集器（Rust 二进制 deck-push，launchd 常驻，登录即启动）。
# 用法：deploy/macos/install-push.sh <hub 地址，如 https://agent-deck.example.com> <设备名> [档位 JSON 文件]
set -euo pipefail
URL=${1:?用法: install-push.sh <hub-url> <device-name> [sources.json]}
NAME=${2:?用法: install-push.sh <hub-url> <device-name> [sources.json]（设备名必填）}
SOURCES=${3:-}
LABEL=com.ohmyagent.deck-push
REPO=${0:A:h:h:h}
APP="$HOME/Library/Application Support/agent-deck"   # 不放在 ~/Documents：launchd 进程读不了受 TCC 保护的目录
BIN="$APP/deck-push"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOGDIR="$HOME/Library/Logs/agent-deck"
LOG="$LOGDIR/deck-push.log"            # deck-push 自行写入并按大小轮转（DECK_LOG_MAX_BYTES / DECK_LOG_KEEP）

# 设备名校验：不含控制字符，长度 1–40 个字符（格式控制字符由 deck-push 启动时再清洗一次）
if [[ "$NAME" == *[[:cntrl:]]* ]]; then
  print -u2 "设备名不能包含控制字符"; exit 1
fi
if (( ${#NAME} < 1 || ${#NAME} > 40 )); then
  print -u2 "设备名长度须为 1–40 个字符（当前 ${#NAME}）"; exit 1
fi
# hub 地址：必须 http(s):// 开头，不含空白与控制字符
if [[ "$URL" != http://* && "$URL" != https://* ]]; then
  print -u2 "hub 地址须以 http:// 或 https:// 开头：$URL"; exit 1
fi
if [[ "$URL" == *[[:space:][:cntrl:]]* ]]; then
  print -u2 "hub 地址不能包含空白或控制字符"; exit 1
fi
if [[ -n "$SOURCES" && ! -f "$SOURCES" ]]; then
  print -u2 "档位文件不存在：$SOURCES"; exit 1
fi

cargo build --release --manifest-path "$REPO/collector/Cargo.toml"
BUILT="$REPO/collector/target/release/deck-push"

# 给了档位文件时，先用新二进制只读预检一次（解析档位、扫描一次），失败则中止安装
if [[ -n "$SOURCES" ]]; then
  if ! "$BUILT" --sources "$SOURCES" --name "$NAME" --once >/dev/null; then
    print -u2 "档位文件预检失败，已中止安装：$SOURCES"; exit 1
  fi
fi

mkdir -p "$APP" "$HOME/Library/LaunchAgents" "$LOGDIR"
cp "$BUILT" "$BIN.new"
chmod 755 "$BIN.new"
mv -f "$BIN.new" "$BIN"
rm -f "$APP/server.py"                 # 清理旧版 Python 采集器
rm -f "$APP/sources.json"              # 每次安装按参数重新决定是否写入档位文件
[[ -n "$SOURCES" ]] && cp "$SOURCES" "$APP/sources.json"
x() { print -r -- "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }
EXTRA=""
[[ -f "$APP/sources.json" ]] && EXTRA="<string>--sources</string><string>$(x "$APP/sources.json")</string>"

# plist 先写临时文件，plutil 校验通过后再替换
TMP_PLIST=$(mktemp "$PLIST.XXXXXX")
trap 'rm -f "$TMP_PLIST"' EXIT
cat > "$TMP_PLIST" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array>
    <string>$(x "$BIN")</string>
    <string>--log-file</string><string>$(x "$LOG")</string>$EXTRA
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>DECK_PUSH_URL</key><string>$(x "$URL")</string>
    <key>DECK_NAME</key><string>$(x "$NAME")</string>
    <key>DECK_PUSH_INTERVAL</key><string>3</string>
    <key>DECK_HOURS</key><string>12</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>StandardOutPath</key><string>/dev/null</string>
  <key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>
PL
plutil -lint "$TMP_PLIST" >/dev/null
chmod 644 "$TMP_PLIST"
mv -f "$TMP_PLIST" "$PLIST"
launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
sleep 1
ok=0
for i in 1 2 3; do
  if launchctl bootstrap "gui/$UID" "$PLIST"; then ok=1; break; fi
  print -u2 "launchctl bootstrap 失败（第 $i 次），1 秒后重试"
  sleep 1
done
(( ok )) || { print -u2 "launchctl bootstrap 连续 3 次失败，请查看 $LOG"; exit 1; }
print "已安装：$LABEL → $URL（设备名 $NAME），二进制 $BIN"
print "日志：$LOG（超过 1MiB 轮转，保留 .1–.3），查看：tail -f \"$LOG\""
