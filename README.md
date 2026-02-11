# checkout

A CLI tool for managing git worktrees with Claude Code integration. Creates isolated worktrees for PR reviews or new branches, then spawns Claude sessions inside them.

## Features

- **`checkout pr <number|url>`** — Check out a GitHub PR into a worktree
- **`checkout review <number|url>`** — Check out a PR and start a code review
- **`checkout branch <name>`** — Create a new branch in a worktree (auto-prefixes `darren/`)
- **`checkout status`** — List all worktrees and their status
- **`checkout clean`** — Remove worktrees with no uncommitted changes

Each worktree gets:
- A unique iTerm2 background color for visual distinction
- `node_modules` symlinked from the main repo
- Claude settings and trust copied over
- `mise trust` run automatically (if mise is installed)

## Install

```sh
./install.sh
```

This adds a `checkout` shell function to your `.zshrc`/`.bashrc`.

## Options

| Flag | Description |
|---|---|
| `--no-claude` | Skip launching Claude after creating the worktree |
| `--claude-prompt <file>` | Use file contents as the initial Claude prompt (branch only) |
| `--repo <path>` | Override the repo path (default: `~/figma/figma`) |
| `-y` | Skip confirmation in `clean` |

## Requirements

- [gh](https://cli.github.com/) (GitHub CLI)
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI
- iTerm2 (for background color differentiation)
