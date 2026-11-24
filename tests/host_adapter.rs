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
        ADAPTER_SCHEMA, AdapterOutcome, AdapterRequest, CancelToken, CodexAdapter,
        CodexAdapterConfig, ExecutionPolicy, FakeAdapter, FakeBehavior, HostAdapter,
        ReconcileReport, ReconcileStatus, SessionCorrelation,
        codex::{CodexCommandRunner, ProcessCommandRunner},
        dispatch_and_run, reconcile_outcome,
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

    fn isolated_workdir(&self, name: &str) -> PathBuf {
        let dir = self.root.join("worktrees").join(name);
        fs::create_dir_all(dir.join("app")).unwrap();
        dir
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
    workspace_mutation: Mutex<Option<(PathBuf, String)>>,
    permission_mutation: Mutex<Option<PathBuf>>,
}

impl MockCodexRunner {
    fn new(executable: Result<PathBuf, String>) -> Self {
        Self {
            executable,
            responses: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            write_last_message: Mutex::new(None),
            workspace_mutation: Mutex::new(None),
            permission_mutation: Mutex::new(None),
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

    fn set_workspace_mutation(&self, rel_path: &str, content: &str) {
        *self.workspace_mutation.lock().unwrap() =
            Some((PathBuf::from(rel_path), content.to_owned()));
    }

    fn set_permission_mutation(&self, rel_path: &str) {
        *self.permission_mutation.lock().unwrap() = Some(PathBuf::from(rel_path));
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
        {
            if let Some((rel, content)) = self.workspace_mutation.lock().unwrap().clone() {
                let target = _cwd.join(rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(target, content).unwrap();
            }
            if let Some(rel) = self.permission_mutation.lock().unwrap().clone() {
                let target = _cwd.join(rel);
                let mut perms = fs::metadata(&target).unwrap().permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let flipped = perms.mode() ^ 0o111;
                    perms.set_mode(flipped);
                }
                #[cfg(not(unix))]
                {
                    perms.set_readonly(true);
                }
                fs::set_permissions(target, perms).unwrap();
            }
            if let Some(msg) = self.write_last_message.lock().unwrap().clone()
                && let Some(pos) = spec.args.iter().position(|a| a == "--output-last-message")
                && let Some(path) = spec.args.get(pos + 1)
            {
                fs::write(path, msg).unwrap();
            }
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
fn misrouted_outcomes_cannot_free_active_leases() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 3), &clock);
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "host-1",
            &clock,
        )
        .unwrap();
    let rev_before = journal.view().revision.unwrap();

    let wrong_corr = SessionCorrelation {
        attempt_id: "other-attempt".to_owned(),
        lease_id: dispatch.lease.lease_id.clone(),
        owner: dispatch.lease.owner.clone(),
        session_id: "sess-1".to_owned(),
    };
    let wrong_lease = SessionCorrelation {
        attempt_id: dispatch.lease.attempt_id.clone(),
        lease_id: "other-lease".to_owned(),
        owner: dispatch.lease.owner.clone(),
        session_id: "sess-2".to_owned(),
    };
    let wrong_owner = SessionCorrelation {
        attempt_id: dispatch.lease.attempt_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        owner: "other-host".to_owned(),
        session_id: "sess-3".to_owned(),
    };

    let misrouted_outcomes = [
        AdapterOutcome::Completed {
            correlation: wrong_lease.clone(),
            result: WorkerResult {
                schema: SCHEMA,
                story: STORY.to_owned(),
                task: "a".to_owned(),
                task_revision: dispatch.packet.task.revision,
                status: ResultStatus::Succeeded,
                report: "forged".to_owned(),
                commit: None,
                artifacts: Vec::new(),
            },
            worker_stopped: true,
        },
        AdapterOutcome::Cancelled {
            correlation: wrong_corr,
            worker_stopped: true,
            reason: "stale cancel".to_owned(),
        },
        AdapterOutcome::TimedOut {
            correlation: wrong_lease,
            worker_stopped: true,
            reason: "stale timeout".to_owned(),
        },
        AdapterOutcome::HostCrashed {
            correlation: wrong_owner.clone(),
            worker_stopped: true,
            exit_code: Some(1),
            detail: "stale crash".to_owned(),
        },
        AdapterOutcome::InvalidOutput {
            correlation: wrong_owner,
            worker_stopped: true,
            detail: "stale invalid output".to_owned(),
        },
    ];

    for outcome in misrouted_outcomes {
        let report = reconcile_outcome(&mut journal, &dispatch, outcome, &clock).unwrap();
        assert_eq!(report.status, ReconcileStatus::RejectedDuplicateOrStale);
        assert_eq!(report.code, "correlation_mismatch");
        assert!(!report.worker_stopped);
        assert!(!report.recoverable);
        assert_eq!(journal.view().revision.unwrap(), rev_before);
        assert_eq!(journal.view().tasks["a"].status, TaskStatus::Running);
        assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 1);
    }
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
    let workdir = fixture.isolated_workdir("wt-a");
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
    runner.set_workspace_mutation("app/a.txt", "in-scope change\n");
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
        &workdir,
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
    let workdir = fixture.isolated_workdir("wt-minimal");
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
        &workdir,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(missing_report.status, ReconcileStatus::HostUnavailable);
    assert_eq!(missing_report.attempt_id, None);

