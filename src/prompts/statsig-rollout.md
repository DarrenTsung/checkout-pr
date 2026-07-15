Continue the rollout or investigation for the Statsig gate `{{GATE}}`.

Use {{STATSIG_SKILL}} for live gate state and audit history, and use {{DOCUMENT_SKILL}} as the workflow for all rollout-dossier work. Keep the durable dossier in the shared dtsung repository under `~/figma/dtsung/documents/`; do not create a rollout document in the flag worktree.

Before creating anything, search the shared documents for the exact gate key and identify an existing document whose primary purpose is tracking this gate's rollout. Prefer a document with the gate in its title and an existing rollout journal. If several documents mention the gate, reuse the canonical rollout dossier under the document skill's rules rather than modifying an incidental design or investigation. If no dossier exists, create one through {{DOCUMENT_SKILL}} and follow that skill's timestamped filename convention.

Read and preserve the dossier if it exists. Preserve human-written text and previous journal entries. Refresh facts that have changed, but append a journal entry only when there is materially new evidence, a decision, an action, or a change in rollout state. Do not create a duplicate dossier or duplicate journal entries on resume.

Build the dossier from verified evidence, not assumptions:

1. Query the exact gate, its current rules and rollout percentages, and its recent audit history.
2. Find every code evaluation site and identify the systems, consumers, workloads, and behaviors controlled by the gate.
3. Find the introducing pull request. Search the exact gate key in history (`git log -S` or equivalent), map the introducing commit to its GitHub pull request, and confirm in the diff that the PR actually introduced the gate or its guarded behavior. If multiple PRs materially define the rollout, include each one and distinguish its role.
4. Read the PR title, body, diff, linked documents, relevant comments, and relevant review threads. Capture the questions the author or reviewers raised about correctness, risk, observability, deployment, rollout, and cleanup. Do not treat unrelated review discussion as rollout guidance.
5. Read `~/figma/dtsung/templates/pr-body-template.md` if it exists. Use its active-flag, metrics-and-logs, testing, and rollout questions as a checklist. Also inspect the implementation and tests so the dossier is useful even when the original PR body is incomplete.
6. Resolve exact metric names, log messages or fields, queries, dashboards, flag-on session identifiers, consumers, healthy expectations, failure thresholds, rollback triggers, and known fallback paths wherever the evidence supports them. Clearly mark missing answers as open questions; never invent a signal or threshold.

Keep the document comprehensive but high-level enough that a cold reader can operate the rollout. Use this structure, omitting empty boilerplate but retaining explicit open questions:

# `{{GATE}}` rollout

## At a glance

- Statsig gate and link
- Current live state, environments, rules, and percentages
- Owner or decision-maker, when verified
- Introducing and materially related pull requests
- Current rollout phase and next human action
- Last verified time in Pacific Time with a `PT` suffix

## What this flag accomplishes

Explain the user or system outcome, why the flag exists, the guarded old and new paths, and the intended end state. Prefer intent and operational consequences over a file-by-file change summary.

## Runtime behavior and scope

For each system or evaluation site, record the effect when on, consumers and workloads, targeting or eligibility, fallback behavior, dependencies, deployment ordering, and rollback nuance.

## Introducing change and decisions

Summarize the introducing pull request and any related design context, tests, unresolved questions, explicit non-goals, reviewer concerns, and decisions that matter during rollout. Link sources directly.

## Metrics, logs, and go/no-go criteria

Organize this around operational questions:

- Is the flagged behavior doing what it should?
- What is the worst-case failure and which exact signal detects it?
- How do we know the intended path ran rather than a fallback masking failure?
- How do we identify sessions or requests where the flag evaluated on?
- Which consumers and workloads must be sampled independently?
- What constitutes healthy, pause, rollback, and success?

For each answer, include the exact signal or query, scope/filter, expected behavior, threshold when one is documented, and evidence source. Keep unknown thresholds visible as open questions.

## Rollout plan

Write ordered rollout beats with prerequisites, target environment or percentage, verification steps, observation window when known, go/no-go criteria, rollback action, and owner. Cover deployment and baseline verification, non-production validation, production ramp stages, and post-rollout cleanup when applicable. Reflect the live state rather than copying a stale generic plan.

## Open questions and risks

List only unresolved items that could change a rollout decision, validation strategy, or cleanup plan. Include the best source or person to resolve each one when known.

## Rollout journal

Append dated entries using `YYYY-MM-DD HH:MM PT`. Each entry should compactly record the live state checked, material evidence, decision or action, and result. This is a factual operating log, not a repeated summary of the whole dossier.

Do not change the gate, post to GitHub, or make another external mutation unless the human explicitly asks. The document may recommend the next action, but rollout decisions remain human-controlled.
