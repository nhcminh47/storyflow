# Parallel worker commit integration

`storyflow::integration` (`IntegrationCoordinator`) and the `storyflow-tasks` companion CLI integrate completed parallel code tasks into one pinned story candidate for `storyflow submit ID REV candidate REPORT.md COMMIT`, `storyflow gate` and `storyflow review`. Parallel commit integration requires **Git 2.38+** for in-memory `git merge-tree --write-tree` (enforced fail-closed with `git_version_unsupported`).

## Commands

```text
storyflow-tasks worktree ID REV TASK
storyflow-tasks worktree-remove ID REV TASK
storyflow-tasks integrate ID REV
storyflow-tasks integration-status ID
```

`REV` is the latest **task ledger** revision (`view.revision` or `status.ledger_revision`). `worktree`, `worktree-remove` and `integration-status` exit 0 on success and 2 on rejected input, stale revision, lock contention or operational error. `integrate` exits **0** when integration produces a `ready` candidate, **1** when integration records a `blocked` outcome with auditable blockers, and **2** on stale revision or operational error.

| Command | Input and result |
| --- | --- |
| `worktree` | Provisions an isolated detached Git worktree under `.storyflow/<id>/worktrees/` for a `running` task at its required base commit, pins `refs/storyflow/<worktree>/<id>/workers/g<graph>/<task>/r<rev>`, and returns `WorkerWorktree`. Idempotent if called again for the same running task revision. |
| `worktree-remove` | Removes a provisioned worker worktree after confirming the task is no longer running and all worker attempts for that task have `worker_stopped: true`. Worktrees from retired story/graph revisions remain tracked and removable once their worker is confirmed stopped. If the task in the current graph has a recorded result commit, pins `refs/storyflow/<worktree>/<id>/tasks/g<graph>/<task>/r<rev>` before removing the detached worktree. |
| `integrate` | Validates worker exit, task completion, commit ancestry, base commit, write scope, protected paths and portable namespace in dependency order; integrates commits into one candidate; pins input refs and candidate ref; persists and returns `IntegrationStatus`. |
| `integration-status` | Returns the current `IntegrationStatus` for the active task graph without mutating Git refs. |

## Invariants and safety rules

1. **Isolated worktrees and per-task base commits:**
   - Independent tasks use the pinned story baseline commit as their base.
   - Dependent tasks use a base commit descending from all code prerequisites (synthesized cleanly via 3-way tree merge when a task depends on multiple parallel code prerequisites).
   - Concurrent running tasks with overlapping `write_scope` cannot both hold active worktrees (`write_scope_occupied`).
2. **Confirmed worker exit before integration:**
   - Scheduler lease expiry, cancellation or graph replacement is not proof that a worker process has exited.
   - Any attempt in the scheduler with `worker_stopped == false` blocks integration with `worker_stop_required`.
3. **Ancestry, write scope and namespace validation:**
   - Every code task (non-empty `write_scope`) must succeed and report an exact lowercase hex commit ID (`missing_task_commit`); read-only tasks (empty `write_scope`) cannot report a commit (`read_only_task_commit`).
   - Each task commit must strictly descend from both the pinned story baseline and the task's required base commit, and every parent along `base..commit` must descend from `base` (`stale_task_base`).
   - Changed paths between the task base and task commit must be non-empty (`empty_task_diff`), reside within the task's declared `write_scope` and the pinned story `scope`, exclude all built-in and policy `protected` paths (`task_scope_violation`), and introduce no case/Unicode portable namespace aliases (`invalid_task_namespace`).
4. **Conflict and overlap detection without auto-resolution:**
   - Independent parallel tasks that modify the same or case/Unicode-aliased path block integration with `overlapping_task_writes` and report the exact conflicting task and file list.
   - Tree merges use `git merge-tree --write-tree` (requiring Git >= 2.38; older or unparseable Git versions fail closed with `git_version_unsupported`) in memory without dirtying any working tree or index. If a 3-way merge produces a content or tree conflict, integration stops immediately, records `integration_conflict` with the exact conflicting file paths, and never auto-resolves conflicts.
5. **Pinned evidence refs and idempotent recovery:**
   - Each validated task input commit is pinned under `refs/storyflow/<worktree>/<id>/tasks/g<graph>/<task>/r<rev>`.
   - The combined candidate commit is pinned under `refs/storyflow/<worktree>/<id>/candidates/story-rev-<story_rev>-graph-<graph_rev>`.
   - Integration state is persisted atomically with a SHA-256 integrity envelope under `.storyflow/<id>/integration/state.json`. Re-running `integrate` after an interruption reuses already integrated prefixes (`reused: true`) idempotently.
   - The existing single-implementer workflow (`storyflow submit ID REV candidate REPORT.md COMMIT` without task graphs) remains unchanged.
