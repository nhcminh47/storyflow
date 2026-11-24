use serde_json::json;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use storyflow::{
    adapters::{
        ADAPTER_SCHEMA, AdapterRequest, CancelToken, CodexAdapter, CodexAdapterConfig,
        ExecutionPolicy, FakeAdapter, FakeBehavior, HostAdapter, ReconcileReport, ReconcileStatus,
        codex::CodexCommandRunner, dispatch_and_run, reconcile_outcome,
    },
    security::process::{ControlledResult, HostCommandSpec, ProcessError, ProcessResult},
    tasks::*,
};

const STORY: &str = "adapter-001";
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct FakeClock(Arc<AtomicU64>);
impl FakeClock {
    fn at(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }
    fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Fixture {
    root: PathBuf,
    context: RunContext,
}
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/host-adapter-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(root.join(format!(".storyflow/{STORY}/events"))).unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        Self {
            root,
            context: RunContext {
                story: STORY.into(),
                base: "1".repeat(40),
                story_revision: 0,
                stage: TaskStage::Plan,
                blocked: false,
                scope: vec!["app".into()],
                protected: vec!["app/private".into()],
            },
        }
    }

    fn configured(&self, policy: SchedulerPolicy, clock: &impl Clock) -> TaskJournal {
        let mut journal = TaskJournal::open(&self.root, self.context.clone()).unwrap();
        journal.initialize(graph()).unwrap();
        journal
            .configure_scheduler(journal.view().revision.unwrap(), policy, clock)
            .unwrap();
        journal
    }

    fn story_events_dir(&self) -> PathBuf {
        self.root.join(format!(".storyflow/{STORY}/events"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let allowed = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/host-adapter-tests");
        assert!(self.root.starts_with(allowed));
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn spec(id: &str, prerequisites: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        story: STORY.into(),
        prerequisites: prerequisites.iter().map(|v| (*v).into()).collect(),
        stage: TaskStage::Plan,
        read_scope: vec!["app".into()],
        write_scope: vec![format!("app/{id}.txt")],
        acceptance_criteria: vec!["AC1 verified".into()],
        decision_owner: "orchestrator".into(),
    }
}

fn graph() -> TaskGraph {
    TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![spec("a", &[]), spec("b", &[]), spec("c", &["a", "b"])],
    }
}

fn policy(max_concurrency: u16, max_attempts: u16) -> SchedulerPolicy {
    SchedulerPolicy {
        max_concurrency,
        max_attempts,
        lease_ms: 1000,
    }
}

fn task_rev(journal: &TaskJournal, task: &str) -> u64 {
    journal.view().tasks[task].revision
}

#[allow(clippy::too_many_arguments)]
fn run_task(
    journal: &mut TaskJournal,
    adapter: &dyn HostAdapter,
    task: &str,
    owner: &str,
    workdir: &Path,
    policy: ExecutionPolicy,
    cancel: &CancelToken,
    clock: &impl Clock,
) -> TaskResult<ReconcileReport> {
    let trev = task_rev(journal, task);
    dispatch_and_run(
        journal,
        adapter,
        task,
        trev,
        owner,
        workdir,
        Duration::from_secs(10),
        policy,
        cancel,
        clock,
    )
}

fn exit_status(code: i32) -> ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}

struct MockCodexRunner {
    executable: Result<PathBuf, String>,
    responses: Mutex<Vec<Result<ControlledResult, ProcessError>>>,
    calls: Mutex<Vec<HostCommandSpec>>,
    write_last_message: Mutex<Option<String>>,
}

impl MockCodexRunner {
    fn new(executable: Result<PathBuf, String>) -> Self {
        Self {
            executable,
            responses: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            write_last_message: Mutex::new(None),
        }
    }

    fn push_ok(&self, code: i32, stdout: &str, stderr: &str, timed_out: bool, cancelled: bool) {
        self.responses.lock().unwrap().push(Ok(ControlledResult {
            process: ProcessResult {
                status: Some(exit_status(code)),
                timed_out,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                truncated: false,
            },
            cancelled,
        }));
    }

    fn push_err(&self, kind: io::ErrorKind, msg: &str) {
        self.responses.lock().unwrap().push(Err(ProcessError {
            error: io::Error::new(kind, msg),
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        }));
    }

