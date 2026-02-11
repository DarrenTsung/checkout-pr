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

## Phase 2: Full Code Review

Check if this PR touches multiplayer code:
```bash
git diff $(git merge-base HEAD main)..HEAD --name-only | grep -E "(multiplayer|mp_)" || echo "NOT_MULTIPLAYER"
```

Create an agent team to review this PR from multiple angles. Use Opus for all teammates. Spawn the following teammates:

### Teammate 1: Correctness Reviewer

Spawn a teammate with this prompt:

> Review the PR diff for correctness issues. Look for:
> - Code that will fail to compile or parse (syntax errors, type errors, missing imports, unresolved references)
> - Code that will definitely produce wrong results regardless of inputs (clear logic errors)
> - Security issues or incorrect logic in the changed code
>
> **CRITICAL: We only want HIGH SIGNAL issues.** Flag issues where:
> - The code will fail to compile or parse
> - The code will definitely produce wrong results regardless of inputs
>
> Do NOT flag:
> - Code style or quality concerns
> - Potential issues that depend on specific inputs or state
> - Subjective suggestions or improvements
> - Pre-existing issues
> - Pedantic nitpicks that a senior engineer would not flag
> - Issues that a linter will catch
>
> If you are not certain an issue is real, do not flag it. False positives erode trust.
>
> Output a list of issues found. For each issue, include the file path relative to the working directory with line number (e.g., src/file.ts:123) and description. If no issues, output "No correctness issues found."

### Teammate 2: Test Coverage Reviewer

Spawn a teammate with this prompt:

> Analyze the PR diff for test coverage. Identify:
> 1. New functionality being added
> 2. Existing functionality being modified
> 3. Whether appropriate tests exist for these changes
>
> For each new or modified piece of functionality, check:
> - Is there a corresponding test?
> - Does the test cover the key behaviors?
> - Are edge cases tested where appropriate?
>
> Do NOT flag:
> - Missing tests for trivial changes (simple renames, formatting)
> - Pre-existing test gaps unrelated to this PR
> - Over-testing suggestions (testing implementation details)
>
> Output a list of coverage gaps. For each gap, include the file path relative to the working directory with line number (e.g., src/file.ts:123) and describe what test is needed. If coverage is adequate, output "Test coverage is adequate."

### Teammate 3: Test Readability Reviewer

Spawn a teammate with this prompt:

> Review test code in this PR for readability. Tests should be understandable with minimal context on test internals or special casing.
>
> Check for:
> 1. **Naming clarity**: Test names should clearly describe what is being tested and expected behavior
>    - Prefer: `test_user_login_fails_with_invalid_password`
>    - Avoid: `test_case_1`, `test_login_error`
>    - Test names must accurately match what the test actually does - flag tests where the name is misleading or outdated
>
> 2. **Self-documenting tests**: Tests should be readable without needing to understand test framework internals
>    - Variable names should be descriptive
>    - Setup and assertions should be clear
>
> 3. **Minimal magic**: Avoid obscure helper functions or macros without clear names
>    - If helpers are used, their names should make the test readable
>
> 4. **Comments**: Comments explaining WHY are fine, but prefer readable names over obscure names + comments
>
> Do NOT flag:
> - Pre-existing test style issues
> - Minor style preferences
> - Tests that are already clear
>
> Output specific suggestions for improving test readability. For each issue, include the file path relative to the working directory with line number (e.g., src/file.test.ts:123). If tests are readable, output "Test readability is good."

### Teammate 4: Test Timing Reviewer

Spawn a teammate with this prompt:

