# Host Adapter Protocol and Codex Adapter

Storyflow separates orchestration truth (Rust) from execution hosts (Codex and future providers). `storyflow::adapters` defines the provider-neutral protocol, authoritative reconciliation bridge, offline CI fake adapter, and first `CodexAdapter`.

## 1. Protocol Contract and Authority Boundaries

Core types live in `src/adapters/mod.rs`:

- **`HostAdapter`**:
  - `provider(&self) -> &str`
  - `probe(&self) -> HostCapabilities`
  - `execute(&self, request: &AdapterRequest, cancel: &CancelToken) -> AdapterOutcome`
- **`HostCapabilities`**: describes probed host support (`available`, `version`, `executable`, `supports_json_output`, `supports_output_schema`, `supports_output_file`, `supports_working_dir`, `supports_sandbox_flag`, `supports_cancel`, `supports_session_id`, `supports_stdin_prompt`, `detected_flags`, `unavailable_reason`).
- **`AdapterRequest`**: carries `schema`, a Rust-issued `DispatchPacket` (including `LeaseToken` and `TaskPacket`), `workdir`, `timeout_ms`, and `ExecutionPolicy` (`ReadOnly` or `WorkspaceWrite`).
- **`SessionCorrelation`**: binds `attempt_id`, `lease_id`, `owner`, and provider `session_id` back to the exact scheduler lease.
- **`CancelToken`**: thread-safe cooperative cancellation flag checked before launch and during process supervision.
- **`AdapterOutcome`**: tagged typed result (`completed`, `cancelled`, `timed_out`, `host_crashed`, `host_unavailable`, `invalid_output`) including explicit `worker_stopped`.

### Non-negotiable invariants

1. **Rust remains authoritative.** Adapters never open or mutate `.storyflow/<id>/events` (the story journal), never advance story stages, and never bypass Rust quality gates or integration checks.
2. **Untrusted host output.** `runner::reconcile_outcome` validates `SessionCorrelation`, `worker_stopped == true`, lease expiry, story context currency, and `WorkerResult` identity (`story`, `task`, `task_revision`, schema, bounded report, commit/artifact format) before calling `TaskJournal::complete_attempt`.
3. **No unapproved high-risk actions.** `ExecutionPolicy` intentionally has no full-access, auto-merge, or auto-push variants. `CodexAdapterConfig` rejects dangerous/reserved flags fail-closed.
4. **Explicit worker exit.** Expiry, cancellation, crash, or invalid output only frees scheduler capacity when process-tree cleanup succeeds and `worker_stopped == true` is confirmed.

## 2. Reconciliation and Failure-Handling Matrix

`src/adapters/runner.rs` provides `dispatch_and_run` and `reconcile_outcome`:

| Condition / Outcome | Journal Action | `ReconcileStatus` | `worker_stopped` | Recoverable |
| --- | --- | --- | --- | --- |
| `probe().available == false` (before dispatch) | No lease reserved (attempt budget preserved) | `HostUnavailable` | `true` | `true` |
| Valid `Completed` on active lease (`worker_stopped: true`) | `complete_attempt` | `Applied` | `true` | `false` (succeeded) / `true` (failed/blocked) |
| Duplicate, unknown attempt, or mismatched lease | None (rejected) | `RejectedDuplicateOrStale` | `false` | `false` |
| Attempt already settled (`worker_stopped: true`) | None (rejected) | `RejectedDuplicateOrStale` | `false` | `false` |
| Correlation mismatch or invalid `WorkerResult` (`worker_stopped: true`) | `confirm_stopped` (task becomes `Blocked`) | `Blocked` | `true` | `true` |
| `Completed` / failure with `worker_stopped: false` | None (capacity stays occupied) | `BlockedUnstopped` | `false` | `false` |
| Lease expired (`now >= expires_at_ms`, `worker_stopped: true`) | `recover_expired` + `confirm_stopped` | `Blocked` (`lease_expired`) | `true` | `true` |
| `TimedOut` / `HostCrashed` / `InvalidOutput` (`worker_stopped: true`) | `confirm_stopped` (task becomes `Blocked`) | `Blocked` | `true` | `true` |
| `HostUnavailable` after dispatch | `confirm_stopped` (task becomes `Blocked`) | `HostUnavailable` | `true` | `true` |
| `Cancelled` on `Leased` task (`worker_stopped: true`) | `confirm_stopped` (task becomes `Blocked`) | `Blocked` | `true` | `true` |
| `Cancelled` / `Completed` after `cancel_task` (`worker_stopped: true`) | `confirm_stopped` (releases occupancy) | `Cancelled` | `true` | `false` |
| Outcome for `Retired` graph attempt (`worker_stopped: true`) | `confirm_stopped` (releases old occupancy) | `RejectedDuplicateOrStale` | `true` | `false` |

## 3. Offline CI Testing with `FakeAdapter`

