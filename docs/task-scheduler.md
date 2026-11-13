# Durable task scheduler

The opt-in scheduler in `storyflow::tasks` extends the [task DAG journal](task-dag.md) with dispatch attempts, owner leases, heartbeats, bounded retries and explicit recovery. Rust decides readiness, capacity, write-scope collisions and every recorded transition. A provider-neutral host receives a `DispatchPacket`, starts its worker separately, routes callbacks through the CLI/library and confirms worker termination. Reserving a lease does not launch a model or process.

## Commands

```text
storyflow-tasks configure ID REV POLICY.json
storyflow-tasks status ID
storyflow-tasks dispatch ID REV TASK TASK_REV OWNER
storyflow-tasks heartbeat ID REV LEASE.json
storyflow-tasks finish ID REV ATTEMPT_RESULT.json
storyflow-tasks cancel ID REV TASK TASK_REV REASON
storyflow-tasks recover ID REV
storyflow-tasks stopped ID REV LEASE.json REASON
storyflow-tasks retry ID REV TASK TASK_REV REASON
```

`REV` is the latest **task ledger** revision, obtained from `view.revision` or `status.ledger_revision`; it is separate from the parent story revision. `TASK_REV` comes from `view.tasks[TASK].revision`. Quote a multiword `REASON` as one argument. Commands return JSON and exit 0 on success or 2 on rejected input, stale identity/revision, lock contention or operational error. Command shapes and revision numbers are checked before opening run data. Requests are bounded JSON files and unknown fields are rejected.

| Command | Input and result |
| --- | --- |
| `configure` | A `SchedulerPolicy`; returns `SchedulerStatus`. Configure once, after defining a current graph and before starting workers. |
| `status` | Read-only `SchedulerStatus`, including ordered `dispatchable` task IDs, per-task `blockers`, all attempts and occupied `active_attempts`. Requires configured state. |
| `dispatch` | A ready task, its revision and an owner; returns `DispatchPacket` with the running task packet, exact lease token and expiry. |
| `heartbeat` | The exact `LeaseToken` from dispatch; returns status. Extends an unexpired lease without changing the running task revision. |
| `finish` | An `AttemptResult` containing the token, `WorkerResult` and `worker_stopped: true`; returns status. |
| `cancel` | An unfinished task and bounded reason; records terminal `cancelled` and returns status. It does not stop its worker. |
| `recover` | Records expired leases as blocked attempts/tasks and returns status. It never redispatches workers. |
| `stopped` | Exact recorded token and a bounded reason after the host confirms termination; fences that attempt, releases its occupancy and returns status. |
| `retry` | A failed/blocked task, its latest revision and reason; returns a fresh ready `TaskPacket` when the attempt budget and termination checks pass. Dispatch remains a separate operation. |

The existing `init`, `replace`, `view`, `packet`, `start` and `complete` grammar remains available. Unconfigured ledgers keep the original manual start/result behavior. Configuration refuses existing running manual tasks; after configuration, legacy `start` and `complete` are refused so they cannot bypass leases. Configuration persists for the run, including graph replacements; it cannot be rebound mid-run.

The [policy fixture](task-fixtures/scheduler-policy.json) contains `max_concurrency` (1–256), `max_attempts` (1–100, including the first dispatch) and `lease_ms` (1–86,400,000). Policy limits are enforced by Rust. Owner/reason fields are bounded strings, not authorization credentials; use opaque host IDs and keep secrets out of requests, reports and journal records.

## Host workflow

After initializing a story and its graph as described in [task DAG](task-dag.md), this PowerShell example dispatches ready task `a` from a graph bound to `demo-001`. Replace the story/task IDs with your run's IDs. Use the latest revision before each mutation, including after another parallel task finishes.

```powershell
$id = 'demo-001'
$policy = @{ max_concurrency = 2; max_attempts = 3; lease_ms = 60000 }
$policy | ConvertTo-Json | Set-Content -Encoding utf8 policy.json
$view = storyflow-tasks view $id | ConvertFrom-Json
storyflow-tasks configure $id $view.revision policy.json

$view = storyflow-tasks view $id | ConvertFrom-Json
$dispatch = storyflow-tasks dispatch $id $view.revision a $view.tasks.a.revision host-1 |
    ConvertFrom-Json
$dispatch.lease | ConvertTo-Json | Set-Content -Encoding utf8 lease.json

# Host starts its worker with $dispatch.packet and retains $dispatch.lease.
# Before expiry, the host may refresh the lease using the latest ledger revision.
$status = storyflow-tasks status $id | ConvertFrom-Json
storyflow-tasks heartbeat $id $status.ledger_revision lease.json

# After the host confirms the worker exited, build the result from its evidence.
$result = @{
    schema = 1
    lease = $dispatch.lease
    worker_stopped = $true
    result = @{
        schema = 1; story = $id; task = 'a'
        task_revision = $dispatch.packet.task.revision
        status = 'succeeded'; report = 'AC1 verified'
        commit = $null; artifacts = @()
    }
}
$result | ConvertTo-Json -Depth 20 | Set-Content -Encoding utf8 attempt-result.json
$status = storyflow-tasks status $id | ConvertFrom-Json
storyflow-tasks finish $id $status.ledger_revision attempt-result.json
```

