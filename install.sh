#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

FUNC='checkout() {
  (cd "'"$SCRIPT_DIR"'" && cargo build --quiet && ./target/debug/checkout "$@")
}'

# Detect shell config file
if [[ -f ~/.zshrc ]]; then
  SHELL_RC=~/.zshrc
elif [[ -f ~/.bashrc ]]; then
  SHELL_RC=~/.bashrc
else
  echo "Could not find ~/.zshrc or ~/.bashrc"
  exit 1
fi

# Remove old checkout function if present
sed -i '' '/^checkout() {$/,/^}$/d' "$SHELL_RC"

# Add new function
echo "$FUNC" >> "$SHELL_RC"

echo "Installed checkout function to $SHELL_RC"
echo "Run 'source $SHELL_RC' or open a new terminal to use it"
