---
description: Run a full code review on the current PR branch and write findings to review.md
---
Run a full code review on the current branch's PR.

**Important:** Do not push commits or reply to PR comments without explicit user approval. Always ask first.

**Assumption:** This command assumes you are already running in a git worktree that has the PR branch checked out.

---

## Phase 0: Determine review scope

Parse `$ARGUMENTS` to determine what diff to review. Two modes:

**Full-PR mode (default).** A PR number, or no argument. The scope is the full PR diff vs. its base branch.

```
SCOPE_LABEL="PR #<N>"
SCOPE_DIFF_CMD="gh pr diff <N>"
SCOPE_FILES_CMD="git diff --name-only $(git merge-base HEAD <base>)..HEAD"
```

If no PR number was passed, resolve from the current branch:
```bash
PR=$(gh pr list --head "$(git rev-parse --abbrev-ref HEAD)" --json number --jq '.[0].number')
```

**Commit-scoped mode.** `$ARGUMENTS` contains `--commits SHA1,SHA2,...` (comma-separated, in chronological order). Scope is *only* those commits' combined diff. Used by `/darren:triage-review` to re-review fix commits without re-examining the whole branch.

```
SCOPE_LABEL="commits SHA1..SHA_LAST"
SCOPE_DIFF_CMD="git show --no-merges --first-parent SHA1 SHA2 ..."
SCOPE_FILES_CMD="git show --name-only --pretty=format: SHA1 SHA2 ... | sort -u"
```

In commit-scoped mode, still fetch PR metadata (`gh pr view`) and PR comments — they remain useful context — but the diff under review is the commits, not the full PR.

When teammate prompts below say "the PR diff," "the diff," or "this PR's changes," interpret them as `$SCOPE_LABEL`'s diff. Pass `$SCOPE_LABEL` and `$SCOPE_DIFF_CMD` into each teammate's prompt explicitly so they review the right surface.

---

## Phase 1: Gather Context

