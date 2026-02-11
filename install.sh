#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Detect shell config file
if [[ -f ~/.zshrc ]]; then
  SHELL_RC=~/.zshrc
elif [[ -f ~/.bashrc ]]; then
  SHELL_RC=~/.bashrc
else
  echo "Could not find ~/.zshrc or ~/.bashrc"
  exit 1
fi

# Prompt for env vars if not already set in shell config
if ! grep -q 'CHECKOUT_REPO' "$SHELL_RC"; then
  read -rp "Path to your git repo (CHECKOUT_REPO): " repo_path
  read -rp "Directory for worktrees (CHECKOUT_WORKTREE_DIR): " worktree_dir
  {
    echo ""
    echo "# checkout tool"
    echo "export CHECKOUT_REPO=\"$repo_path\""
    echo "export CHECKOUT_WORKTREE_DIR=\"$worktree_dir\""
  } >> "$SHELL_RC"
  echo "Added CHECKOUT_REPO and CHECKOUT_WORKTREE_DIR to $SHELL_RC"
fi

# Install shell function for dev use (builds from source on each invocation)
FUNC='checkout() {
  (cd "'"$SCRIPT_DIR"'" && cargo build --quiet && ./target/debug/checkout "$@")
}'

# Remove old checkout function if present
sed -i '' '/^checkout() {$/,/^}$/d' "$SHELL_RC"

# Add new function
echo "$FUNC" >> "$SHELL_RC"

echo "Installed checkout function to $SHELL_RC"
echo "Run 'source $SHELL_RC' or open a new terminal to use it"

# Symlink Claude skills
SKILLS_DIR="$HOME/.claude/commands/checkout"
mkdir -p "$SKILLS_DIR"

for skill in "$SCRIPT_DIR"/skills/*.md; do
  name="$(basename "$skill")"
  target="$SKILLS_DIR/$name"
  if [[ -L "$target" ]]; then
    rm "$target"
  elif [[ -e "$target" ]]; then
    echo "Warning: $target exists and is not a symlink, skipping"
    continue
  fi
  ln -s "$skill" "$target"
  echo "Linked skill: checkout:$name"
done
