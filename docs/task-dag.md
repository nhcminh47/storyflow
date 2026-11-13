# Typed task DAG and journal (schema 1)

`storyflow::tasks` adds task definitions that are immutable within each story revision and an event-sourced task ledger below one initialized story run. An explicit replacement event provides a fresh graph after the story advances. `storyflow-tasks` is a companion executable installed alongside `storyflow` by `cargo install --locked --path .`. `cargo run` continues to select `storyflow`; use `cargo run --bin storyflow-tasks -- --help` for the companion. The original `init/next/submit/gate/audit` grammar and story event bytes are unchanged.

## Authority and binding

The companion reuses the existing Rust story journal, state and Git implementations as private source modules. It opens the existing story with `Journal::open`, verifies the entire story journal and pinned configuration snapshot, and holds `run.lock` until task publication finishes. It reads scope from the pinned Git baseline and protections from the verified snapshot (or the existing legacy policy). A separate `tasks.lock` serializes task writers. Lock ordering is always story first, tasks second. No child CLI process or status-to-mutation race is involved.

Every graph binds the exact story ID, baseline commit, story revision and stage. Every task binds the same parent/stage, portable repository-relative read/write scopes, acceptance criteria and decision owner. Write scopes must be inside the pinned story scope; protected paths and their descendants are refused, including case/canonical-Unicode aliases. Dependency IDs must exist, be distinct and differ from the task itself. Duplicate task IDs, cycles, unsafe paths, invalid schema versions and unknown JSON fields fail closed.

The task module is also usable by other hosts. A direct library caller must construct `RunContext` from a verified parent run and retain its authoritative story lock throughout the task operation. A caller-created context, deserialized view or packet grants no execution authority. Use `TaskJournal::packet` for a packet checked against the current context. Views remain readable after the story advances or blocks, but packets and task-result mutations from an old story revision/stage are rejected. Completed stories permit views and the scheduler's cleanup-only stop confirmation.

The base DAG provides manual **bookkeeping**: `start` records an orchestrator's explicit start and never launches a model or process. The optional [durable scheduler](task-scheduler.md) adds leases, bounded retries and recovery in the same journal; enabling it refuses legacy `start`/`complete` to prevent bypass. The host remains responsible for actual worker liveness, worker tool permissions and validating reported evidence. Task scopes and artifact digests are declarations, not a filesystem sandbox or proof that an artifact was read. A directory scope can contain protected descendants; actual candidate changes still face the existing story gate. Result commits are syntactically validated references, not integrated or approved candidates. Task success cannot change the story stage, skip a gate or approve a story. Managed capabilities and commit integration remain #9–#10.

## Commands and concurrency

```text
storyflow-tasks init ID GRAPH.json
storyflow-tasks replace ID REV GRAPH.json
storyflow-tasks view ID [--markdown]
storyflow-tasks packet ID TASK
storyflow-tasks start ID REV TASK TASK_REV
storyflow-tasks complete ID REV RESULT.json
```

Commands emit JSON except the explicit Markdown view. Success exits 0; invalid input, stale revision, lock contention, corruption and operational errors exit 2. `--help` describes the protocol. Invalid command grammar is rejected before opening any run. An invented/uninitialized parent run is never created automatically.

`init` defines the graph once at ledger revision 0. Tasks start with task revision 0. A task is ready only when every predecessor has a validated `succeeded` result in the task journal. `start` requires both the current ledger revision and task revision, records `running`, increments that task revision, and returns its running packet. `complete` requires the current ledger revision and the running task revision. It records one of `succeeded`, `failed` or `blocked`, increments the task revision, and recomputes the ready set. Duplicate completion, stale revisions and completion of a task that is not running are rejected. Failed or blocked predecessors never unlock successors. No automatic retry is available.

### Advancing or resuming a story

After a normal story transition (including `submit plan`, gate/review transitions, or `block` followed by `resume`), the previous graph remains readable but cannot dispatch or accept results. The orchestrator reads the current story status and task view, prepares a new graph bound to that exact unblocked, unfinished story revision/stage and the same pinned story/base, then calls `replace ID REV GRAPH.json` with the current **task ledger** revision. The companion holds both authoritative locks while checking and publishing the replacement. A newer story revision is required even if the stage is unchanged after resume; replacement within the same story revision, against a stale ledger revision, or while blocked/done is refused. `init` never replaces existing state.

Replacement appends a `replace` event to the same hash chain. Ledger revisions do not reset. The new active graph starts with fresh pending/ready tasks, without inheriting dependency successes, results or running assignments from its predecessor. Every previous definition, start and result remains in the journal for audit/replay; the ordinary task view shows the current graph. Replacement explicitly retires any unfinished assignments from the previous graph, so the host must stop those workers and disregard their stale callbacks. With the scheduler enabled, unresolved retired attempts remain visible in scheduler state and retain capacity/write-scope occupancy until exact-token stop confirmation. Replacement does not launch, cancel or retry processes. Evidence reused in another stage requires a fresh explicit task/result; cross-graph dependencies are not supported.

Task IDs may be reused safely: initial task revisions in a replacement graph equal the ledger revision of its `replace` event, which exceeds every prior task revision. Subsequent starts/results increment them as before. A stale result cannot complete a new task with the same ID, even if submitted with the latest ledger revision. Hosts must take task revisions from the new packets instead of assuming zero. The existing schema-1 graph/packet/result shapes remain unchanged; `story_revision` identifies the graph's parent generation. Readers predating the new event reject a replaced ledger as an unknown event; upgrade readers before replacement. No journal is rewritten or automatically migrated.