    // 2. Minimal CLI (with required --sandbox, but no --json, no --output-file, no stdin flag) passes prompt as arg.
    let minimal_help = "Usage: codex exec --sandbox <MODE> [PROMPT]\n";
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
        &workdir,
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
        &workdir,
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
        &workdir,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(unstopped_report.status, ReconcileStatus::BlockedUnstopped);
    assert!(!unstopped_report.worker_stopped);
}

#[test]
fn codex_adapter_fails_closed_when_sandbox_flag_is_missing() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-no-sandbox");
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let no_sandbox_help = "Usage: codex exec --json [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner.push_ok(0, "codex-cli 0.1.0\n", "", false, false);
    runner.push_ok(0, no_sandbox_help, "", false, false);
    runner.push_ok(0, "codex-cli 0.1.0\n", "", false, false);
    runner.push_ok(0, no_sandbox_help, "", false, false);

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner.clone());
    let caps = adapter.probe();
    assert!(!caps.available);
    assert!(!caps.supports_sandbox_flag);
    assert!(
        caps.unavailable_reason
            .as_deref()
            .unwrap_or_default()
            .contains("--sandbox")
    );

    let cancel = CancelToken::new();
    let report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::HostUnavailable);
    assert_eq!(report.attempt_id, None);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Ready);
}

#[test]
fn codex_adapter_enforces_workspace_isolation_and_write_scope() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-scope");
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 5), &clock);

    let sandbox_help = "Usage: codex exec --sandbox <MODE> --json [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner.clone());
    let cancel = CancelToken::new();

    // 1. Refuse WorkspaceWrite directly in repo root containing .storyflow/.
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, sandbox_help, "", false, false);
    let root_req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: fixture.root.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let root_outcome = adapter.execute(&root_req, &cancel);
    assert!(matches!(
        root_outcome,
        AdapterOutcome::HostUnavailable { ref detail, .. } if detail.contains(".storyflow/")
    ));
    let root_report = reconcile_outcome(&mut journal, &dispatch, root_outcome, &clock).unwrap();
    assert_eq!(root_report.status, ReconcileStatus::HostUnavailable);

    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry in isolated worktree",
            &clock,
        )
        .unwrap();

    // 2. Reject out-of-scope modification under WorkspaceWrite (task a touches app/b.txt).
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "claimed success despite out-of-scope write",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/b.txt", "out of scope edit\n");
    runner.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&result_json).unwrap()
        ),
        "",
        false,
        false,
    );
    let oos_report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(oos_report.status, ReconcileStatus::Blocked);
    assert_eq!(oos_report.code, "invalid_output");
    assert!(oos_report.detail.contains("app/b.txt"));
    assert!(!workdir.join("app/b.txt").exists());
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);

    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry under read-only",
            &clock,
        )
        .unwrap();

    // 3. Reject any workspace modification under ReadOnly even if inside write_scope.
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let ro_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "modified file during read-only",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "unexpected write in read-only\n");
    runner.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&ro_json).unwrap()
        ),
        "",
        false,
        false,
    );
    let ro_report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(ro_report.status, ReconcileStatus::Blocked);
    assert_eq!(ro_report.code, "invalid_output");
    assert!(ro_report.detail.contains("app/a.txt"));
    assert!(!workdir.join("app/a.txt").exists());

    // 3b. Even when Codex crashes (exit 1) without JSON, out-of-scope edits are detected & rolled back.
    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry crash rollback check",
            &clock,
        )
        .unwrap();
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    runner.set_workspace_mutation("app/b.txt", "dirty file left by crashed worker\n");
    runner.push_ok(1, "", "worker crashed", false, false);
    let crash_report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(crash_report.status, ReconcileStatus::Blocked);
    assert_eq!(crash_report.code, "host_crashed");
    assert!(crash_report.detail.contains("app/b.txt"));
    assert!(crash_report.detail.contains("restored"));
    assert!(!workdir.join("app/b.txt").exists());

    // 4. Refuse WorkspaceWrite when task write_scope is empty.
    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry with empty write_scope check",
            &clock,
        )
        .unwrap();
    let mut empty_scope_dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();
    empty_scope_dispatch.packet.task.spec.write_scope.clear();
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, sandbox_help, "", false, false);
    let empty_req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: empty_scope_dispatch.clone(),
        workdir: fixture.isolated_workdir("wt-empty"),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let empty_outcome = adapter.execute(&empty_req, &cancel);
    assert!(matches!(
        empty_outcome,
        AdapterOutcome::HostUnavailable { ref detail, .. } if detail.contains("empty write_scope")
    ));
    let empty_report =
        reconcile_outcome(&mut journal, &empty_scope_dispatch, empty_outcome, &clock).unwrap();
    assert_eq!(empty_report.status, ReconcileStatus::HostUnavailable);
}

