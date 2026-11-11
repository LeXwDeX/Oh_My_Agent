#!/bin/zsh
# 卸载 Agent Deck 推送采集器（deck-push 二进制、档位文件与 launchd 配置）
LABEL=com.ohmyagent.deck-push
APP="$HOME/Library/Application Support/agent-deck"
launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
rm -f "$APP/deck-push" "$APP/deck-push.new" "$APP/sources.json" "$APP/server.py"
rmdir "$APP" 2>/dev/null || true
print "已卸载 $LABEL（日志目录保留在 ~/Library/Logs/agent-deck/，不需要可手动删除）"