`src/adapters/fake.rs` implements `FakeAdapter`, which requires no cloud model, network, or API key:

- Script deterministic success, failure, blocked, invalid output, mismatched correlation, invalid session ID, stale task revision, wrong task/story, unstopped completion, timeout, cancellation, host crash, and unavailable host outcomes.
- Assert exact `AdapterRequest` inspection (`requests()`) and end-to-end `TaskJournal` + `dispatch_and_run` reconciliation in `tests/host_adapter.rs`.

## 4. `CodexAdapter` Design and Security Boundaries

`src/adapters/codex.rs` implements `CodexAdapter` on top of `security::process::run_host_command`:

- **Capability probing:** Resolves `executable` (explicit path or `host_executable("codex")` on absolute `PATH`, refusing `.bat`/`.cmd` wrappers on Windows), runs `codex --version` and `codex exec --help` under `probe_timeout`, and only emits flags actually observed in help output (`--cd`, `--sandbox`, `--json`, `--output-schema`, `--output-last-message`, `--ephemeral`, and `-` stdin prompt).
- **Safe flag policy:** `CodexAdapterConfig::validate()` rejects empty/control-char args and refuses high-risk or reserved flags in `extra_args` (`--yolo`, `--dangerously-bypass-approvals-and-sandbox`, `--full-auto`, `--ask-for-approval`, `-a`, `--sandbox`, `-s`, `danger-full-access`, `--cd`, `-C`, `--json`, `--output-schema`, `--output-last-message`, `-o`, `--auto-merge`, `--push`).
- **Environment filtering:** Starts from `security::process::configure_environment` and passes through only explicit variable names in `CodexAdapterConfig::passthrough_env` (`HOME`, `USERPROFILE`, `CODEX_HOME`, `XDG_CONFIG_HOME`, `APPDATA`, `LOCALAPPDATA`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`), never logging or serializing secret values.
- **Process-tree containment & cancellation:** Cancellation triggers `ChildGuard::cleanup()` (Windows Job Object / Linux process group `SIGKILL`) before returning `worker_stopped: true`. If supervision cleanup fails, `worker_stopped` is reported as `false`.

## 5. Codex Smoke Checklist (Manual / Online Host)

Offline CI runs without Codex installed. To smoke-test against a real installed Codex CLI:

1. **Verify host installation and auth:**
   ```sh
   codex --version
   codex exec --help
   ```
   Confirm `codex` is authenticated in your local environment (`codex login status` or configured API credentials).
2. **Verify probe & sandboxed read-only execution in an isolated scratch directory:**
   Run the ignored smoke test (requires a real `codex` binary on `PATH` and valid auth):
   ```sh
   cargo test --locked --test host_adapter codex_live_smoke -- --ignored --nocapture
   ```
3. **Inspect checklist outcomes:**
   - `CodexAdapter::new().probe()` reports `available: true`, non-empty `version`, and expected `detected_flags`.
   - `dispatch_and_run` executes in a temporary read-only story worktree without touching `.storyflow/` or running `git push` / auto-merge.
   - `ReconcileReport` reports `status: Applied`, a valid `session_id` (matching Codex `thread_id`), and `worker_stopped: true`.
   - Parent story journal stage and revision remain unchanged until explicit Rust `storyflow gate` / `submit` transitions.

## 6. Compatibility Notes for Future Cursor and Claude Code Adapters

> **Scope note:** Cursor and Claude Code adapters are **not** implemented or promised in Issue #11. These notes capture protocol expectations for future M3+ adapter issues.

- **Capability probing first:** Like `CodexAdapter`, future adapters must probe the installed host binary (`--version`, `--help`) and record detected flags in `HostCapabilities` rather than hard-coding CLI assumptions.
- **Expected CLI / invocation differences:**
  - **Claude Code (`claude`):** Non-interactive execution typically uses print mode (`-p`) with structured output flags when supported (`--output-format json` / `stream-json`), explicit tool-permission flags, and session identifiers in JSON output. Its adapter will map `ExecutionPolicy::ReadOnly` vs `WorkspaceWrite` to the supported tool/permission model and enforce Storyflow's `read_scope` / `write_scope` constraints in prompt + post-execution checks.
  - **Cursor (`cursor-agent` / CLI):** Headless CLI availability, flags, sandbox options, and JSON output support vary across releases and may require GUI/IDE host bridging when headless CLI is absent. `probe()` must fail closed with `HostUnavailable` when non-interactive execution is unsupported.
- **Shared invariants across all adapters:**
  - Use `security::process::run_host_command` (or an equally supervised process-tree boundary) so cancellation/timeout guarantees `worker_stopped` only after descendant termination.
  - Correlate every invocation with `SessionCorrelation` (`attempt_id`, `lease_id`, `owner`, `session_id`) and reconcile exclusively through `runner::reconcile_outcome`.
  - Keep Rust policy floors, write-scope validation, integration gates, and story transitions authoritative.