    fn set_last_message(&self, content: &str) {
        *self.write_last_message.lock().unwrap() = Some(content.to_owned());
    }

    fn calls(&self) -> Vec<HostCommandSpec> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| HostCommandSpec {
                program: c.program.clone(),
                args: c.args.clone(),
                timeout: c.timeout,
                stdin: c.stdin.clone(),
                passthrough_env: c.passthrough_env.clone(),
            })
            .collect()
    }
}

impl CodexCommandRunner for MockCodexRunner {
    fn resolve_executable(&self, _configured: Option<&Path>) -> Result<PathBuf, String> {
        self.executable.clone()
    }

    fn run(
        &self,
        spec: &HostCommandSpec,
        _cwd: &Path,
        _should_stop: &dyn Fn() -> bool,
    ) -> Result<ControlledResult, ProcessError> {
        self.calls.lock().unwrap().push(HostCommandSpec {
            program: spec.program.clone(),
            args: spec.args.clone(),
            timeout: spec.timeout,
            stdin: spec.stdin.clone(),
            passthrough_env: spec.passthrough_env.clone(),
        });
        if spec.args.first().map(String::as_str) == Some("exec")
            && spec.args.get(1).map(String::as_str) != Some("--help")
            && let Some(msg) = self.write_last_message.lock().unwrap().clone()
            && let Some(pos) = spec.args.iter().position(|a| a == "--output-last-message")
            && let Some(path) = spec.args.get(pos + 1)
        {
            fs::write(path, msg).unwrap();
        }
        let mut queue = self.responses.lock().unwrap();
        assert!(!queue.is_empty(), "unexpected runner call: {:?}", spec.args);
        queue.remove(0)
    }
}

#[test]
fn fake_host_end_to_end_dag_execution_and_authority_boundary() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let adapter = FakeAdapter::available("fake");
    let cancel = CancelToken::new();

    // Story journal directory has no story events touched by task/adapter execution.
    let before_story_entries = fs::read_dir(fixture.story_events_dir()).unwrap().count();

    let report_a = run_task(
        &mut journal,
        &adapter,
        "a",
        "host-1",
        &fixture.root,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report_a.status, ReconcileStatus::Applied);
    assert_eq!(report_a.code, "task_succeeded");
    assert!(report_a.worker_stopped);
    assert!(!report_a.recoverable);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Succeeded);
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Pending);

    // Task b fails first, then retries and succeeds, unlocking c.
    adapter.push_behavior(FakeBehavior::Fail {
        report: "transient test failure".into(),
        commit: None,
        artifacts: Vec::new(),
    });
    let report_b1 = run_task(
        &mut journal,
        &adapter,
        "b",
        "host-1",
        &fixture.root,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report_b1.status, ReconcileStatus::Applied);
    assert_eq!(report_b1.code, "task_failed");
    assert!(report_b1.recoverable);
    assert_eq!(journal.view().tasks["b"].status, TaskStatus::Failed);
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Pending);

    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "b",
            task_rev(&journal, "b"),
            "retry transient failure",
            &clock,
        )
        .unwrap();
    let report_b2 = run_task(
        &mut journal,
        &adapter,
        "b",
        "host-1",
        &fixture.root,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report_b2.status, ReconcileStatus::Applied);
    assert_eq!(journal.view().tasks["b"].status, TaskStatus::Succeeded);
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Ready);

    let report_c = run_task(
        &mut journal,
        &adapter,
        "c",
        "host-1",
        &fixture.root,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report_c.status, ReconcileStatus::Applied);
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Succeeded);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);

    // Adapter requests recorded exact prerequisite evidence for c.
    let requests = adapter.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[3].dispatch.packet.prerequisites.contains_key("a"));
    assert!(requests[3].dispatch.packet.prerequisites.contains_key("b"));

    // Story journal events remain completely untouched by adapter runs.
    let after_story_entries = fs::read_dir(fixture.story_events_dir()).unwrap().count();
    assert_eq!(before_story_entries, after_story_entries);
}

#[test]
fn unavailable_host_does_not_burn_attempt_budget_before_dispatch() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 1), &clock);
    let adapter = FakeAdapter::unavailable("codex", "codex binary missing");
    let cancel = CancelToken::new();

    let report = run_task(
        &mut journal,
        &adapter,
        "a",
        "host-1",
        &fixture.root,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::HostUnavailable);
    assert_eq!(report.attempt_id, None);
    assert!(report.recoverable);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Ready);
    assert!(
        journal
            .scheduler_status(&clock)
            .unwrap()
            .attempts
            .is_empty()
    );
}