For example, after a plan graph at story revision 0 produces task ledger revision 4, `storyflow submit ID 0 plan REPORT.md` advances the story to revision 1/implement. Prepare an implement graph with `story_revision: 1`, then run `storyflow-tasks replace ID 4 IMPLEMENT.json`. The new task ledger revision and initial task revisions are 5. After `block`/`resume`, prepare another graph for the newly resumed story revision and use the latest task ledger revision for another explicit replacement.

Use the running packet's task revision in a result. Re-read the view before retrying a rejected mutation. Independent tasks can be running together, but ledger mutations are serialized. A conflicting writer receives an explicit lock or stale-revision error; it cannot publish another event at the same revision.

## Wire contract

Public Serde structs in `src/tasks/model.rs` are the schema-1 typed contract; all object types reject unknown fields. Checked-in examples are [graph](task-fixtures/graph.json), [running packet](task-fixtures/packet.json) and [result](task-fixtures/result.json). Their placeholder baseline must be replaced with the initialized story's exact baseline and revision before CLI use.

| Type | Required content |
| --- | --- |
| `TaskGraph` | `schema`, `story`, `base`, `story_revision`, `stage`, ordered `tasks` |
| `TaskSpec` | `id`, `story`, `prerequisites`, `stage`, `read_scope`, `write_scope`, `acceptance_criteria`, `decision_owner` |
| `TaskPacket` | `schema`, `story`, `base`, `story_revision`, `ledger_revision`, typed `task` state, successful `prerequisites` keyed by ID |
| `WorkerResult` | `schema`, `story`, `task`, `task_revision`, `status`, nonblank `report`, nullable `commit`, `artifacts` |
| `Artifact` | portable relative `path`, lowercase hexadecimal `sha256` |

Stages are `plan`, `implement`, `gate`, `review` and the terminal context `done`. Statuses are `pending`, `ready`, `running`, `succeeded`, `failed`, `blocked`, plus scheduler cancellation's `cancelled`. A result status is limited to `succeeded`, `failed` or `blocked`. IDs use 1–40 lowercase ASCII letters/digits/internal hyphens and reject device/path aliases. Commit IDs are lowercase hexadecimal Git SHA-1 or SHA-256 IDs. Reports are at most 4096 UTF-8 bytes; at most 64 artifact references are allowed. The immutable graph contains 1–256 tasks, each with at most 256 scope paths/dependencies, 64 bounded acceptance criteria and a bounded decision owner.

`TaskView` exports a BTreeMap ordered by task ID, an ordered ready set, pinned graph context and the ledger revision. Fixed inputs and events produce the same state, packets and Markdown/JSON view after restart. `view` of a run without tasks returns `revision: null`, empty tasks and ready set. It may create task storage/lock directories but does not rewrite or advance the story journal. Markdown is a derived export; existing `tasks.md` files are never treated as authoritative or silently imported.

## Persistence and recovery boundary

The ledger lives in `.storyflow/ID/tasks/events/NNNNNNNNNNNN.json`, separate from story events. Each schema-1 event contains its revision, prior event SHA-256 and typed event, protected by a checksum over compact typed JSON. Replay checks contiguous filenames, schema/revision/checksum chain, event ordering, graph validation and every transition. Gaps, unexpected entries, malformed/partially written published records and checksum changes fail closed. Missing task storage is the explicit no-task migration path.

Each event is serialized and bounded before publication, synced to `pending.tmp`, then atomically renamed while the writer lock is held. On Unix, the parent `events/` directory is also synced after rename; newly created task storage directories have their parent synced as well. Memory changes only after publication and the applicable directory syncs. An interrupted pending file is ignored on replay and replaced by the next valid write. Published events are not overwritten. Events/documents are limited to 1 MiB, journals to 4096 events and 64 MiB. Task storage entries reject existing symlinks/reparse points. These are trusted local-state persistence checks, not a hostile-filesystem sandbox.

Filesystem sync/rename errors fail explicitly. A failure after rename can leave a published event despite an error return: close and reopen the journal and read its revision before retrying. Unix directory-sync failures are not silently ignored. On Windows and other non-Unix platforms, directory sync is not implemented: file contents are synced, but a power loss after successful return may still lose the renamed entry or a newly created directory. This is the accepted platform durability limit, also present in the existing story journal. Preserve the complete ledger with its parent journal, snapshot and Git objects. The checksum chain detects corruption of retained records; it does not authenticate state against an attacker who rewrites the whole chain or removes a complete suffix. Story configuration/base rebinding and automatic graph migration remain unsupported.

## Validation

`tests/task_dag.rs` covers strict graph/path validation; A,B→C readiness; failed/blocked dependencies; global/task/story stale revisions; duplicate completion; exact replay/export/packet equality; concurrent writer locks and competing real CLI processes; corrupt/gapped/unknown/partial/oversized events; interrupted pending publication; invalid result identity/artifacts; schema fixture round trips; missing parent runs; pinned snapshot rejection; story run-lock contention; and unchanged legacy story events/revisions. Replacement tests verify retained history, fresh dependency readiness, stage/scope/base binding, monotonic task revisions and rejection of old results even with a current ledger revision. A real CLI lifecycle covers plan→implement, block/resume in the same stage, gate→review and terminal read-only behavior. Unix runs exercise directory syncing on task creation and every publication. It uses temporary local repositories and no provider, credentials or cloud calls.