#[test]
fn codex_adapter_protects_root_git_metadata_and_resolves_relative_executable() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-git");
    fs::create_dir_all(workdir.join(".git")).unwrap();
    fs::write(workdir.join(".git/config"), "[core]\n\tbare = false\n").unwrap();

    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let sandbox_help = "Usage: codex exec --sandbox <MODE> --json [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": 1,
        "status": "succeeded",
        "report": "attempted .git/config edit",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation(".git/config", "[core]\n\tfsmonitor = malicious\n");
    runner.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&result_json).unwrap()
        ),
        "",
        false,
        false,
    );

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner);
    let cancel = CancelToken::new();
    let report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::Blocked);
    assert_eq!(report.code, "invalid_output");
    assert!(report.detail.contains(".git/config"));
    assert_eq!(
        fs::read_to_string(workdir.join(".git/config")).unwrap(),
        "[core]\n\tbare = false\n"
    );

    // Relative configured executable path must be canonicalized to an absolute path.
    let bin_path = fixture.root.join("codex-mock-bin");
    fs::write(&bin_path, "placeholder").unwrap();
    let cwd = std::env::current_dir().unwrap();
    if let Ok(rel_path) = bin_path.strip_prefix(&cwd) {
        let resolved = ProcessCommandRunner
            .resolve_executable(Some(rel_path))
            .expect("relative executable should resolve");
        assert!(resolved.is_absolute());
        assert_eq!(resolved, fs::canonicalize(&bin_path).unwrap());
    }
}

#[test]
fn codex_adapter_detects_permission_changes_and_preserves_contract_on_prompt_truncation() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-perms");
    let protected_target = workdir.join("app/b.txt");
    fs::write(&protected_target, "unchanged content\n").unwrap();
    let orig_perms = fs::metadata(&protected_target).unwrap().permissions();

    let clock = FakeClock::at(100);
    let mut big_spec = spec("a", &[]);
    big_spec.acceptance_criteria = (0..64)
        .map(|i| format!("AC-{i:02}: {}", "x".repeat(500)))
        .collect();
    let big_graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![big_spec],
    };
    let mut journal = TaskJournal::open(&fixture.root, fixture.context.clone()).unwrap();
    journal.initialize(big_graph).unwrap();
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 2), &clock)
        .unwrap();

    let sandbox_help = "Usage: codex exec --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": 1,
        "status": "succeeded",
        "report": "chmod only",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_permission_mutation("app/b.txt");
    runner.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&result_json).unwrap()
        ),
        "",
        false,
        false,
    );

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner.clone());
    let cancel = CancelToken::new();
    let report = run_task(
        &mut journal,
        &adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(
        fs::metadata(&protected_target).unwrap().permissions(),
        orig_perms,
        "rollback must restore original file permissions automatically"
    );

    assert_eq!(report.status, ReconcileStatus::Blocked);
    assert_eq!(report.code, "invalid_output");
    assert!(report.detail.contains("app/b.txt"));

    // All 64 acceptance criteria + Constraints & JSON contract are preserved intact.
    let calls = runner.calls();
    let exec_call = calls.last().unwrap();
    let prompt_bytes = exec_call.stdin.as_ref().unwrap();
    assert!(prompt_bytes.len() <= 65_536);
    let prompt_str = std::str::from_utf8(prompt_bytes).unwrap();
    assert!(prompt_str.contains("AC-00:"));
    assert!(prompt_str.contains("AC-63:"));
    assert!(prompt_str.contains("\nConstraints:\n"));
    assert!(prompt_str.contains("Return ONLY a JSON object with fields: story"));

    // When huge prerequisites push total size past 65_536 bytes, only prerequisites are truncated.
    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry with large prerequisites",
            &clock,
        )
        .unwrap();
    let mut big_dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();
    for i in 0..600 {
        let dep_id = format!("dep-{i:03}");
        big_dispatch.packet.prerequisites.insert(
            dep_id.clone(),
            WorkerResult {
                schema: SCHEMA,
                story: STORY.into(),
                task: dep_id,
                task_revision: 1,
                status: ResultStatus::Succeeded,
                report: "ok".to_owned(),
                commit: Some("a".repeat(40)),
                artifacts: Vec::new(),
            },
        );
    }
    let runner2 = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner2.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner2.push_ok(0, sandbox_help, "", false, false);
    runner2.push_ok(
        0,
        "{\"type\":\"thread.started\",\"thread_id\":\"th_ok\"}\n",
        "",
        false,
        false,
    );
    let adapter2 = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner2.clone());
    let big_req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: big_dispatch,
        workdir: workdir.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::ReadOnly,
    };
    let _ = adapter2.execute(&big_req, &cancel);
    let big_call = runner2.calls().pop().unwrap();
    let big_prompt_bytes = big_call.stdin.as_ref().unwrap();
    assert!(big_prompt_bytes.len() <= 65_536);
    let big_prompt = std::str::from_utf8(big_prompt_bytes).unwrap();
    assert!(big_prompt.contains("AC-00:"));
    assert!(big_prompt.contains("AC-63:"));
    assert!(big_prompt.contains("...[prerequisites truncated]..."));
    assert!(big_prompt.contains("\nConstraints:\n"));
    assert!(big_prompt.contains("Return ONLY a JSON object with fields: story"));
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