> Review test code in this PR for hardcoded sleeps and timing-based synchronization.
>
> Look for:
> - `sleep()`, `setTimeout()`, `thread::sleep()`, `tokio::time::sleep()`, or similar timing functions in tests
> - Arbitrary delays used for synchronization (e.g., waiting for async operations to complete)
> - Flaky test patterns that rely on timing assumptions
>
> For each hardcoded sleep found, think hard about whether it's truly necessary or if it could be replaced with:
> - Async channels (mpsc, oneshot, broadcast)
> - Signals/notifications (condvar, notify, semaphore)
> - Polling with condition checks
> - Explicit synchronization primitives
> - Event-driven waiting (await on a future/promise that resolves when ready)
>
> Some sleeps ARE appropriate (e.g., testing timeout behavior, rate limiting). Only flag sleeps that are used as a workaround for proper synchronization.
>
> Output each finding with the file path relative to the working directory with line number (e.g., src/test.rs:45), the current sleep pattern, and a suggested alternative approach. If no problematic sleeps found, output "No timing issues found."

### Teammate 5: Test Value Reviewer

Spawn a teammate with this prompt:

> Review test code in this PR for redundancy and value.
>
> Look for:
> - **Redundant tests**: Multiple tests that verify the same behavior or code path
> - **Low-value tests**: Tests that only verify trivial behavior (e.g., testing getters/setters, testing that a constructor sets fields)
> - **Duplicate assertions**: Tests that repeat assertions already covered by other tests in the same PR
> - **Over-mocking**: Tests that mock so much they're not testing real behavior
> - **Tautological tests**: Tests that can never fail because they test the mock, not the implementation
>
> Consider whether each test in the PR adds unique value:
> - Does it test a distinct code path or behavior?
> - Would removing it reduce confidence in the code?
> - Is it testing implementation details that could change without affecting correctness?
>
> Do NOT flag:
> - Tests that appear similar but cover different edge cases
> - Integration tests that overlap with unit tests (both have value)
> - Pre-existing redundant tests not introduced in this PR
>
> Output each finding with the file path relative to the working directory with line number (e.g., src/test.rs:45), explain why the test is redundant or low-value, and suggest whether to remove or consolidate. If all tests add value, output "All tests add value."

### Teammate 6: Protocol Consistency Reviewer (only if multiplayer PR)

**Only spawn this teammate if the PR touches multiplayer code.**

Spawn a teammate with this prompt:

> Review this multiplayer PR for TypeScript/Rust protocol consistency.
>
> Check that any protocol changes are synchronized between TypeScript and Rust:
> 1. Message types defined in both languages should match
> 2. Field names and types should be consistent
> 3. Enum variants should match
> 4. Serialization/deserialization should be compatible
>
> Look at files in:
> - TypeScript: Look for protocol definitions, message types, API contracts
> - Rust: Look for corresponding structs, enums, and serde attributes
>
> Output any mismatches found between TS and Rust protocol definitions. For each mismatch, include file paths relative to the working directory with line numbers for both the TS and Rust locations (e.g., src/protocol.ts:45 and multiplayer/src/protocol.rs:78). If protocols are consistent, output "Protocol definitions are consistent."

---

## Phase 3: Aggregate and Present Results

Wait for all teammates to complete their reviews. Then synthesize their findings into a single review:

---

## Code Review Summary

### Correctness
[Teammate 1 findings]

### Test Coverage
[Teammate 2 findings]

### Test Readability
[Teammate 3 findings]

### Test Timing
[Teammate 4 findings]

### Test Value
[Teammate 5 findings]

### Protocol Consistency (if applicable)
[Teammate 6 findings]

---

**IMPORTANT:** Only report unique issues. If multiple teammates flag the same issue, consolidate into one finding.

---

## Phase 4: Interactive Q&A

After presenting the review summary, ask the user what specific questions they have about the PR. Common questions might include:
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
- When the user asks you to reply to a review comment, use: `gh api -X POST repos/{owner}/{repo}/pulls/<PR_NUMBER>/comments -F body="<your_reply>" -F in_reply_to=<COMMENT_ID>`
- The comment ID is the numeric ID from the review comment URL (e.g., https://github.com/{owner}/{repo}/pull/123#discussion_r12345 → comment ID is 12345)
- Example: `gh api -X POST repos/{owner}/{repo}/pulls/123/comments -F body="Done in abc123def" -F in_reply_to=12345`
- Note: Use `-F` (not `-f`) for the `in_reply_to` parameter since it must be passed as a number
