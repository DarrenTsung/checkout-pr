---
description: Run a full code review on the current PR branch and write findings to review.md
---
Run a full code review on the current branch's PR.

**Important:** Do not push commits or reply to PR comments without explicit user approval. Always ask first.

**Assumption:** This command assumes you are already running in a git worktree that has the PR branch checked out.

---

## Phase 1: Gather Context

1. Fetch the PR details:
   - Run `gh pr view <PR_NUMBER> --json title,body,files,additions,deletions,author,baseRefName,headRefName,state,mergeable,reviewDecision`
   - Run `gh pr diff <PR_NUMBER>` to get the full diff
   - Run `gh pr view <PR_NUMBER> --comments` to get all PR comments (the author's comments often explain known issues, trade-offs, or intentional decisions)

If the PR number was not provided as $ARGUMENTS, determine it from the current branch:
   - Run `gh pr list --head $(git rev-parse --abbrev-ref HEAD) --json number --jq '.[0].number'`

---

## Phase 2: Full Code Review

Check if this PR touches multiplayer or Go code:
```bash
git diff $(git merge-base HEAD main)..HEAD --name-only | grep -E "(multiplayer|mp_)" || echo "NOT_MULTIPLAYER"
git diff $(git merge-base HEAD main)..HEAD --name-only | grep '\.go$' || echo "NOT_GO"
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
> - Coverage gaps where the code path is already exercised by existing tests, even if with a different input variant
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

### Teammate 6: Coherence Reviewer

Spawn a teammate with this prompt:

> Review this PR for coherence — does the implementation actually accomplish what the PR claims to do, and does it avoid unnecessary work? Read the PR title and body (provided via `gh pr view`) and compare against the actual diff.
>
> Look for:
> - **Incomplete implementation**: Features described in the PR body that aren't actually implemented in the diff
> - **Commented-out code**: Tests or logic that was commented out rather than fixed (e.g., `// TODO: fix later`, `// skipping for now`)
> - **Disabled tests**: Tests marked as `#[ignore]`, `.skip()`, `xit(`, `xdescribe(`, or similar — especially if they were previously enabled
> - **Deferred logic**: `todo!()`, `unimplemented!()`, `// FIXME`, placeholder implementations, or stubbed-out functions that return hardcoded values
> - **Scope drift**: Changes unrelated to the stated goal of the PR that snuck in without explanation
> - **Contradictions**: The PR body says one thing but the code does another (e.g., "removes feature X" but feature X is still partially present)
> - **Hardcoded workarounds**: Magic numbers, hardcoded paths, or temporary hacks that bypass the proper solution
> - **Silent behavior changes**: Functions that now silently swallow errors, return early, or skip logic without explanation
> - **Unnecessary code**: Functions that handle cases that can never be reached from their call sites (e.g., an `Option` parameter that is always `Some`, a match arm for a variant that's never constructed). Check each function's callers to see if defensive branches are actually reachable.
> - **Unnecessary work**: Redundant iterations over the same data that could be combined into a single pass, or computations that are performed unconditionally but only consumed conditionally
>
> Do NOT flag:
> - Intentional `todo!()` or `unimplemented!()` that are explicitly called out in the PR body as planned follow-ups
> - Pre-existing disabled tests unrelated to this PR
> - Reasonable scope limitations that are acknowledged in the PR description
>
> Output each finding with the file path relative to the working directory with line number (e.g., src/file.ts:123), what the issue is, and why it matters. If the PR is coherent, output "PR implementation is coherent with its stated goals."

### Teammate 7: Risk Reviewer

Spawn a teammate with this prompt:

> Review this PR for hidden risks and questionable assumptions. Don't look for bugs — look for things that could go wrong in production even if the code is technically correct.
>
> **Question the assumptions:**
> - What invariants does this code rely on? Are they documented or enforced?
> - What happens if the input data doesn't match expected patterns (unexpected nulls, empty collections, malformed strings)?
> - Are there implicit ordering dependencies (e.g., "X must happen before Y") that aren't enforced?
> - Does this code assume single-threaded execution, low latency, or bounded data sizes?
>
> **Look for failure modes:**
> - What happens under concurrent access or high load?
> - What's the failure mode if an external dependency (database, API, service) is slow or unavailable?
> - Are there race conditions between multiple readers/writers?
> - Can partial failures leave the system in an inconsistent state?
> - What happens if this code is called with unexpected frequency (e.g., thundering herd, retry storms)?
>
> **Assess blast radius:**
> - If this change breaks, what's affected? Just this feature, or does it cascade?
> - Is there a rollback path? Feature flag? Can this be safely reverted?
> - Does this change affect data persistence? Could bad data be written that's hard to clean up?
>
> Do NOT flag:
> - Theoretical risks that require extremely unlikely conditions
> - Risks already mitigated by existing error handling visible in the diff
> - Generic concerns without specific connection to code in this PR
> - Pre-existing risks not introduced or worsened by this PR
>
> Output each risk with the file path relative to the working directory with line number (e.g., src/file.ts:123), the assumption being made, and what could go wrong if it's violated. If no significant risks, output "No hidden risks identified."

### Teammate 8: Multiplayer Reviewer (only if multiplayer PR)

**Only spawn this teammate if the PR touches multiplayer code.**

Spawn a teammate with this prompt:

> Review this multiplayer PR for protocol consistency and multiplayer-specific patterns.
>
> **Protocol consistency:** Check that any protocol changes are synchronized between TypeScript and Rust:
> 1. Message types defined in both languages should match
> 2. Field names and types should be consistent
> 3. Enum variants should match
> 4. Serialization/deserialization should be compatible
> 5. Proto schema files (in `figment/schemas/`) should be updated when analytics events gain new fields or new events are added
>
> Look at files in:
> - TypeScript: Look for protocol definitions, message types, API contracts
> - Rust: Look for corresponding structs, enums, and serde attributes
>
> **Kiwi/NodeChange performance patterns:** When code accesses fields on NodeChange or kiwi-generated types, check for:
> - Using `get_*().is_some()` when `has_*()` exists — `get_*` decodes the field value which is more expensive than `has_*` which only checks for presence. Flag cases where the decoded value is not used.
> - Unnecessary field decoding in hot loops — prefer existence checks over value extraction when only checking presence
>
> **Operational safety for hot paths:** If the PR adds new computation to the scenegraph query path, initial load path, or message handling path:
> - Is the new work gated behind a LaunchDarkly feature flag for safe rollout/rollback?
> - Could the computation be deferred or made async if it's not needed for the response?
>
> Output any issues found with file paths relative to the working directory and line numbers (e.g., src/protocol.ts:45 and multiplayer/src/protocol.rs:78). If no issues, output "No multiplayer-specific issues found."

### Teammate 9: Go Style Reviewer (only if Go PR)

**Only spawn this teammate if the PR touches `.go` files.**

Spawn a teammate with this prompt:

> Review this PR's Go code against our style guide. Focus on patterns that affect correctness, maintainability, and API design — not cosmetic issues that `gofmt` handles.
>
> **Error handling:**
> - Handle OR return errors, never both (no `log.Error(err); return err` double-handling)
> - Return an error OR a usable value, not both (no `return partialResult, err`)
> - Errors should be wrapped with context: `fmt.Errorf("fetching user: %w", err)`
> - Never `panic` in library code; never `recover`
> - At error origin, add stack trace with `xerrors.WithStack(err)`
>
> **Context usage:**
> - `ctx context.Context` must be the first parameter, named `ctx`
> - Pass context through, don't store it in structs
> - Context values only for request-scoped cross-cutting concerns (logging, tracing) — never for controlling behavior
>
> **Types and APIs:**
> - Use type definitions for domain concepts (e.g., `type UserID uint64`) instead of bare primitives
> - Prefer generics over `interface{}` / `any` for type-safe APIs
> - Interfaces should be narrow (1-3 methods); define where used, not where implemented
> - Use options pattern (`func WithTimeout(d time.Duration) ClientOption`) for extensible config
> - Return concrete types, accept interfaces
>
> **Dependencies and construction:**
> - Take dependencies as interface arguments (dependency injection), not global singletons
> - Parse env/flags only in `main()` — pass explicit config structs to components
> - Never check `if production` — use explicit config
> - Prefer standard library, then minimal well-known deps, then frameworks
>
> **Concurrency:**
> - Prefer NOT spawning goroutines — let callers decide
> - Always know when goroutines will exit; use `xsync.Group` for background tasks
> - Be judicious with channels — often a mutex is simpler
>
> **Conditionals and style:**
> - Use `len(x) > 0` not `x != nil` for "has items" checks on slices/maps
> - Don't check nil before `len()` — `len(nil) == 0` is safe
> - Use early returns to flatten nested conditionals
> - Never use bare returns in named return functions
> - Use pointer receivers consistently (don't mix value and pointer receivers)
>
> **Testing:**
> - Use standard `testing` package (exception: `testify/require` for assertions)
> - Prefer fakes (in-memory implementations) over mock frameworks
>
> Do NOT flag:
> - Issues already caught by `gofmt`, `go vet`, or standard linters
> - Pre-existing style violations not introduced by this PR
> - Minor naming preferences that don't affect clarity
>
> Output each finding with the file path relative to the working directory with line number (e.g., go/common/metrics/client.go:45), the pattern violated, and a brief fix suggestion. If no issues, output "Go code follows style guide."

---

## Phase 3: Write Review File

Wait for all teammates to complete their reviews. Then synthesize their findings into a structured markdown file.

**Cross-reference with PR comments:** Before writing each finding, check if it was already acknowledged or discussed in the PR comments you fetched in Phase 1. If the author or a reviewer already called out the issue, append "(Acknowledged by author in PR comments)" to the description.

**Consolidate duplicates:** If multiple teammates flag the same issue, write it once in the most relevant section.

Write `review.md` to the worktree root with this structure:

```markdown
# Code Review: PR #<number> - <title>

> <1-3 sentence summary of the PR>

## Summary

| # | Finding | Outcome |
|---|---------|---------|
| 1 | ◯ <short description> | N/A |
| 2 | ◯ ... | N/A |

## Fix

### ◯ <Concise finding title> <!-- @actions: elaborate, fix, ignore -->

> **Outcome:** N/A
>
> [Correctness] `path/to/file.ts:123` - <description of the issue>

<br>

### ◯ <Another finding> <!-- @actions: elaborate, fix, ignore -->

> **Outcome:** N/A
>
> [Risks] `path/to/file.ts:456` - <description>

## Follow-up

### ◯ <Finding title> <!-- @actions: elaborate, fix, ignore -->

> **Outcome:** N/A
>
> [Test Timing] `path/to/file.ts:789` - <description>

## Ignore

### ◯ <Finding title> <!-- @actions: elaborate, fix, ignore -->

> **Outcome:** N/A
>
> [Coherence] `path/to/file.ts:101` - <description>
```

Rules:
- The three `##` headings are **Fix**, **Follow-up**, and **Ignore**. Each finding is a `###` subsection under the appropriate recommendation heading.
- Each finding's description starts with the reviewer category in brackets (e.g., `[Correctness]`, `[Risks]`, `[Go Style]`), followed by a backtick-wrapped `file:line` reference and description.
- The finding body (Outcome and description) is a single block quote.
- Every `###` finding title and its corresponding summary table row start with a status emoji: ◯ (no outcome), 🟣 (in progress/elaborate), 🟡 (ignored), 🟢 (done/fixed). Initially all are ◯.
- Add `<br>` between findings within the same `##` section (between the end of one block quote and the next `###` heading).
- The `<!-- @actions: elaborate, fix, ignore -->` comment goes on the `###` heading line itself (e.g., `### ◯ Finding title <!-- @actions: elaborate, fix, ignore -->`).
- Add a numbered summary table at the top with all findings. Outcome is initially `N/A`.
- If no findings for a recommendation group, omit that `##` section entirely.

After writing the file, tell the user: "Review written to `review.md`."
