#!/usr/bin/env bash
set -euo pipefail

if command -v "$HOME/.local/bin/agent-talk" >/dev/null 2>&1; then
  agent_talk="$HOME/.local/bin/agent-talk"
elif command -v agent-talk >/dev/null 2>&1; then
  agent_talk="$(command -v agent-talk)"
else
  tmux display-message "agent-talkd: agent-talk が見つかりません。リポジトリで cargo build --release を実行し、target/release/agent-talk を ~/.local/bin に配置してください"
  exit 1
fi

AGENT_TALK_TMUX_SOCKET="$(tmux display-message -p '#{socket_path}')" \
  "$agent_talk" ensure-daemon >/dev/null 2>&1 || {
  tmux display-message "agent-talkd: デーモンを起動できません。ログを確認してください"
  exit 1
}
