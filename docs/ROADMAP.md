# Storyflow Roadmap

> **Canonical execution tracker:** [GitHub Issue #2](https://github.com/nhcminh47/storyflow/issues/2). This document is the repository-local narrative roadmap. Keep it synchronized with issue #2 and [TODO.md](../TODO.md); GitHub implementation issues remain the detailed acceptance contracts.

## Product goal

Evolve Storyflow from a v0.1 single-story Git workflow into a reusable, deterministic AI-native software-delivery orchestrator with:

- scoped configuration and trust;
- durable task execution, scheduling and recovery;
- safe integration of parallel workers into a pinned Git candidate;
- host/provider adapters;
- compact context and auditable evidence handoff;
- configurable SDLC workflows and multi-repository project coordination;
- future evidence-backed engineering intelligence.

Storyflow's durable product assets are workflow definitions, state transitions, permissions, Git candidate lifecycle and evidence. Agents, hosts and intelligence providers are replaceable.

## Reference models and product boundaries

| Reference | Learn from it | Do not copy / do not delegate |
| --- | --- | --- |
| Agent Team IDE | Execution environment, host/provider selection, multi-agent sessions and explicit action approval. | Do not move authoritative state or policy enforcement into the host UI. |
| DevAgents / DevAgents OS | SDLC specialization, shared context, traceable artifacts, governance, metrics and continuous improvement. | Do not hard-code every role as a fixed persona; model roles as configurable workflow capabilities. |
| [Graphify](https://github.com/Graphify-Labs/graphify) | Codebase intelligence: symbols, imports/calls/dependencies, source-grounded context and impact exploration through a code knowledge graph. | Do not require Graphify at runtime; do not treat graph output as permission, test result, gate verdict or execution truth. |

### Two linked but separate knowledge domains

1. **Code knowledge graph** describes source structure: files, symbols, calls, imports, dependencies and inferred relationships.
2. **Storyflow execution/evidence journal** describes delivery truth: runs, tasks, revisions, leases, approvals, commits, gates, tests, reviews and acceptance evidence.

Link these domains through repository identity, exact source commit, task IDs and artifact references. Do not merge their authority.

### Core architecture principles

1. **Orchestration ≠ execution.** Rust owns authoritative state transitions and enforcement. Hosts/providers dispatch work and return typed results.
2. **Roles are capabilities in a workflow.** BA/PO/SA/Developer/Reviewer/QA/Security/DevOps roles are assigned to versioned stages/tasks, not permanently embedded as personas.
3. **Traceability is first-class.** Preserve requirement → decision/design → task → commit/artifact → test → review → UAT/acceptance evidence.
4. **Human approval for consequential decisions.** Risky operations, scope changes, conflict resolution, release and acceptance require explicit authorization.
5. **Observed metrics only.** Record known duration, retries, rework, test outcomes and host-reported usage/cost with provenance. Unknown stays unknown.
6. **Code graph is advisory.** Every graph result used in context must identify repository, exact revision, provider/version, provenance and source location. Stale graph data cannot validate a candidate.
7. **Optional intelligence providers.** Missing/unavailable Graphify or alternative provider must not prevent normal Storyflow operation unless a workflow explicitly requires it.
8. **Legacy modernization comes later.** Build rule/code/test traceability only after evidence contracts stabilize.

## Milestones

### M1 — Configuration foundation <span>Complete</span>

**Scope:** User → explicitly trusted Workspace → Repo config resolution, schema, policy floors, provenance and per-run snapshots; non-destructive CLI bootstrap and inspect/validate.

**Issues:** #3, #4, #5, #6.

**Exit:** deterministic scope resolution, fail-closed policy composition, snapshot/replay and legacy CLI compatibility. Implementation is marked complete in GitHub roadmap #2.

### M2 — Task execution and runtime safety <span>In progress</span>

**Scope:** typed task DAG, durable journal, scheduler, leases/retries/recovery, managed capabilities, isolated workers and deterministic Git candidate integration.

| Issue | Priority | Scope | Status |
| --- | --- | --- | --- |
| [#7](https://github.com/nhcminh47/storyflow/issues/7) | P0 | Typed Task DAG and durable task journal | Complete — PR #23 |
| [#8](https://github.com/nhcminh47/storyflow/issues/8) | P0 | Scheduler, leases, retries and crash recovery | Complete — PR #24 |
| [#9](https://github.com/nhcminh47/storyflow/issues/9) | P0 | Managed capabilities and gate lifecycle hardening | Complete — PR #25 |
| [#10](https://github.com/nhcminh47/storyflow/issues/10) | P1 | Integrate parallel worker commits into one pinned candidate | **Next** |

**Exit gate:** task readiness and recovery deterministic; capability denials enforced; worker exit verified before integration; conflict blocks with precise evidence; one pinned candidate is used for gate/review; no silent auto-resolution.

### M3 — Host integration and context <span>Planned</span>

| Issue | Priority | Scope |
| --- | --- | --- |
| [#11](https://github.com/nhcminh47/storyflow/issues/11) | P0 | Host adapter protocol and first Codex adapter; fake-host CI path |
| [#12](https://github.com/nhcminh47/storyflow/issues/12) | P1 | Structured context packets, evidence index, cross-session handoff and optional Code Graph Context Provider |
| [#13](https://github.com/nhcminh47/storyflow/issues/13) | P2 | Optional model tier hints and observed usage/cost telemetry |

**Code graph scope in #12:**

- Define a provider-neutral interface; Graphify is the first reference/evaluation target.
- Return task-relevant symbols, callers, imports, dependencies and impact hints within deterministic size budgets.
- Record provider identity/version, repository, exact source commit, provenance (extracted/inferred/ambiguous) and file/line locations.
- Exclude or mark stale results whose revision differs from the pinned run/task revision.
- Never let graph output authorize operations, satisfy a required gate, prove tests passed or replace the Storyflow journal.
- Keep provider optional and replaceable; no Graphify-specific behavior in the Rust core.

**Exit gate:** fake-host offline E2E, real Codex smoke checklist, restart/resume preserves pinned evidence and permissions, bounded deterministic context packets, stale graph fixtures rejected, and operation remains valid without Graphify.

### M4 — Project delivery <span>Planned</span>

| Issue | Priority | Scope |
| --- | --- | --- |
| [#14](https://github.com/nhcminh47/storyflow/issues/14) | P1 | Project → Feature → Story → Task coordination across repositories |
| [#15](https://github.com/nhcminh47/storyflow/issues/15) | P1 | Declarative SDLC workflow: intake/BA, research/SA, implementation, validation, review, UAT and delivery |

**Exit gate:** multi-repo fixtures prove explicit dependencies and independent repository baselines/candidates/gates/audits; workflow definitions are versioned and configurable without weakening mandatory gates.

### Future — Evidence and engineering intelligence

Do not start implementation until #10–#12 establish stable task, adapter and evidence contracts. Split each capability into focused issues:

- Evidence graph across requirements, decisions, tasks, commits, tests, reviews and acceptance.
- Optional links from evidence nodes to code-graph entities at exact repository revisions.
- Workflow role registry and per-stage evidence requirements.
- Delivery/quality metrics: lead time, cycle time, throughput, blocked time, retries, rework, test effectiveness and gate outcomes.
- Usage/cost reporting based only on observed host/provider data.
- Advisory graph-assisted impact analysis and review context.
- Legacy modernization workflow linking extracted rules ↔ source code ↔ regression tests.

## Execution sequence and parallelism

1. **#10 first:** finish safe candidate integration on top of the completed DAG/scheduler/capability foundations.
2. **#11 second:** establish the host contract and fake adapter before binding to real host behavior.
3. **#12 third:** build deterministic handoff/evidence packets; integrate code graph behind an optional provider interface.
4. **#15 may be refined in parallel** only if it does not compete for runtime/adapter files or expand #10/#11 scope. Its implementation follows the runtime foundations.
5. **#13 stays optional** and must not block M4.
6. **#14 follows** host/task contracts; keep repo-local execution state independent.

For parallel implementation, use separate worktrees, explicit file ownership, dependency-aware task dispatch and independent acceptance review. Never assume a lease expiry proves a worker has exited.

## Non-negotiable invariants

- Preserve existing CLI compatibility: `storyflow init ID [BASE]`, `next`, `submit`, `gate`, `audit`.
- Rust-side deterministic gates remain authoritative; LLM/host approval cannot replace them.
- User/workspace settings cannot silently weaken protected paths, mandatory checks or tool permissions.
- Each run remains pinned to its approved baseline/config/agent snapshot. Parent config changes do not silently mutate active runs.
- Every story/repository owns its baseline, candidate, gate and audit. Cross-repo orchestration does not create a shared Git commit or shared verdict.
- Host/provider capabilities must be detected/declared and enforced, not inferred from marketing documentation.
- No auto-merge, auto-push or silent conflict resolution by default.
- Code graph data is revision-bound advisory context, never authorization or gate evidence.
- Secrets do not enter Markdown contracts, context packets, journals or analytics.

## Definition of done

Every implementation PR:

- references one focused GitHub issue;
- states scope, dependencies, compatibility impact and changed contracts;
- includes tests and reproducible evidence;
- updates relevant docs and TODO tracking;
- passes independent review and deterministic gates.

A feature is not complete merely because an agent reports success. Do not auto-merge by default.
