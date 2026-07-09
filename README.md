# checkout

A CLI tool for managing git worktrees with Codex and Claude Code integration. It creates isolated worktrees for PR reviews or new branches, then opens a coding-agent session inside them. Codex is the default; pass `--agent claude` to use Claude Code.

## Features

- **`checkout pr <number|url>`** — Check out a GitHub PR into a worktree
- **`checkout review <number|url>`** — Check out a PR and start a code review
- **`checkout branch <name>`** — Create a new branch in a worktree
- **`checkout new`** — Create or recycle a randomly named worktree
- **`checkout resume`** — Browse Codex and Claude sessions together and resume with the original agent
- **`checkout resume-last`** — Resume the most recently exited session for the selected agent
- **`checkout status`** — List all worktrees and their status
- **`checkout clean`** — Remove worktrees with no uncommitted changes

Each worktree gets:
- A unique iTerm2 background color for visual distinction
- `node_modules` symlinked from the main repo
- Claude settings and trust copied over when Claude is selected
- Worktree safety guidance injected into both agents
- `mise trust` run automatically (if mise is installed)

## Install

```sh
cargo install --path .
```

Or, if you're making frequent changes and don't want to reinstall each time:

```sh
./install.sh
```

This adds a shell function that builds from source on each invocation and links the bundled workflows into both Claude and Codex.

## Configuration

| Environment Variable | Description | Default |
|---|---|---|
| `CHECKOUT_REPO` | Path to the main git repo | (required) |
| `CHECKOUT_WORKTREE_DIR` | Directory for worktrees | (required) |

## Options

| Flag | Description |
|---|---|
| `--agent <codex\|claude>` | Select the agent for new sessions and `resume-last` (default: `codex`) |
| `--no-agent` | Skip launching an agent after creating the worktree |
| `--prompt <file>` | Use file contents as the initial agent prompt (`branch` and `new`) |
| `--repo <path>` | Override the repo path |
| `-y` | Skip confirmation in `clean` |

`--no-claude` and `--claude-prompt` remain accepted as compatibility aliases for `--no-agent` and `--prompt`.

Examples:

```sh
checkout new                         # Codex
checkout new --agent claude          # Claude Code
checkout resume                      # browse Codex and Claude sessions
```

## Requirements

- [gh](https://cli.github.com/) (GitHub CLI)
- [Codex CLI](https://developers.openai.com/codex/cli) (default agent)
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI (when using `--agent claude`)
- iTerm2 (for background color differentiation)