Check `$LASTEXITCODE` after every command before consuming its output or proceeding. An actual host must supply its worker's verified report/evidence; the illustrative success report is not proof of execution. `worker_stopped: true` is the host's explicit termination assertion. Rust enforces that it is present and true, but this scheduler cannot independently observe an external model or worker process. False assertions violate the host contract.

Successful prerequisites unlock dependent tasks only after their valid results are published. A failed, blocked or cancelled predecessor never unlocks its successors. Dispatches reserve global capacity while workers are unresolved, and reject overlapping write scopes in either containment direction after case/Unicode normalization. This includes unresolved attempts from retired graphs. An unresolved worker for the same task ID blocks redispatch even across graph generations and even with an empty write scope. Read scopes do not reserve write capacity. Worker isolation/worktree provisioning and tool execution remain host responsibilities; worktrees are not sandboxes.

## Lease identity and time

Every dispatch has a distinct deterministic attempt ID and lease ID within one run journal, bound to the story/base, graph generation, task and publication revision. Story/base is not globally unique across separate checkouts: a host coordinating multiple runs must namespace its idempotency storage by run/worktree identity as well as the attempt ID. These IDs are fencing identities, not authentication secrets. A token contains all of `attempt_id`, `lease_id` and `owner`; pass it unchanged. Heartbeats preserve the attempt and task revision. A retry creates a fresh attempt on its next dispatch. Completion checks the active graph/task revision and exact token as well as the latest ledger revision. Duplicates and callbacks from an old owner, lease, attempt or graph are rejected; giving them a current ledger revision does not make them valid.

The CLI uses `SystemClock` in milliseconds since the Unix epoch. Library hosts can inject `Clock` for deterministic fixtures. Every scheduler event records its observed timestamp, and replay uses those recorded times rather than the reopening clock. Times cannot move backward from the last recorded observation; clock regression fails explicitly. Lease duration addition is checked for overflow. At `now >= expires_at_ms`, heartbeat and completion are stale even before a `recover` event has been published. Status observes expiry and reports actionable blockers without silently publishing transitions or releasing capacity.

## Cancellation, recovery and graph replacement

Expiration, cancellation and host/CLI restart provide no evidence that a worker stopped. Unstopped expired/cancelled/retired attempts continue to occupy capacity and block overlapping writes. Never redispatch a task on the assumption that an expired lease killed its worker.

After a crash, reopen `view` and `status` under the verified parent story context. Accepted results and all attempt identities are replayed from the journal. Use the following recovery sequence when a lease has expired:

1. Read the latest status and publish `recover ID REV` to record expiry.
2. Locate the old worker using the recorded owner/token. Have the host stop or otherwise positively establish termination of that exact worker.
3. Read the latest ledger revision and call `stopped ID REV LEASE.json 'host confirmed exit'`.
4. If the task is failed/blocked and has budget remaining, read its latest task revision and call `retry`; then perform a new `dispatch` from the returned task state and latest ledger revision.

There are no automatic retries. The configured budget counts dispatches for that task in that graph, including the first attempt. A successful or cancelled task cannot be retried. A new graph starts its own task attempt budgets while preserving all previous attempt history and unresolved-worker occupancy.

Normal story transitions, `block`/`resume` and terminal completion keep their existing story-journal behavior. An old graph becomes noncurrent: `status.current` is false and `dispatchable` is empty; old callbacks cannot mutate its tasks. Publish an explicit `replace` graph for the new unblocked, unfinished story revision using the existing protocol. Replacement retires unfinished old assignments, preserves policy and attempt history, and does not inherit old prerequisite successes.

The cleanup-only `stopped` operation is permitted against the latest task ledger revision even when the story has advanced, is blocked or is done. It requires the exact recorded token, fences a live attempt, and never applies a `WorkerResult` or changes the story stage. In the current graph, stopping an unfinished active attempt blocks the task; in a retired graph it only releases old occupancy. This exception allows safe cleanup after a story transition without reviving stale work.

## Wire and persistence compatibility

Public types are exported from `storyflow::tasks`; new scheduler types live in `src/tasks/scheduler.rs`. The strict examples are [policy](task-fixtures/scheduler-policy.json), [lease token](task-fixtures/scheduler-lease.json), [dispatch packet](task-fixtures/scheduler-dispatch.json) and [attempt result](task-fixtures/scheduler-result.json). Their tokens/base are illustrative placeholders, not usable authority; use the exact token and packet returned by a real dispatch.

Scheduler records are additive `scheduler` events in the existing schema-1 task hash chain. Existing graph/packet/result shapes and previously published event bytes/checksums remain unchanged. `TaskView` omits optional scheduler state until configured. Readers predating scheduler events fail closed on those events; upgrade readers before enabling the scheduler. Existing Markdown task ledgers are never imported as state.

The same story-first/task-second locks, bounded atomic publication, checksum/revision replay and interrupted `pending.tmp` handling apply. After an error that might follow publication, close/reopen and read the latest revision before retrying. Preserve the complete task journal together with the parent story journal, pinned configuration snapshot and Git objects. The existing [platform durability limits](task-dag.md#persistence-and-recovery-boundary) remain: Windows does not sync directory entries, and hash chains do not authenticate against malicious whole-chain rewriting or suffix removal.

The scheduler does not validate reported commit ancestry/diffs, integrate commits, enforce host tool capabilities, run sandboxed gates or prove artifact contents. Existing Rust story candidate scope/gate/review checks remain authoritative. Managed capability enforcement and worker commit integration are separate follow-up work (#9 and #10).
