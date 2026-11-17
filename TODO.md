# Storyflow TODO

> Issue tracker mirror for quick repository-level planning. GitHub issues are the execution source of truth; see [GitHub roadmap #2](https://github.com/nhcminh47/storyflow/issues/2) and [docs/ROADMAP.md](docs/ROADMAP.md).
>
> Status is intentionally based on the reviewed roadmap/merged PRs, not the GitHub issue state field alone. Issues #3–#9 remain open in GitHub but their implementation is marked complete in the roadmap.

## Current priority

- [ ] **Next: #10 — Parallel worker commit integration.** Complete M2 by integrating isolated worker commits into one pinned candidate.
- [ ] **Then: #11 — Host adapter protocol + Codex adapter.**
- [ ] **Then: #12 — Structured context, evidence handoff + optional code graph provider.**
- [ ] Refine #15 workflow stage/role model in parallel only when file ownership and implementation scope do not conflict with #10/#11.
- [ ] Keep #13 optional; do not make it a prerequisite for M4.
- [ ] Defer standalone evidence graph and metrics issues until #10–#12 contracts stabilize.

## M1 — Configuration foundation

- [x] [#3 — User / Workspace / Repo configuration schema and migration contract](https://github.com/nhcminh47/storyflow/issues/3) — P0, config. Schema, scope resolution and compatibility contract.
- [x] [#4 — Deterministic Rust effective-config resolver with provenance](https://github.com/nhcminh47/storyflow/issues/4) — P0, config. Resolve trusted scopes and produce typed effective config.
- [x] [#5 — Policy floors and per-run config/agent snapshots](https://github.com/nhcminh47/storyflow/issues/5) — P0, security. Fail-closed policy composition and reproducible run snapshots.
- [x] [#6 — Scope bootstrap and effective-config inspect/validate CLI](https://github.com/nhcminh47/storyflow/issues/6) — P1, CLI. Non-destructive bootstrap and validation while preserving legacy story-run init.

**Exit:** M1 implementation is complete. Preserve regression coverage and backwards compatibility.

## M2 — Task execution and runtime safety

- [x] [#7 — Typed Task DAG and durable per-task journal](https://github.com/nhcminh47/storyflow/issues/7) — P0, runtime. Merged in PR [#23](https://github.com/nhcminh47/storyflow/pull/23).
- [x] [#8 — Scheduler, dispatch leases, retries and crash recovery](https://github.com/nhcminh47/storyflow/issues/8) — P0, runtime. Merged in PR [#24](https://github.com/nhcminh47/storyflow/pull/24).
- [x] [#9 — Bounded tool capabilities and quality-gate isolation](https://github.com/nhcminh47/storyflow/issues/9) — P0, security. Merged in PR [#25](https://github.com/nhcminh47/storyflow/pull/25).
- [ ] [#10 — Safe integration of parallel worker commits into one story candidate](https://github.com/nhcminh47/storyflow/issues/10) — P1, git. **Next implementation priority.**

**#10 acceptance focus:** isolated worktrees and per-task write scopes; validate ancestry and changed paths; integrate in dependency order only after worker exit; block and report conflicts without automatic resolution; pin one candidate for gate/review; make recovery idempotent; preserve the single-worker path.

**Exit:** deterministic DAG/scheduler/recovery, enforced capability denials and safe multi-worker candidate integration. No gate bypass or silent conflict resolution.

## M3 — Host integration and context

- [ ] [#11 — Host adapter protocol and first Codex adapter](https://github.com/nhcminh47/storyflow/issues/11) — P0, adapters. Provider-neutral dispatch/result/cancel/recovery contract; fake adapter for offline CI; Codex smoke path.
- [ ] [#12 — Structured context packets, evidence index and cross-session handoff](https://github.com/nhcminh47/storyflow/issues/12) — P1, context. Deterministic bounded packets, artifact integrity, resume/reconcile, secret redaction and optional Code Graph Context Provider.
  - Graphify is the first evaluation/reference implementation, not a mandatory dependency.
  - Graph data is advisory context only; bind it to repository + exact source commit + provider/version + provenance + source locations.
  - Stale/inferred/ambiguous graph results cannot satisfy required evidence or gates. Keep code graph separate from Storyflow's execution/evidence journal.
- [ ] [#13 — Model tier routing and per-task usage/cost telemetry](https://github.com/nhcminh47/storyflow/issues/13) — P2, observability. Optional host hints and observed-only metering; unknown values remain unknown.

**Exit:** fake-host offline E2E, Codex smoke checklist, restart/resume preserving pinned evidence and permissions. Normal dispatch works without a code graph provider.

## M4 — Project delivery

- [ ] [#14 — Project → Feature → Story → Task coordination across repositories](https://github.com/nhcminh47/storyflow/issues/14) — P1, project. Explicit versioned cross-repo dependencies and independent per-repo baselines, candidates, gates and audits.
- [ ] [#15 — Declarative delivery workflow with discovery, UAT and review gates](https://github.com/nhcminh47/storyflow/issues/15) — P1, workflow. Versioned stages/roles, configurable BA/SA discovery, implementation, QA/security/review and UAT; mandatory gates cannot be skipped.

**Exit:** multi-repo fixture, repo-specific gates/audits, configurable workflows without weakening mandatory checks.

## Future — Evidence and engineering intelligence

- [ ] Split traceability/evidence graph work into focused issues after #10–#12 contracts stabilize.
- [ ] Link requirements, decisions, tasks, commits, tests, reviews and acceptance evidence.
- [ ] Optionally link evidence records to code-graph entities using repository + exact commit identity.
- [ ] Add workflow-stage role registry and explicit evidence requirements.
- [ ] Add delivery/quality metrics: lead/cycle time, throughput, blocked time, retries, rework, test effectiveness and gate outcomes.
- [ ] Add usage/cost reporting from observed host/provider data only, with provenance and explicit unknowns.
- [ ] Add advisory graph-assisted impact analysis and review context.
- [ ] Add legacy modernization workflow with extracted rule ↔ code ↔ regression-test traceability.

## Working rules

- Start only issues whose dependencies are satisfied.
- Keep each PR focused on one issue and reference that issue in the PR body.
- Use a topic branch and separate worktree for parallel tasks; declare file ownership before dispatch.
- Rust remains authoritative for task state, permissions, candidate identity and gates.
- Review test evidence independently. No auto-merge, auto-push or silent conflict resolution by default.
- Update this file and [docs/ROADMAP.md](docs/ROADMAP.md) when issue scope, dependency, priority or completion changes. Keep GitHub roadmap #2 synchronized.
