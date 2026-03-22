---
description: Checkout a PR, summarize it, and run a full code review
---
Checkout and review PR $ARGUMENTS using the gh CLI tool.

**Important:** Do not push commits or reply to PR comments without explicit user approval. Always ask first.

**Assumption:** This command assumes you are already running in a git worktree that has the PR branch checked out. The worktree setup is handled by the `checkout_pr` shell function before spawning claude.

---

## Phase 1: Checkout and Summarize

1. First, fetch the PR details:
   - Run `gh pr view <PR_NUMBER> --json title,body,files,additions,deletions,author,baseRefName,headRefName,state,mergeable,reviewDecision`
   - Run `gh pr diff <PR_NUMBER>` to get the full diff

2. Summarize the PR:
   - Title and author
   - What the PR is doing at a high level
   - Key files changed and their purpose
   - Any notable patterns or architectural decisions

---

## Phase 2: Code Review

Run `/checkout:review <PR_NUMBER>` to perform the full code review and write findings to `review.md`.

---

## Phase 3: Interactive Q&A

After the review is complete, ask the user what specific questions they have about the PR. Common questions might include:
- Why certain code can be deleted
- How a feature flag cleanup affects the codebase
- What the test changes are validating
- Whether there are any concerns with the approach
- Deeper exploration of any review findings

When answering questions:
- Reference specific lines from the diff when relevant
- Use `gh pr diff <PR_NUMBER>` to get more context if needed
- If you need to understand existing code better, read files directly (you're already in the worktree)
- Explain the "why" behind changes, not just the "what"
- When linking to files, use paths relative to the current directory (e.g., `multiplayer/lib/rust_process.ts:2465`)
- When providing a local path, also remind the user: "To compare against the base branch (`<baseRefName>`), use `Cmd+Shift+G M` → GitLens: Open Changes with Branch"

When resolving comments, please DO NOT push the commit until approval by the user.

Replying to PR review comments:
- When the user asks you to reply to a review comment, use: `gh api -X POST repos/figma/figma/pulls/<PR_NUMBER>/comments -F body="<your_reply>" -F in_reply_to=<COMMENT_ID>`
- The comment ID is the numeric ID from the review comment URL (e.g., https://github.com/figma/figma/pull/650848#discussion_r2678105376 → comment ID is 2678105376)
- Example: `gh api -X POST repos/figma/figma/pulls/650848/comments -F body="Done in abc123def" -F in_reply_to=2678105376`
- Note: Use `-F` (not `-f`) for the `in_reply_to` parameter since it must be passed as a number
