# Storyflow Agent Guide

This file is the repository-level operating contract for Codex, Claude Code, Cursor and other coding agents. Follow it together with the relevant role contract under `agents/`, the story specification and the issue acceptance criteria.

## 1. Mission and source of truth

- Treat [README.md](README.md) as the current product/CLI overview.
- Treat [docs/ROADMAP.md](docs/ROADMAP.md) as the narrative roadmap, [TODO.md](TODO.md) as the quick checklist and [GitHub roadmap issue #2](https://github.com/nhcminh47/storyflow/issues/2) as the canonical planning tracker.
- Treat the linked GitHub implementation issue as the acceptance contract for the task being implemented. If docs and issue scope conflict, stop and ask rather than silently broadening scope.
- Before coding, inspect the relevant Rust modules, tests, current branch state and recent changes. Do not infer implementation from roadmap prose alone.

## 2. Architecture invariants

1. **Rust is authoritative** for task/story state, revisions, DAG readiness, leases, capability enforcement, candidate identity, gate verdicts and audit.
2. Hosts and agents are execution adapters. They may propose work/results but must not mutate authoritative state or bypass Rust validation.
3. Markdown instructions are advisory contracts, not a security boundary. Enforced permissions must live in the managed runtime.
4. Every run is pinned to its approved Git baseline and config/agent snapshot. Do not silently rebind an active run after config changes.
5. Every repository owns its own baseline, candidate, gates and audit. Cross-repo orchestration must not conflate commits or verdicts.
6. Never auto-merge, auto-push, silently resolve code conflicts or weaken mandatory checks.
7. A code knowledge graph (e.g. Graphify) is optional advisory context. It cannot grant permissions, prove tests passed or satisfy a mandatory gate. Bind graph data to repository + exact commit + provider/version + provenance + source locations; reject stale data for candidate validation.
8. Never put secrets, credentials, private user data or raw prompts into committed Markdown, reports, journals, logs or analytics.

## 3. Before implementation

1. Confirm the requested issue, milestone, priority, dependencies and acceptance criteria.
2. Check the working tree and branch. Do not overwrite or discard pre-existing user changes.
3. Read the relevant contracts:
   - `agents/orchestrator.md` for orchestration;
   - the matching worker/role contract for assigned work;
   - `agents/task-coordination.md` for parallel task ownership and integration;
   - `docs/task-dag.md` and `docs/task-scheduler.md` for task runtime behavior;
   - `docs/runtime-security.md` and `docs/run-snapshots.md` for security and run pinning.
4. Identify exact files/modules you own, files owned by parallel agents and integration points. If ownership overlaps, resolve it before dispatch.
5. Write a short implementation plan with design choices, risks, tests and explicit out-of-scope items.
6. Do not start work whose dependencies are unsatisfied. The current roadmap's next implementation priority is #10, followed by #11 then #12.

## 4. Branches, worktrees and parallel agents

- Use a focused topic branch targeting `master`. Follow the existing `story/<id>` convention for story worktrees; for repository maintenance use a descriptive topic branch such as `feat/<issue>-<slug>`, `fix/<issue>-<slug>` or `docs/<slug>`.
- For parallel implementation tasks, create a separate Git worktree per worker from the approved baseline. Never let concurrent workers write to the same worktree.
- Before dispatch, define for each task: task ID, objective, dependencies, base commit, read scope, write scope, acceptance criteria, expected artifacts and owner.
- Prefer parallelism only for independent tasks with non-overlapping write scopes. Keep integration and final candidate ownership with the orchestrator.
- Verify each worker has exited before integrating its commit. A scheduler lease expiry or heartbeat timeout is not proof that the process has stopped.
- Integrate in dependency order. Validate ancestry and changed paths against task scope. On overlap, stale base or conflict, stop and report exact files; do not auto-resolve.
- Preserve input refs and integration evidence. Gate/review the exact final pinned candidate, not a moving branch tip.

## 5. Host and tool behavior

- Use only documented/supported capabilities of the installed host. Do not assume a CLI flag, subagent API, model selector or cancellation feature exists.
- Use a fake adapter for automated tests. Unit/integration tests must not require a cloud model, API key or interactive host.
- Host results are untrusted input: validate attempt/session IDs, task IDs, revisions, schemas and idempotency before accepting them.
- If the host is unavailable, a worker crashes, cancellation is uncertain or a result is stale/duplicated, record an explicit recoverable/blocked outcome. Never fabricate successful execution.
- Do not execute project-provided gate commands until you have inspected them. They run with the caller's permissions unless a supported managed sandbox says otherwise; a Git worktree is not a sandbox.

## 6. Context and evidence

- Build context from the pinned story, task dependencies, read/write scope, acceptance criteria and required evidence. Avoid copying entire chat histories or unrelated repository files.
- Keep packets and reports deterministic and size-bounded. Include IDs, revisions, commit hashes, artifact hashes, provenance and actionable blockers.
- Treat missing/corrupt required artifacts as a stop/re-gather condition. Do not silently omit required evidence.
- Redact secrets and sensitive values. Keep full logs on disk; include only bounded relevant tails in handoffs.
- If a code graph provider is configured, include only task-relevant slices and record provider identity/version, repository, exact source revision, provenance (extracted/inferred/ambiguous) and source locations. Mark stale graph data and exclude it from required evidence. Normal operation must work when the provider is absent.

## 7. Implementation and testing

- Keep PRs small and scoped to one issue. Do not mix unrelated refactors or future roadmap features into the current task.
- Preserve public CLI behavior and existing v0.1 compatibility unless the issue explicitly authorizes a versioned change.
- Add/update tests for success, invalid input, stale revision, duplicate/replay, failure, recovery and security boundaries as applicable.
- Prefer deterministic fixtures and simulated clocks/workers over timing-sensitive tests.
- Run focused tests first, then the relevant regression suite. For Rust changes, start with:
  ```sh
  cargo fmt --check
  cargo test --locked
  ```
- Run repository acceptance scripts when relevant and supported by the environment:
  ```sh
  pwsh -NoProfile -File examples/slug/demo.ps1
  pwsh -NoProfile -File examples/matrix/run.ps1
  ```
- If a command cannot run, report the exact command, environment limitation and remaining validation. Never claim tests passed without observed output.

## 8. GitHub issue and PR workflow

- Reference the implementation issue in the PR title/body and explain how every acceptance criterion is met.
- Include changed contracts/APIs, migration/backwards-compatibility notes, test evidence and known limitations.
- Keep issue checkboxes/TODO/roadmap status accurate. Do not mark work complete based only on code generation; require passing tests, review and merge evidence.
- Do not close issues or merge PRs unless explicitly requested and all required checks/reviews pass.
- Human approval is required for scope changes, destructive actions, conflict resolution, release and acceptance.

## 9. Completion report

At the end of a task, report:

1. What changed and why.
2. Files/modules changed.
3. Tests run and their observed results.
4. Acceptance criteria met, unmet or blocked.
5. Risks, follow-up issues and any human decision required.
6. Branch/commit/PR references.

If blocked, stop at the boundary, preserve the worktree and evidence, and state the smallest decision needed to continue.