#[test]
fn invalid_mismatched_stale_duplicate_and_crash_outcomes_are_safely_reconciled() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(2, 10), &clock);
    let cancel = CancelToken::new();

    for (label, behavior, expected_code) in [
        (
            "invalid output",
            FakeBehavior::InvalidOutput {
                detail: "malformed json\n\x07".into(),
            },
            "invalid_output",
        ),
        (
            "mismatched correlation",
            FakeBehavior::MismatchedCorrelation,
            "correlation_mismatch",
        ),
        (
            "invalid session id",
            FakeBehavior::InvalidSessionId {
                session_id: "bad\nsession".into(),
            },
            "correlation_mismatch",
        ),
        (
            "stale task revision",
            FakeBehavior::StaleRevision,
            "invalid_worker_result",
        ),
        (
            "wrong task",
            FakeBehavior::WrongTask { task: "b".into() },
            "invalid_worker_result",
        ),
        (
            "wrong story",
            FakeBehavior::WrongStory {
                story: "other-001".into(),
            },
            "invalid_worker_result",
        ),
        (
            "host crash",
            FakeBehavior::HostCrashed {
                exit_code: Some(137),
                detail: "segfault".into(),
                worker_stopped: true,
            },
            "host_crashed",
        ),
        (
            "timed out",
            FakeBehavior::TimedOut {
                reason: "deadline exceeded".into(),
                worker_stopped: true,
            },
            "worker_timed_out",
        ),
    ] {
        let adapter = FakeAdapter::available("fake").with_default(behavior);
        let report = run_task(
            &mut journal,
            &adapter,
            "a",
            "host-1",
            &fixture.root,
            ExecutionPolicy::WorkspaceWrite,
            &cancel,
            &clock,
        )
        .unwrap();
        assert_eq!(report.status, ReconcileStatus::Blocked, "{label}");
        assert_eq!(report.code, expected_code, "{label}");
        assert!(report.worker_stopped, "{label}");
        assert!(report.recoverable, "{label}");
        assert_eq!(
            journal.view().tasks["a"].status,
            TaskStatus::Blocked,
            "{label}"
        );
        assert_eq!(
            journal.scheduler_status(&clock).unwrap().active_attempts,
            0,
            "{label}"
        );
        journal
            .retry_task(
                journal.view().revision.unwrap(),
                "a",
                task_rev(&journal, "a"),
                label,
                &clock,
            )
            .unwrap();
    }

    // Now test duplicate/replayed outcome rejection on a completed attempt.
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "host-1",
            &clock,
        )
        .unwrap();
    let req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: fixture.root.clone(),
        timeout_ms: 5000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome = FakeAdapter::available("fake").execute(&req, &cancel);
    let first = reconcile_outcome(&mut journal, &dispatch, outcome.clone(), &clock).unwrap();
    assert_eq!(first.status, ReconcileStatus::Applied);
    let rev_after_first = journal.view().revision.unwrap();

    let duplicate = reconcile_outcome(&mut journal, &dispatch, outcome, &clock).unwrap();
    assert_eq!(duplicate.status, ReconcileStatus::RejectedDuplicateOrStale);
    assert_eq!(duplicate.code, "duplicate_or_settled_attempt");
    assert_eq!(journal.view().revision.unwrap(), rev_after_first);
}

#[test]
fn unstopped_worker_keeps_capacity_occupied_and_blocks_retry() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 3), &clock);
    let adapter = FakeAdapter::available("fake").with_default(FakeBehavior::UnstoppedCompletion);
    let cancel = CancelToken::new();

    let report = run_task(
        &mut journal,
        &adapter,
        "a",
        "host-1",
        &fixture.root,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::BlockedUnstopped);
    assert!(!report.worker_stopped);
    assert!(!report.recoverable);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Running);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 1);
    assert!(
        journal
            .scheduler_status(&clock)
            .unwrap()
            .dispatchable
            .is_empty()
    );
}