1. Fetch the PR details:
   - Run `gh pr view <PR_NUMBER> --json title,body,files,additions,deletions,author,baseRefName,headRefName,state,mergeable,reviewDecision`
   - Run `$SCOPE_DIFF_CMD` to get the diff under review (full PR in default mode, just the listed commits in commit-scoped mode).
   - Run `gh pr view <PR_NUMBER> --comments` to get all PR comments (the author's comments often explain known issues, trade-offs, or intentional decisions).

---

## Phase 2: Full Code Review

Check whether the diff touches multiplayer or Go code:
```bash
$SCOPE_FILES_CMD | grep -E "(multiplayer|mp_)" || echo "NOT_MULTIPLAYER"
$SCOPE_FILES_CMD | grep '\.go$' || echo "NOT_GO"
```

Create an agent team to review this diff from multiple angles. Use Opus for all teammates. Each teammate prompt should be prefixed with the scope it covers (e.g. "Review scope: commits abc123..def456. Use `git show abc123 def456` to fetch the diff."), so commit-scoped runs don't accidentally pull in the whole PR. Spawn the following teammates:

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
> Output a list of issues found. For each issue, include the file path relative to the working directory with line number (e.g., src/file.ts:123) and description. For each finding, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If no issues, output "No correctness issues found."

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
> Output a list of coverage gaps. For each gap, include the file path relative to the working directory with line number (e.g., src/file.ts:123) and describe what test is needed. For each gap, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If coverage is adequate, output "Test coverage is adequate."

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
> Output specific suggestions for improving test readability. For each issue, include the file path relative to the working directory with line number (e.g., src/file.test.ts:123). For each suggestion, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If tests are readable, output "Test readability is good."

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
> Output each finding with the file path relative to the working directory with line number (e.g., src/test.rs:45), the current sleep pattern, and a suggested alternative approach. For each finding, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If no problematic sleeps found, output "No timing issues found."

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
> Output each finding with the file path relative to the working directory with line number (e.g., src/test.rs:45), explain why the test is redundant or low-value, and suggest whether to remove or consolidate. For each finding, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If all tests add value, output "All tests add value."

### Teammate 6: Coherence Reviewer

Before spawning this teammate, **auto-discover a design doc** for this PR:
1. Check the PR description for a link to `~/figma/dtsung/designs/` or `~/figma/dtsung/documents/` or a filename like `YYYYMMDD-*.md`.
2. If no link, `ls ~/figma/dtsung/designs/` and look for a filename that matches the PR branch name or title.
3. If still nothing, proceed without a design doc.

Pass the discovered path (or "NO_DESIGN_DOC_FOUND") to the teammate as `DESIGN_DOC_PATH`.

Spawn a teammate with this prompt:

> Review this PR for coherence — does the implementation actually accomplish what the PR claims to do, and does it avoid unnecessary work? Read the PR title and body (provided via `gh pr view`) and compare against the actual diff.
>
> `DESIGN_DOC_PATH`: {path or NO_DESIGN_DOC_FOUND}
>
> If `DESIGN_DOC_PATH` is a real path, read the design doc and verify:
> - **Task list coverage**: every task in the design's task list is either completed in the diff or explicitly noted as out-of-scope in the PR description
> - **Scope-audit coverage**: every dependent classified as "needs treatment" in the design's "Scope audit" section is actually handled in the diff. Dependents classified as "stays unchanged" must genuinely not appear in the diff, and dependents classified as "follow-up" must have a tracking link.
> - **Design decisions**: the implementation follows the decisions recorded in the design. If the diff deviates, flag it with the design reference.
>
> If `DESIGN_DOC_PATH` is `NO_DESIGN_DOC_FOUND`, note that in the output ("No design doc found in the usual places; checking PR body against diff only") and skip the design-vs-diff checks.
>
> Always look for:
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
> Output each finding with the file path relative to the working directory with line number (e.g., src/file.ts:123), what the issue is, and why it matters. For each finding, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If the PR is coherent, output "PR implementation is coherent with its stated goals."

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
> Output each risk with the file path relative to the working directory with line number (e.g., src/file.ts:123), the assumption being made, and what could go wrong if it's violated. For each risk, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If no significant risks, output "No hidden risks identified."

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
> Output any issues found with file paths relative to the working directory and line numbers (e.g., src/protocol.ts:45 and multiplayer/src/protocol.rs:78). For each issue, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If no issues, output "No multiplayer-specific issues found."

### Teammate 9: Bug Hunter Reviewer

Spawn a teammate with this prompt:

> You are a bug hunter reviewer. Your question is narrow: *"does this fail?"*. Look only at specific, mechanical patterns.
>
> **Dead wires:** Any new field on a struct that crosses a process or serialization boundary must have a populate path end-to-end. For each new struct field added in this PR:
> 1. Is there a proto / wire schema field that maps to it?
> 2. Is there a site that writes to it (producer side, deserializer, or translation)?
> 3. Is there a site that reads it (consumer)?
> Any empty or stub link in the chain is a finding. Common pattern: a sibling field is fully wired and the new one was added alongside without the proto change. Grep the codebase for where the sibling field flows and check the new field has matching wiring.
>
> **Peer-site drift:** When the PR modifies a reader of a shared structure (cache, registry, singleton, map), find all sibling readers in the codebase and diff them for pattern drift: nil guards, lock acquisition, error wrapping. If 2-of-3 readers have a guard and the third doesn't, *phrase the flag as a question* ("site C lacks the `!= nil` guard that sites A and B have; verify whether this is covered by a pre-condition") rather than asserting a bug. Downgrade the flag if the absent guard is documented in a code comment or guaranteed by a visible pre-condition. Legitimately different guards are common; false positives erode trust fast.
>
> **Lost invariants on deletion:** For every test or helper removed in the diff, extract the invariant it enforced (usually clear from the name: `AllFields`, `NoFieldForgotten`, `NoRace`, `NoAlias`) and verify that invariant is still enforced by something. If removing the test silently disables regression protection for a class of bugs, flag it. Suggest either restoring a smaller version or accepting the regression explicitly in the PR body.
>
> **Other classic bug patterns to flag:**
> - Unguarded dereferences on pointers that peers guard
> - Rollout-window races: reads sampled before `WaitForReady` / synchronization
> - Name collisions: new types whose unqualified name collides with existing types in sibling packages
> - Hot-path locks: if the PR replaces `atomic.*` / lock-free primitives with `sync.Mutex` / `sync.RWMutex` on paths called per-request, flag with the specific call sites
>
> Do NOT flag:
> - Pre-existing bugs not touched or worsened by this diff
> - Speculative issues without a concrete code citation
> - Issues covered by other existing tests visible in the diff
>
> Output each finding with file path and line number (e.g., `src/file.go:123`), the specific pattern, and why it's a bug. For each finding, include `**Recommendation**` (Fix/Follow-up/Ignore with a short reason phrase). If none, output "No bug patterns found."

### Teammate 10: Observability Reviewer

Spawn a teammate with this prompt:

> You are an observability reviewer. Your question is *"can we see what this is doing?"*. The checks are domain-specific: log levels, cardinality, context propagation, dashboard-partitionability, counter coverage.
>
> **New identifying dimensions:** If the PR introduces a data structure indexed by a new dimension (e.g., a map keyed by `WorkspaceID` that used to be a singleton, a per-tenant cache), that dimension is now the partition key of the new data model. Check that it appears in:
> - Log lines that touch the partitioned data (`slog.InfoContext(... "workspace_id", wsID)`)
> - Metric tags for counters/gauges on the touched code paths
> - Observability context / trace context that flows through RPC handlers
>
> If you can't partition the rollout dashboard by the new dimension, coverage is incomplete.
>
> **Log-level choice:** Skip paths and migration-window branches in production should not be at `DebugContext`. "Silent in prod" is a bug. Raise to `WarnContext` (or an explicit metric counter) for:
> - AfterSave-style skip branches (empty workload, workload not in registry)
> - Cache-miss branches that indicate state hasn't synced yet
> - Migration-window fallback branches
>
> **Context propagation:** `slog.InfoContext` / `slog.DebugContext` / `slog.WarnContext` / `slog.ErrorContext` wherever a `ctx` is in scope. Bare `slog.Info` etc. is a finding in any code path with a `ctx` available. (For sbox specifically, this is mandated by `services/agentplat/sbox/CLAUDE.md`; similar conventions exist elsewhere.)
>
> **Counter / gauge coverage on state transitions:** Flag the common gaps:
> - Cache-miss indistinguishable from `false`: if the code returns `false` on missing cache entry without a counter tagged `{hit, miss}`, operators can't tell "feature disabled" from "state not populated yet"
> - Rollout-progress counters: when a PR migrates A → B, is there a counter that tracks the mix of A vs B so the rollout dashboard is queryable?
> - Skip-outcome distribution values: if a code path has success/failure/timeout outcomes, missing a `skipped` or `no_op` value hides important states
>
> **Lost telemetry on deletion:** Analogous to lost invariants, but applied to logs/metrics. If the diff removes a log line or metric emission without a replacement, flag it as "removed observability, no replacement visible."
>
> Do NOT flag:
> - Pre-existing observability gaps unrelated to changed code
> - Debug-level logs on truly cold paths (once-at-boot init)
> - Cardinality explosions — if the new dimension has bounded cardinality (e.g., dozens of workspaces), it's fine
>
> Output each finding with file path and line number, the observability gap, and what to add. For each finding, include `**Recommendation**` (Fix/Follow-up/Ignore with a short reason phrase). If none, output "Observability coverage is adequate."

### Teammate 11: Component-Test-Value Reviewer (only if sbox PR)

**Only spawn this teammate if the PR touches `services/agentplat/sbox/` code.**

Spawn a teammate with this prompt:

> You are a component-test-value reviewer for sbox changes. Your question is *"is behavior proven to be tested, not just asserted?"*. You do NOT run tests inline — the checks are read-only and use only the diff and source. Expensive mutation-testing work is owned by `/darren:sbox-component-test-scan` and runs separately.
>
> **Identify touched behavior surfaces:** RPC handlers, mappers, session manager state transitions, workspace manager state transitions, bootstrap wiring, gate closures, after-save hooks, foundry sync proxy paths. Skip cosmetic-only changes (renames, comment updates, metric name adjustments).
>
> **Check for component-test coverage:** Does a test under `services/agentplat/sbox/sboxd/componenttest/` (or the nearest equivalent) exercise each touched surface end-to-end through the JSON-RPC surface? Component tests drive behavior through the public JSON-RPC surface, which makes them refactor-resilient and exercise cross-layer wiring that mapper/unit tests miss.
>
> **Distinguish pre-existing gaps:** Before flagging a coverage gap, run `git log origin/master..HEAD -- <path>` on each touched surface. If the file was changed cosmetically (rename, comment, metric name), the behavior was already present on master without coverage; classify the finding as "pre-existing coverage gap, not introduced by this PR" and do not file it as a blocker for this PR.
>
> **If coverage exists, judge whether it provides value by reading the test code:**
> - *Does it exercise real production code paths, or just read back state it wrote?* The tautological pattern: a test-only setter writes to the same map a test-only getter reads from, and neither touches the production gate closures. Flag tests that only touch the map they just wrote.
> - *Does it drive behavior through the public JSON-RPC surface?* Tests that import internals and bypass the RPC surface are unit tests in component-test clothing; they lose the cross-layer wiring guarantee.
> - *Would a plausible regression fail this test?* Mentally inject a realistic mutation into the production code the test is meant to cover: invert a condition, skip a state transition, return the default instead of the cached value. Walk through the test assertions: would any fail? If not, the test is not load-bearing.
> - *Does it duplicate an existing unit test of the same scenario?* If a mapper unit test already covers the exact NDJSON frames the component test feeds through the RPC surface, the component test's value is in the cross-layer wiring (e.g., agent_type → mapper-instance switch) that unit tests don't exercise. Call that out, or downgrade the component test.
>
> Do NOT flag:
> - Tests for cosmetic-only changes
> - Pre-existing tautological tests not added by this PR
> - Missing coverage on surfaces that already had no coverage on master (file those as Follow-ups, not blockers)
>
> Output each finding with file and line, specific reason (e.g., "reads back what it wrote via `bootstrapCache[wsID]`, never exercises the gate closure in `session.Manager`"). For each finding, include `**Recommendation**` (Fix/Follow-up/Ignore with a short reason phrase). If coverage is valuable and present, output "Component-test coverage is valuable."

### Teammate 12: Pattern Reviewer

Spawn a teammate with this prompt:

> You are a pattern reviewer. Your job is mechanical: scan the diff for matches against the curated bug patterns below. Unlike role-based reviewers (Correctness, Risk, Bug Hunter, etc.) who exercise judgment about what *could* go wrong, you match shapes. If a pattern shape fits a site in the diff, flag it. If it doesn't fit, don't.
>
> The patterns below are the kinds of bugs that humans and role-based reviewers reliably miss because they look obvious only in retrospect. Each pattern earned its spot by appearing as a real bug missed by prior review passes (e.g., caught later by Cursor Bugbot, surfaced post-merge, or fixed in a follow-up PR). Treat each as a checklist item: walk the diff, ask "does this pattern apply here?", flag every match.
>
> **False negatives (missed matches) are the failure mode to avoid.** False positives are also bad — only flag if the shape literally fits and the cited code is in the diff.
>
> ## Patterns
>
> 1. **Returned-resource lifecycle.** A function returns a resource handle (`*websocket.Conn`, `*os.File`, `net.Conn`, `*sql.Rows`, a lock token, an `io.Closer`). Every caller of that function must release the resource on every exit path. Walk every call site in the diff: does the caller's defer/return close it? Pay special attention when the *signature changed* in this PR to start returning a resource — old call sites that ignored the return value will leak. Sibling functions that share the same lifecycle (e.g., `Close()` and a `readPump`/`writePump` both calling the same teardown helper) should mirror each other; flag when one closes the returned handle and another discards it.
>
> 2. **Multi-keyed iteration produces duplicates.** When the diff iterates a structure where the same value is reachable under multiple keys (`map[K1]map[K2]V` where V is registered under multiple K1 values, a slice indexed by tag with shared entries, a fan-out registry) and collects values to process them, the collected list contains duplicates. Flag if the downstream processing is non-idempotent: closing a channel twice panics; calling `sub.close()` N times wastes work and skews counts; emitting a metric per element over-counts. The fix is dedup-by-identity before the loop.
>
> 3. **Bare `slog.*` in `ctx`-scope.** Every `slog.Info`/`Warn`/`Error`/`Debug` call where a `ctx context.Context` is in scope (function parameter, struct field, captured by closure) should be the `*Context` variant: `slog.InfoContext(ctx, ...)`. Bare `slog.Info(...)` in ctx-scope drops trace correlation. Flag every site. Project CLAUDE.md may mandate this explicitly (e.g., `services/agentplat/sbox/CLAUDE.md`).
>
> 4. **Defer assumes invariants that don't hold on every exit.** A `defer X()` added or modified in this PR fires on every function exit, including early returns, panics, and the natural exit path. Walk every exit path: does the invariant the defer assumes hold there too? Common failure: a defer that flips state (`defer func() { c.connected = false }()`) is correct on the natural exit but wrong on transport-eviction exit where the underlying connection is still alive. The fix is to gate the defer's action on the actual condition (`if c.agent == nil { c.connected = false }`).
>
> 5. **One-line fix not mirrored at peer call sites.** When the diff modifies a single defer/close/lock-acquisition/error-handling pattern in one function, find peer functions that share the same lifecycle. If `Close()` adds `if conn != nil { conn.Close() }` to its defer but `readPump`'s defer still discards `disconnect()`'s returned conn, the fix is incomplete. Grep for callers of the same helper; flag every peer that should mirror the change but doesn't.
>
> 6. **Channel send/close after consumer can drop out.** A subscription buffer with eviction-on-overflow, a watcher whose underlying channel can be closed by the dispatcher's defer, a context-cancellation that closes a channel — any path where the consumer's channel can be closed/nilled while a producer still holds a reference. Flag sends to such channels that lack a guard (closed-channel panic, send on nil blocks forever). Flag readers that re-fetch the channel field after consumer-drop and treat nil as "still connected."
>
> 7. **Test fix doesn't actually verify the new invariant.** A test added or modified in this PR claims to assert behavior X (per its name or comment), but the assertions only check a precondition or a downstream side effect. Examples: `TestSubscribeSelfUnsubFromConsumerLoop` waits for the first message but never asserts `unsub()` returns; `TestNoLeak` counts goroutines but doesn't actually exercise the leak path; `TestRejectsAfterClose` calls `Close()` then asserts no error on a method that doesn't return errors. Mentally inject a regression matching the test's name: would any assertion fail? If not, flag the test as not load-bearing for its stated invariant.
>
> 8. **Doc fix inverts the contract.** A doc comment added or rewritten in this PR (especially lock-discipline, ownership, or "must be called with X held"). Read the function body and call sites: does the doc match the actual behavior? Common failure mode in iterative review: a prior reviewer flagged "this doc is stale" and the fix rewrote the doc *the wrong direction* — now it's wrong in a different way, and four reviewers will independently flag it on the next pass. Verify lock state at every caller of the documented function.
>
> Do NOT flag:
> - Patterns that match code outside the diff (pre-existing).
> - Patterns where the cited site is exempted by an explicit comment or pre-condition (e.g., a comment says "channel is guaranteed non-nil here by the caller").
> - Patterns where the "match" requires speculative reasoning about what could happen — only flag if the shape literally fits.
>
> Output each match with file:line, the pattern number/name, and a one-sentence explanation of why this site matches. For each finding, include `**Recommendation**` — usually `Fix` for a real pattern match (these are concrete bugs by construction). If none, output "No pattern matches."

<!--
Curation: this pattern list is mutable. New patterns earn a spot when (a) a
Bugbot/postmortem/external-review finding catches something the role-based
reviewers missed, AND (b) the bug shape generalizes beyond the one site.
Transcribe the shape verbatim from the source incident; include enough
detail that the reviewer can mechanically match it without inferring intent.
Patterns retire when a linter or static-analysis check reliably catches them
(at which point the lint becomes the enforcement, not this list).
-->

### Teammate 13: Go Style Reviewer (only if Go PR)

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
> Output each finding with the file path relative to the working directory with line number (e.g., go/common/metrics/client.go:45), the pattern violated, and a brief fix suggestion. For each finding, also include a **Recommendation** with brief reasoning in the format `**Fix** — <reason>`, `**Follow-up** — <reason>`, or `**Ignore** — <reason>` (Fix = address in this PR; Follow-up = real issue, out of scope; Ignore = not worth changing). The reason is a short phrase that explains why this recommendation fits. If no issues, output "Go code follows style guide."

---

## Phase 3: Write Review File

Wait for all teammates to complete their reviews. Then synthesize their findings into a structured markdown file.

**Cross-reference with PR comments:** Before writing each finding, check if it was already acknowledged or discussed in the PR comments you fetched in Phase 1. If the author or a reviewer already called out the issue, append "(Acknowledged by author in PR comments)" to the description.

**Consolidate duplicates:** If multiple teammates flag the same issue, write it once.

**Use reviewer recommendations:** Each reviewer includes a **Recommendation** (Fix, Follow-up, or Ignore, with reasoning) for each of their findings. Carry that recommendation into the output. If multiple reviewers flag the same issue with different recommendations, use your judgment to pick the most appropriate one.

**Order findings** within the `## Findings` section by recommendation: Fix first, then Follow-up, then Ignore. This makes the most actionable items easiest to scan.

Write `review.md` to the worktree root with this structure:

```markdown
# Code Review: PR #<number> - <title>

> <1-3 sentence summary of the PR>

## Summary

| # | Finding | Recommendation | Outcome |
|---|---------|----------------|---------|
| 1 | ◯ <short description> | Fix | N/A |
| 2 | ◯ ... | Follow-up | N/A |
| 3 | ◯ ... | Ignore | N/A |

## Findings

### ◯ <Concise finding title> <!-- @actions: elaborate, fix, ignore -->

> **Recommendation:** Fix — <brief reasoning>
>
> **Outcome:** N/A
>
> [Correctness] `path/to/file.ts:123` - <description of the issue>

<br>

### ◯ <Another finding> <!-- @actions: elaborate, fix, ignore -->

> **Recommendation:** Follow-up — <brief reasoning>
>
> **Outcome:** N/A
>
> [Risks] `path/to/file.ts:456` - <description>

<br>

### ◯ <Finding title> <!-- @actions: elaborate, fix, ignore -->

> **Recommendation:** Ignore — <brief reasoning>
>
> **Outcome:** N/A
>
> [Coherence] `path/to/file.ts:101` - <description>
```

Rules:
- All findings go under a single `## Findings` heading, ordered Fix → Follow-up → Ignore.
- Each finding's block quote starts with `**Recommendation:** <Fix|Follow-up|Ignore> — <brief reasoning>`, followed by `**Outcome:** N/A`, followed by `[Category] \`file:line\` - description`.
- The reasoning should be a short phrase the reader can scan to understand *why* this recommendation fits (e.g., "logic error, will corrupt data", "minor naming preference", "out of scope for this PR").
- The finding body is a single block quote containing all three lines (Recommendation, Outcome, description).
- Every `###` finding title and its corresponding summary table row start with a status emoji: ◯ (no outcome), 🟣 (in progress/elaborate), 🟡 (ignored), 🟢 (done/fixed). Initially all are ◯.
- Add `<br>` between findings (between the end of one block quote and the next `###` heading).
- The `<!-- @actions: elaborate, fix, ignore -->` comment goes on the `###` heading line itself.
- The summary table includes a `Recommendation` column (just "Fix", "Follow-up", or "Ignore" — no reasoning).

After writing the file, tell the user: "Review written to `review.md`."
