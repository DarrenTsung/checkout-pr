#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source_script="$script_dir/checkout_iterm_daemon.py"
autolaunch_dir="$HOME/Library/ApplicationSupport/iTerm2/Scripts/AutoLaunch"
target_script="$autolaunch_dir/checkout_iterm_daemon.py"
socket_path="$HOME/.local/share/checkout/iterm-api.sock"

daemon_ready() {
  local reply
  reply="$(printf '{"action":"ping"}\n' | /usr/bin/nc -U -w 1 "$socket_path" 2>/dev/null || true)"
  [[ "$reply" == *'"ok":true'* ]]
}

mkdir -p "$autolaunch_dir"
if [[ -L "$target_script" ]]; then
  rm "$target_script"
fi
install -m 755 "$source_script" "$target_script"

echo "Installed iTerm Python API AutoLaunch daemon: $target_script"
if daemon_ready; then
  echo "The daemon is running."
else
  echo "The daemon will start the next time iTerm2 starts."
fi