#[test]
fn expired_lease_and_cancelled_task_reconcile_cleanly() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let cancel = CancelToken::new();

    // 1. Lease expires before outcome arrives -> recovers & confirms stopped.
    let dispatch_a = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "host-1",
            &clock,
        )
        .unwrap();
    let req_a = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch_a.clone(),
        workdir: fixture.root.clone(),
        timeout_ms: 5000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome_a = FakeAdapter::available("fake").execute(&req_a, &cancel);
    clock.set(dispatch_a.expires_at_ms);
    let report_a = reconcile_outcome(&mut journal, &dispatch_a, outcome_a, &clock).unwrap();
    assert_eq!(report_a.status, ReconcileStatus::Blocked);
    assert_eq!(report_a.code, "lease_expired");
    assert!(report_a.worker_stopped);
    assert!(report_a.recoverable);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);

    // 2. Task is cancelled while worker is active -> stopped confirmation releases occupancy.
    let dispatch_b = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "b",
            task_rev(&journal, "b"),
            "host-1",
            &clock,
        )
        .unwrap();
    journal
        .cancel_task(
            journal.view().revision.unwrap(),
            "b",
            task_rev(&journal, "b"),
            "user cancelled",
            &clock,
        )
        .unwrap();
    let req_b = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch_b.clone(),
        workdir: fixture.root.clone(),
        timeout_ms: 5000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome_b = FakeAdapter::available("fake").execute(&req_b, &cancel);
    let report_b = reconcile_outcome(&mut journal, &dispatch_b, outcome_b, &clock).unwrap();
    assert_eq!(report_b.status, ReconcileStatus::Cancelled);
    assert!(report_b.worker_stopped);
    assert!(!report_b.recoverable);
    assert_eq!(journal.view().tasks["b"].status, TaskStatus::Cancelled);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
}

#[test]
fn codex_adapter_probes_flags_and_rejects_high_risk_extra_args() {
    let full_help = "\
Usage: codex exec [OPTIONS] [PROMPT]
Options:
      --json                    Emit JSONL events
      --output-schema <FILE>    JSON schema file
  -o, --output-last-message <FILE>
      --cd <DIR>                Working directory
      --sandbox <MODE>          read-only | workspace-write
      --ephemeral               Do not persist session
  [PROMPT]                      Prompt or - to read from stdin
";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, full_help, "", false, false);

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner);
    let caps = adapter.probe();
    assert!(caps.available);
    assert_eq!(caps.provider, "codex");
    assert_eq!(caps.version.as_deref(), Some("codex-cli 0.55.0"));
    assert!(caps.supports_json_output);
    assert!(caps.supports_output_schema);
    assert!(caps.supports_output_file);
    assert!(caps.supports_working_dir);
    assert!(caps.supports_sandbox_flag);
    assert!(caps.supports_stdin_prompt);
    assert!(caps.supports_session_id);
    assert!(caps.detected_flags.contains(&"--ephemeral".to_owned()));

    for forbidden in [
        "--yolo",
        "--dangerously-bypass-approvals-and-sandbox",
        "--full-auto",
        "--ask-for-approval=never",
        "-a",
        "--sandbox=danger-full-access",
        "--auto-merge",
        "--push",
    ] {
        let cfg = CodexAdapterConfig {
            extra_args: vec![forbidden.to_owned()],
            ..Default::default()
        };
        assert_eq!(
            cfg.validate().unwrap_err().code,
            "forbidden_host_flag",
            "{forbidden}"
        );
        let bad_adapter = CodexAdapter::with_runner(
            cfg,
            Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex")))),
        );
        assert!(!bad_adapter.probe().available, "{forbidden}");
    }
}

#[test]
fn codex_adapter_executes_using_probed_flags_and_extracts_session_and_result() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let full_help = "\
Usage: codex exec [OPTIONS] [PROMPT]
      --json
      --output-schema <FILE>
      --output-last-message <FILE>
      --cd <DIR>
      --sandbox <MODE>
      --ephemeral
      -  Read prompt from stdin
";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    // probe() during dispatch_and_run, then probe() inside execute()
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, full_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": 1,
        "status": "succeeded",
        "report": "implemented task a safely",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_last_message(&result_json);
    let jsonl_stdout = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"th_12345\"}}\n{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
        serde_json::to_string(&result_json).unwrap()
    );
    runner.push_ok(0, &jsonl_stdout, "", false, false);

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner.clone());
    let cancel = CancelToken::new();
    let report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &fixture.root,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();

    assert_eq!(report.status, ReconcileStatus::Applied);
    assert_eq!(report.session_id.as_deref(), Some("th_12345"));
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Succeeded);

    let calls = runner.calls();
    let exec_call = calls.last().unwrap();
    assert!(exec_call.args.contains(&"--cd".to_owned()));
    assert!(exec_call.args.contains(&"--sandbox".to_owned()));
    assert!(exec_call.args.contains(&"workspace-write".to_owned()));
    assert!(exec_call.args.contains(&"--json".to_owned()));
    assert!(exec_call.args.contains(&"--output-schema".to_owned()));
    assert!(exec_call.args.contains(&"--output-last-message".to_owned()));
    assert!(exec_call.args.contains(&"--ephemeral".to_owned()));
    assert_eq!(exec_call.args.last().map(String::as_str), Some("-"));
    assert!(exec_call.stdin.is_some());
}

#[test]
fn codex_adapter_handles_minimal_flags_timeout_cancel_crash_invalid_json_and_missing_binary() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 10), &clock);

    // 1. Real missing executable returns HostUnavailable without touching attempt budget.
    let missing_adapter = CodexAdapter::with_config(CodexAdapterConfig {
        executable: Some(fixture.root.join("nonexistent-codex-bin")),
        ..CodexAdapterConfig::default()
    });
    let cancel = CancelToken::new();
    let missing_report = run_task(
        &mut journal,
        &missing_adapter,
        "a",
        "codex-host",
        &fixture.root,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(missing_report.status, ReconcileStatus::HostUnavailable);
    assert_eq!(missing_report.attempt_id, None);

    // 2. Minimal CLI (no --json, no --output-file, no stdin flag) passes prompt as arg.
    let minimal_help = "Usage: codex exec [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.1.0\n", "", false, false);
        runner.push_ok(0, minimal_help, "", false, false);
    }
    let bad_json_stdout = "not valid json";
    runner.push_ok(0, bad_json_stdout, "", false, false);

    let minimal_adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner.clone());
    let invalid_report = run_task(
        &mut journal,
        &minimal_adapter,
        "a",
        "codex-host",
        &fixture.root,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(invalid_report.status, ReconcileStatus::Blocked);
    assert_eq!(invalid_report.code, "invalid_output");
    let exec_call = runner.calls().pop().unwrap();
    assert!(!exec_call.args.contains(&"--json".to_owned()));
    assert!(exec_call.stdin.is_none());

    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry after invalid output",
            &clock,
        )
        .unwrap();

    // 3. Timeout and Crash and Supervision Cleanup Error (worker_stopped = false).
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.1.0\n", "", false, false);
        runner.push_ok(0, minimal_help, "", false, false);
    }
    runner.push_ok(1, "", "timed out", true, false);
    let timeout_report = run_task(
        &mut journal,
        &minimal_adapter,
        "a",
        "codex-host",
        &fixture.root,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(timeout_report.status, ReconcileStatus::Blocked);
    assert_eq!(timeout_report.code, "worker_timed_out");
    assert!(timeout_report.worker_stopped);

    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry after timeout",
            &clock,
        )
        .unwrap();

    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.1.0\n", "", false, false);
        runner.push_ok(0, minimal_help, "", false, false);
    }
    runner.push_err(io::ErrorKind::TimedOut, "process tree cleanup failed");
    let unstopped_report = run_task(
        &mut journal,
        &minimal_adapter,
        "a",
        "codex-host",
        &fixture.root,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(unstopped_report.status, ReconcileStatus::BlockedUnstopped);
    assert!(!unstopped_report.worker_stopped);
}

#[test]
#[ignore]
fn codex_live_smoke() {
    let fixture = Fixture::new();
    let clock = SystemClock;
    let mut journal = fixture.configured(policy(1, 1), &clock);
    let adapter = CodexAdapter::new();
    let caps = adapter.probe();
    assert!(
        caps.available,
        "codex must be installed and on PATH for smoke test: {:?}",
        caps.unavailable_reason
    );
    let cancel = CancelToken::new();
    let trev = task_rev(&journal, "a");
    let report = dispatch_and_run(
        &mut journal,
        &adapter,
        "a",
        trev,
        "codex-smoke",
        &fixture.root,
        Duration::from_secs(60),
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");
    assert!(report.worker_stopped);
}
