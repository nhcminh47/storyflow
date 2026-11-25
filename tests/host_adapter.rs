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
    integration::{IntegrationCoordinator, IntegrationPhase},
    security::process::{
        ControlledResult, HostCommandSpec, ProcessError, ProcessResult, git_executable,
    },
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
fn fixture_base_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
        .join("host-adapter-tests")
}

impl Fixture {
    fn new() -> Self {
        let root = fixture_base_dir().join(format!(
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
        let allowed = fixture_base_dir();
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
    remove_staged_paths: Mutex<Vec<PathBuf>>,
    permission_mutation: Mutex<Option<PathBuf>>,
    #[cfg(unix)]
    chmod_zero_targets: Mutex<Vec<PathBuf>>,
    observed_exec_paths: Mutex<Vec<Vec<String>>>,
    unbroken_containment: bool,
    read_root_confinement: bool,
    ambient_legacy_sandbox: bool,
}

impl MockCodexRunner {
    fn new(executable: Result<PathBuf, String>) -> Self {
        Self {
            executable,
            responses: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            write_last_message: Mutex::new(None),
            workspace_mutation: Mutex::new(None),
            remove_staged_paths: Mutex::new(Vec::new()),
            permission_mutation: Mutex::new(None),
            #[cfg(unix)]
            chmod_zero_targets: Mutex::new(Vec::new()),
            observed_exec_paths: Mutex::new(Vec::new()),
            unbroken_containment: true,
            read_root_confinement: true,
            ambient_legacy_sandbox: false,
        }
    }

    fn with_containment(mut self, unbroken: bool) -> Self {
        self.unbroken_containment = unbroken;
        self
    }

    fn with_read_root_confinement(mut self, confined: bool) -> Self {
        self.read_root_confinement = confined;
        self
    }

    fn with_ambient_legacy_sandbox(mut self, ambient: bool) -> Self {
        self.ambient_legacy_sandbox = ambient;
        self
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

    fn push_pre_launch_cancelled(&self) {
        self.responses.lock().unwrap().push(Ok(ControlledResult {
            process: ProcessResult {
                status: None,
                timed_out: false,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            },
            cancelled: true,
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

    fn set_remove_staged_paths(&self, rel_paths: &[&str]) {
        *self.remove_staged_paths.lock().unwrap() = rel_paths.iter().map(PathBuf::from).collect();
    }

    fn set_permission_mutation(&self, rel_path: &str) {
        *self.permission_mutation.lock().unwrap() = Some(PathBuf::from(rel_path));
    }

    #[cfg(unix)]
    fn set_chmod_zero_targets(&self, rel_paths: &[&str]) {
        *self.chmod_zero_targets.lock().unwrap() = rel_paths.iter().map(PathBuf::from).collect();
    }

    fn observed_exec_paths(&self) -> Vec<Vec<String>> {
        self.observed_exec_paths.lock().unwrap().clone()
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
            let mut seen = Vec::new();
            collect_rel_paths(_cwd, _cwd, &mut seen);
            self.observed_exec_paths.lock().unwrap().push(seen);

            for rel in self.remove_staged_paths.lock().unwrap().drain(..) {
                let target = _cwd.join(rel);
                if let Ok(meta) = fs::symlink_metadata(&target) {
                    if meta.file_type().is_dir() {
                        fs::remove_dir_all(&target).unwrap();
                    } else {
                        fs::remove_file(&target).unwrap();
                    }
                }
            }
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for rel in self.chmod_zero_targets.lock().unwrap().clone() {
                    let target = _cwd.join(rel);
                    let mut perms = fs::metadata(&target).unwrap().permissions();
                    perms.set_mode(0o000);
                    fs::set_permissions(target, perms).unwrap();
                }
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

    fn has_unbroken_containment(&self) -> bool {
        self.unbroken_containment
    }

    fn has_read_root_confinement(&self) -> bool {
        self.read_root_confinement
    }

    fn ambient_config_sets_legacy_sandbox(&self) -> bool {
        self.ambient_legacy_sandbox
    }
}

fn collect_rel_paths(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if let Ok(rel) = path.strip_prefix(root) {
            out.push(
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
        if path.is_dir() {
            collect_rel_paths(root, &path, out);
        }
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
        "-anever",
        "--sandbox=danger-full-access",
        "--add-dir",
        "--add-dir=/tmp",
        "--config",
        "-c",
        "-csandbox_mode=danger-full-access",
        "--profile",
        "-p",
        "--enable",
        "--disable",
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
    let mut journal = fixture.configured(policy(1, 10), &clock);

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

    // 5. Refuse WorkspaceWrite when rollback backup coverage is incomplete.
    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry backup budget check",
            &clock,
        )
        .unwrap();
    let bounded_workdir = fixture.isolated_workdir("wt-backup-cap");
    fs::write(
        bounded_workdir.join("app/existing.txt"),
        "0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let bounded_runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    bounded_runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    bounded_runner.push_ok(0, sandbox_help, "", false, false);
    let bounded_adapter = CodexAdapter::with_runner(
        CodexAdapterConfig {
            max_backup_bytes: 16,
            ..Default::default()
        },
        bounded_runner.clone(),
    );
    let cap_dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();
    let cap_req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: cap_dispatch.clone(),
        workdir: bounded_workdir,
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let cap_outcome = bounded_adapter.execute(&cap_req, &cancel);
    assert!(matches!(
        cap_outcome,
        AdapterOutcome::HostUnavailable { ref detail, .. } if detail.contains("rollback backup limit")
    ));
    let cap_report = reconcile_outcome(&mut journal, &cap_dispatch, cap_outcome, &clock).unwrap();
    assert_eq!(cap_report.status, ReconcileStatus::HostUnavailable);
    // Probe ran (2 calls), exec was never invoked.
    assert_eq!(bounded_runner.calls().len(), 2);
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
fn codex_adapter_enforces_read_scope_and_promotes_only_write_scope() {
    assert!(!ProcessCommandRunner.has_read_root_confinement());

    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-read-scope");
    fs::write(workdir.join("app/input.txt"), "allowed context\n").unwrap();
    fs::create_dir_all(workdir.join("secrets")).unwrap();
    fs::write(workdir.join("secrets/token.txt"), "top-secret-token\n").unwrap();
    fs::create_dir_all(workdir.join(".git")).unwrap();
    fs::write(workdir.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    fs::write(workdir.join("AGENTS.md"), "# Agent guide\n").unwrap();

    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 3), &clock);

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";

    // Without read-root confinement or `--config`, probes refuse execution fail-closed before launching codex
    // (even when the workdir contains only scoped files, because legacy `--sandbox` allows reading absolute paths outside workdir).
    let clean_only_workdir = fixture.isolated_workdir("wt-clean-only");
    fs::write(
        clean_only_workdir.join("app/input.txt"),
        "allowed context\n",
    )
    .unwrap();

    let unconfined_runner = Arc::new(
        MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_read_root_confinement(false),
    );
    unconfined_runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    unconfined_runner.push_ok(0, sandbox_help, "", false, false);
    let unconfined_adapter =
        CodexAdapter::with_runner(CodexAdapterConfig::default(), unconfined_runner.clone());
    let cancel = CancelToken::new();
    let refused_clean = run_task(
        &mut journal,
        &unconfined_adapter,
        "a",
        "codex-host",
        &clean_only_workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(refused_clean.status, ReconcileStatus::HostUnavailable);
    assert_eq!(refused_clean.attempt_id, None);
    assert!(
        refused_clean.detail.contains("--config")
            || refused_clean.detail.contains("read-root confinement"),
        "{refused_clean:?}"
    );
    assert_eq!(unconfined_runner.calls().len(), 2);
    assert!(unconfined_runner.observed_exec_paths().is_empty());

    // Direct `Adapter::execute` also refuses execution fail-closed when read-root confinement is unavailable.
    let direct_unconfined_runner = Arc::new(
        MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_read_root_confinement(false),
    );
    direct_unconfined_runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    direct_unconfined_runner.push_ok(0, sandbox_help, "", false, false);
    let direct_unconfined = CodexAdapter::with_runner(
        CodexAdapterConfig::default(),
        direct_unconfined_runner.clone(),
    );
    let direct_dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();
    let direct_req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: direct_dispatch.clone(),
        workdir: workdir.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let direct_outcome = direct_unconfined.execute(&direct_req, &cancel);
    match direct_outcome {
        AdapterOutcome::HostUnavailable { ref detail, .. } => {
            assert!(
                detail.contains("--config") || detail.contains("read-root confinement"),
                "{detail}"
            );
        }
        other => panic!("expected HostUnavailable for unconfined execute, got {other:?}"),
    }
    assert!(direct_unconfined_runner.observed_exec_paths().is_empty());
    let settled =
        reconcile_outcome(&mut journal, &direct_dispatch, direct_outcome, &clock).unwrap();
    assert_eq!(settled.status, ReconcileStatus::HostUnavailable);
    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "retry with confined runner",
            &clock,
        )
        .unwrap();

    // When runner enforces read-root confinement, only read_scope entries are staged and only write_scope is promoted.
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "wrote allowed file only",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "promoted change\n");
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
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");
    assert_eq!(
        fs::read_to_string(workdir.join("app/a.txt")).unwrap(),
        "promoted change\n"
    );

    let seen = runner.observed_exec_paths();
    assert_eq!(seen.len(), 1);
    let staged = &seen[0];
    assert!(staged.contains(&"app/input.txt".to_owned()), "{staged:?}");
    assert!(
        !staged
            .iter()
            .any(|p| p.starts_with("secrets") || p.starts_with(".git") || p == "AGENTS.md"),
        "out-of-scope or protected paths must not exist in staged cwd: {staged:?}"
    );
}

#[test]
fn codex_adapter_does_not_promote_writes_when_worker_result_is_invalid() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-invalid-result");
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    // Wrong task_revision triggers WorkerResult::validate failure.
    let invalid_result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": 999,
        "status": "succeeded",
        "report": "wrong revision result",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "should not leak to workdir\n");
    runner.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&invalid_result_json).unwrap()
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
    assert!(
        !workdir.join("app/a.txt").exists(),
        "invalid worker result must not promote staged writes to workdir"
    );
}

#[cfg(unix)]
#[test]
fn codex_adapter_recovers_unreadable_post_run_mutations() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-chmod-zero");
    fs::write(workdir.join("app/b.txt"), "protected content\n").unwrap();
    fs::create_dir_all(workdir.join("app/locked_dir")).unwrap();
    fs::write(workdir.join("app/locked_dir/nested.txt"), "nested\n").unwrap();

    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);
    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
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
        "report": "chmod 000 out of scope paths",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_chmod_zero_targets(&["app/b.txt", "app/locked_dir"]);
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
    assert!(report.detail.contains("restored"), "{}", report.detail);
    assert!(report.detail.contains("app/b.txt"), "{}", report.detail);
    assert!(
        report.detail.contains("app/locked_dir"),
        "{}",
        report.detail
    );
}

#[test]
fn codex_adapter_reports_unstopped_when_containment_is_escapable() {
    assert_eq!(
        ProcessCommandRunner.has_unbroken_containment(),
        cfg!(windows)
    );

    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-escapable");
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_containment(false));
    for _ in 0..3 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": 1,
        "status": "succeeded",
        "report": "completed while descendants unconfirmed",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "should not be promoted\n");
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
    let caps = adapter.probe();
    assert!(caps.available);
    assert!(!caps.supports_cancel);

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
    assert_eq!(report.status, ReconcileStatus::BlockedUnstopped);
    assert!(!report.worker_stopped);
    assert!(!report.recoverable);
    assert!(
        !workdir.join("app/a.txt").exists(),
        "unconfirmed worker writes must not be promoted to workdir"
    );
}

#[test]
fn codex_adapter_respects_case_sensitivity_in_task_scopes() {
    let fixture = Fixture::new();
    let workdir = fixture.root.join("worktrees/wt-case");
    fs::create_dir_all(workdir.join("secret")).unwrap();
    fs::write(workdir.join("secret/token.txt"), "lowercase secret\n").unwrap();
    #[cfg(unix)]
    {
        fs::create_dir_all(workdir.join("Secret")).unwrap();
        fs::write(workdir.join("Secret/allowed.txt"), "exact case\n").unwrap();
    }

    let clock = FakeClock::at(100);
    let mut case_spec = TaskSpec {
        id: "a".into(),
        story: STORY.into(),
        prerequisites: Vec::new(),
        stage: TaskStage::Plan,
        read_scope: vec!["Secret".into()],
        write_scope: vec!["Secret/out.txt".into()],
        acceptance_criteria: vec!["AC1 verified".into()],
        decision_owner: "orchestrator".into(),
    };
    let case_graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![case_spec.clone()],
    };
    let mut case_context = fixture.context.clone();
    case_context.scope = vec!["Secret".into()];
    let mut journal = TaskJournal::open(&fixture.root, case_context).unwrap();
    journal.initialize(case_graph).unwrap();
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 5), &clock)
        .unwrap();

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";

    // Unconfined runner without `--config` must refuse execution fail-closed before launching codex.
    let unconfined = Arc::new(
        MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_read_root_confinement(false),
    );
    unconfined.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    unconfined.push_ok(0, sandbox_help, "", false, false);
    let cancel = CancelToken::new();
    let unconfined_adapter =
        CodexAdapter::with_runner(CodexAdapterConfig::default(), unconfined.clone());
    let refused = run_task(
        &mut journal,
        &unconfined_adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(refused.status, ReconcileStatus::HostUnavailable);
    assert_eq!(refused.attempt_id, None);
    assert!(
        refused.detail.contains("--config") || refused.detail.contains("read-root confinement"),
        "{refused:?}"
    );
    assert!(unconfined.observed_exec_paths().is_empty());

    // Confined runner excludes `secret/token.txt` from staging and blocks lowercase write `secret/out.txt`.
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    case_spec.id = "a".into();
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "attempted wrong-case write",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("secret/out.txt", "wrong case leak\n");
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
    let wrong_case = run_task(
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
    assert_eq!(wrong_case.status, ReconcileStatus::Blocked);
    assert_eq!(wrong_case.code, "invalid_output");
    assert!(
        wrong_case.detail.contains("secret/out.txt"),
        "{wrong_case:?}"
    );
    let seen = runner.observed_exec_paths();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].iter().any(|p| p == "secret/token.txt"),
        "wrong-case entry must not be staged: {:?}",
        seen[0]
    );
    #[cfg(unix)]
    assert!(
        seen[0].contains(&"Secret/allowed.txt".to_owned()),
        "{:?}",
        seen[0]
    );
}

#[test]
fn codex_adapter_commits_promoted_writes_for_integration() {
    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(git_executable())
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    let fixture = Fixture::new();
    let repo_root = fixture.root.join("git-repo");
    fs::create_dir_all(repo_root.join("app")).unwrap();
    fs::write(repo_root.join(".gitignore"), ".storyflow/\n").unwrap();
    fs::write(repo_root.join("app/seed.txt"), "seed\n").unwrap();

    run_git(&repo_root, &["init", "-b", "master"]);
    run_git(&repo_root, &["config", "user.name", "test"]);
    run_git(&repo_root, &["config", "user.email", "test@local"]);
    run_git(&repo_root, &["add", ".gitignore", "app/seed.txt"]);
    run_git(&repo_root, &["commit", "-m", "initial baseline"]);
    let base = run_git(&repo_root, &["rev-parse", "HEAD"]);
    fs::create_dir_all(repo_root.join(format!(".storyflow/{STORY}/events"))).unwrap();

    let context = RunContext {
        story: STORY.into(),
        base: base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        blocked: false,
        scope: vec!["app".into()],
        protected: Vec::new(),
    };
    let mut impl_spec = spec("a", &[]);
    impl_spec.stage = TaskStage::Implement;
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base,
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![impl_spec],
    };

    let mut journal = TaskJournal::open(&repo_root, context.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();
    let clock = FakeClock::at(100);
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 2), &clock)
        .unwrap();
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();

    let coordinator = IntegrationCoordinator::new(&repo_root, context).unwrap();
    let worktree = coordinator
        .prepare_worktree(journal.view(), Some(&graph), "a")
        .unwrap();
    let workdir = repo_root.join(&worktree.path);

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, sandbox_help, "", false, false);
    let forged_commit = "f".repeat(40);
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": dispatch.packet.task.revision,
        "status": "succeeded",
        "report": "created app/a.txt in staged dir",
        "commit": forged_commit,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "implemented in git worktree\n");
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
    let req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: workdir.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome = adapter.execute(&req, &cancel);
    match &outcome {
        AdapterOutcome::Completed { result, .. } => {
            let pinned = result
                .commit
                .as_deref()
                .expect("adapter must pin commit in git worktree");
            assert_ne!(
                pinned, forged_commit,
                "untrusted worker commit must be replaced by Rust-authored commit"
            );
        }
        other => panic!("expected Completed outcome, got {other:?}"),
    }

    let report = reconcile_outcome(&mut journal, &dispatch, outcome, &clock).unwrap();
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");

    let status = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(
        status.phase,
        IntegrationPhase::Ready,
        "{:?}",
        status.blockers
    );
    assert!(status.candidate_commit.is_some());
}

#[cfg(unix)]
#[test]
fn codex_adapter_rejects_non_utf8_workspace_paths() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-non-utf8");
    fs::create_dir_all(workdir.join("secrets")).unwrap();
    let raw_name = OsStr::from_bytes(b"\xff");
    fs::write(workdir.join("secrets").join(raw_name), b"secret\n").unwrap();

    let mut ctx = fixture.context.clone();
    ctx.scope = vec!["app".into(), "secrets".into()];
    let mut bad_spec = spec("a", &[]);
    bad_spec.read_scope = vec!["secrets/\u{fffd}".into()];
    let bad_graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![bad_spec],
    };

    let clock = FakeClock::at(100);
    let mut journal = TaskJournal::open(&fixture.root, ctx).unwrap();
    journal.initialize(bad_graph).unwrap();
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 3), &clock)
        .unwrap();

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
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
    assert_eq!(report.status, ReconcileStatus::Blocked);
    assert!(report.detail.contains("non-UTF-8"), "{report:?}");
    assert!(runner.observed_exec_paths().is_empty());
}

#[test]
fn dispatch_and_run_validates_workdir_and_timeout_before_reserving_lease() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);
    let rev_before = journal.view().revision.unwrap();
    let trev = task_rev(&journal, "a");
    let adapter = FakeAdapter::available("fake");
    let cancel = CancelToken::new();

    let missing_dir = fixture.root.join("does-not-exist");
    let err_dir = dispatch_and_run(
        &mut journal,
        &adapter,
        "a",
        trev,
        "host-1",
        &missing_dir,
        Duration::from_secs(5),
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap_err();
    assert_eq!(err_dir.code, "invalid_workdir");

    let file_path = fixture.root.join("app/not-a-dir.txt");
    fs::write(&file_path, "file\n").unwrap();
    let err_file = dispatch_and_run(
        &mut journal,
        &adapter,
        "a",
        trev,
        "host-1",
        &file_path,
        Duration::from_secs(5),
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap_err();
    assert_eq!(err_file.code, "invalid_workdir");

    let err_zero = dispatch_and_run(
        &mut journal,
        &adapter,
        "a",
        trev,
        "host-1",
        &fixture.root,
        Duration::ZERO,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap_err();
    assert_eq!(err_zero.code, "invalid_timeout");

    let err_subms = dispatch_and_run(
        &mut journal,
        &adapter,
        "a",
        trev,
        "host-1",
        &fixture.root,
        Duration::from_micros(500),
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap_err();
    assert_eq!(err_subms.code, "invalid_timeout");

    assert_eq!(journal.view().revision.unwrap(), rev_before);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Ready);
    assert_eq!(journal.view().tasks["a"].revision, trev);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
    assert!(adapter.requests().is_empty());
}

#[test]
fn codex_adapter_confines_reads_via_config_in_git_worktree() {
    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(git_executable())
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    let fixture = Fixture::new();
    let repo_root = fixture.root.join("git-config-confinement");
    fs::create_dir_all(repo_root.join("app")).unwrap();
    fs::create_dir_all(repo_root.join("secrets")).unwrap();
    fs::write(repo_root.join(".gitignore"), ".storyflow/\n").unwrap();
    fs::write(repo_root.join("app/seed.txt"), "seed\n").unwrap();
    fs::write(repo_root.join("secrets/hidden.txt"), "do not read\n").unwrap();

    run_git(&repo_root, &["init", "-b", "master"]);
    run_git(&repo_root, &["config", "user.name", "test"]);
    run_git(&repo_root, &["config", "user.email", "test@local"]);
    run_git(
        &repo_root,
        &["add", ".gitignore", "app/seed.txt", "secrets/hidden.txt"],
    );
    run_git(&repo_root, &["commit", "-m", "initial baseline"]);
    let base = run_git(&repo_root, &["rev-parse", "HEAD"]);
    fs::create_dir_all(repo_root.join(format!(".storyflow/{STORY}/events"))).unwrap();

    let context = RunContext {
        story: STORY.into(),
        base: base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        blocked: false,
        scope: vec!["app".into()],
        protected: Vec::new(),
    };
    let mut impl_spec = spec("a", &[]);
    impl_spec.stage = TaskStage::Implement;
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base,
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![impl_spec],
    };

    let mut journal = TaskJournal::open(&repo_root, context.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();
    let clock = FakeClock::at(100);
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 2), &clock)
        .unwrap();
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();

    let coordinator = IntegrationCoordinator::new(&repo_root, context).unwrap();
    let worktree = coordinator
        .prepare_worktree(journal.view(), Some(&graph), "a")
        .unwrap();
    let workdir = repo_root.join(&worktree.path);

    let help_with_config =
        "Usage: codex exec --cd <DIR> --sandbox <MODE> --config <KEY=VALUE> --json - [PROMPT]\n";
    let runner = Arc::new(
        MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_read_root_confinement(false),
    );
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, help_with_config, "", false, false);
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": dispatch.packet.task.revision,
        "status": "succeeded",
        "report": "wrote app/a.txt under --config read confinement",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "confined git worktree edit\n");
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
    let req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: workdir.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome = adapter.execute(&req, &cancel);
    let report = reconcile_outcome(&mut journal, &dispatch, outcome, &clock).unwrap();
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");

    let status = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(
        status.phase,
        IntegrationPhase::Ready,
        "{:?}",
        status.blockers
    );

    let calls = runner.calls();
    let exec_call = calls.last().unwrap();
    assert!(
        !exec_call.args.iter().any(|a| a == "--sandbox"),
        "legacy --sandbox flag must be omitted when activating default_permissions profile: {:?}",
        exec_call.args
    );
    let profile_kv = exec_call
        .args
        .windows(2)
        .find_map(|w| {
            if w[0] == "--config" && w[1].starts_with("default_permissions=\"storyflow_scoped_") {
                Some(w[1].clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            panic!(
                "missing invocation-unique default_permissions in {:?}",
                exec_call.args
            )
        });
    let profile_name = profile_kv
        .strip_prefix("default_permissions=\"")
        .and_then(|s| s.strip_suffix('"'))
        .unwrap();
    assert!(
        exec_call.args.windows(2).any(|w| {
            w[0] == "--config"
                && w[1] == format!("permissions.{profile_name}.network.enabled=false")
        }),
        "{:?}",
        exec_call.args
    );
    assert!(
        exec_call.args.windows(2).any(|w| {
            w[0] == "--config"
                && w[1] == format!("permissions.{profile_name}.filesystem.\":root\"=\"deny\"")
        }),
        "{:?}",
        exec_call.args
    );
    let git_dir_norm = fs::canonicalize(repo_root.join(".git"))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let git_dir_trimmed = git_dir_norm.strip_prefix(r"\\?\").unwrap_or(&git_dir_norm);
    let expected_git_deny = format!(
        "permissions.{profile_name}.filesystem.{}=\"deny\"",
        serde_json::to_string(git_dir_trimmed).unwrap()
    );
    assert!(
        exec_call
            .args
            .windows(2)
            .any(|w| w[0] == "--config" && w[1] == expected_git_deny),
        "expected {expected_git_deny} in {:?}",
        exec_call.args
    );
}

#[test]
fn codex_adapter_rejects_stale_git_worktree_base_before_promotion() {
    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(git_executable())
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    let fixture = Fixture::new();
    let repo_root = fixture.root.join("git-stale-base");
    fs::create_dir_all(repo_root.join("app")).unwrap();
    fs::write(repo_root.join(".gitignore"), ".storyflow/\n").unwrap();
    fs::write(repo_root.join("app/seed.txt"), "seed\n").unwrap();

    run_git(&repo_root, &["init", "-b", "master"]);
    run_git(&repo_root, &["config", "user.name", "test"]);
    run_git(&repo_root, &["config", "user.email", "test@local"]);
    run_git(&repo_root, &["add", ".gitignore", "app/seed.txt"]);
    run_git(&repo_root, &["commit", "-m", "commit 1"]);
    let stale_head = run_git(&repo_root, &["rev-parse", "HEAD"]);

    fs::write(repo_root.join("app/seed.txt"), "seed v2\n").unwrap();
    run_git(&repo_root, &["add", "app/seed.txt"]);
    run_git(&repo_root, &["commit", "-m", "commit 2 (expected base)"]);
    let expected_base = run_git(&repo_root, &["rev-parse", "HEAD"]);

    fs::create_dir_all(repo_root.join(format!(".storyflow/{STORY}/events"))).unwrap();
    let context = RunContext {
        story: STORY.into(),
        base: expected_base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        blocked: false,
        scope: vec!["app".into()],
        protected: Vec::new(),
    };
    let mut impl_spec = spec("a", &[]);
    impl_spec.stage = TaskStage::Implement;
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: expected_base,
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![impl_spec],
    };

    let mut journal = TaskJournal::open(&repo_root, context).unwrap();
    journal.initialize(graph).unwrap();
    let clock = FakeClock::at(100);
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 2), &clock)
        .unwrap();
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();

    // Provision a stale worktree pointing at commit 1 instead of expected_base (commit 2).
    let stale_wt = repo_root.join("worktree-stale");
    run_git(
        &repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            stale_wt.to_str().unwrap(),
            &stale_head,
        ],
    );

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, sandbox_help, "", false, false);
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": dispatch.packet.task.revision,
        "status": "succeeded",
        "report": "wrote on stale base",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "should not promote on stale base\n");
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
    let req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: stale_wt.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome = adapter.execute(&req, &cancel);
    let report = reconcile_outcome(&mut journal, &dispatch, outcome, &clock).unwrap();
    assert_eq!(report.status, ReconcileStatus::Blocked);
    assert_eq!(report.code, "invalid_output");
    assert!(
        report.detail.contains("does not match expected task base"),
        "{}",
        report.detail
    );
    assert!(!stale_wt.join("app/a.txt").exists());
    assert_eq!(run_git(&stale_wt, &["rev-parse", "HEAD"]), stale_head);
}

#[cfg(unix)]
#[test]
fn codex_adapter_does_not_collapse_unicode_nfc_and_nfd_scope_paths() {
    let fixture = Fixture::new();
    let workdir = fixture.root.join("worktrees/wt-unicode-norm");
    // Declared scope uses NFC precomposed `é`, whereas disk contains NFD `e\u{0301}`.
    fs::create_dir_all(workdir.join("cafe\u{0301}")).unwrap();
    fs::write(
        workdir.join("cafe\u{0301}/secret.txt"),
        "nfd secret sibling\n",
    )
    .unwrap();
    fs::create_dir_all(workdir.join("caf\u{00e9}")).unwrap();
    fs::write(workdir.join("caf\u{00e9}/ok.txt"), "nfc allowed\n").unwrap();

    let mut ctx = fixture.context.clone();
    ctx.scope = vec!["caf\u{00e9}".into()];
    let norm_spec = TaskSpec {
        id: "a".into(),
        story: STORY.into(),
        prerequisites: Vec::new(),
        stage: TaskStage::Plan,
        read_scope: vec!["caf\u{00e9}".into()],
        write_scope: vec!["caf\u{00e9}/out.txt".into()],
        acceptance_criteria: vec!["AC1 verified".into()],
        decision_owner: "orchestrator".into(),
    };
    let norm_graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![norm_spec],
    };

    let clock = FakeClock::at(100);
    let mut journal = TaskJournal::open(&fixture.root, ctx).unwrap();
    journal.initialize(norm_graph).unwrap();
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 5), &clock)
        .unwrap();

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";

    // Unconfined runner without `--config` must refuse execution fail-closed before launching codex.
    let unconfined = Arc::new(
        MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_read_root_confinement(false),
    );
    unconfined.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    unconfined.push_ok(0, sandbox_help, "", false, false);
    let cancel = CancelToken::new();
    let unconfined_adapter =
        CodexAdapter::with_runner(CodexAdapterConfig::default(), unconfined.clone());
    let refused = run_task(
        &mut journal,
        &unconfined_adapter,
        "a",
        "codex-host",
        &workdir,
        ExecutionPolicy::WorkspaceWrite,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(refused.status, ReconcileStatus::HostUnavailable);
    assert_eq!(refused.attempt_id, None);
    assert!(
        refused.detail.contains("--config") || refused.detail.contains("read-root confinement"),
        "{refused:?}"
    );
    assert!(unconfined.observed_exec_paths().is_empty());

    // Confined runner stages only the exact NFC spelling and blocks writing NFD sibling.
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "attempted NFD write",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("cafe\u{0301}/out.txt", "nfd leak\n");
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
    let blocked = run_task(
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
    assert_eq!(blocked.status, ReconcileStatus::Blocked);
    assert_eq!(blocked.code, "invalid_output");
    let seen = runner.observed_exec_paths();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains(&"caf\u{00e9}/ok.txt".to_owned()),
        "{:?}",
        seen[0]
    );
    assert!(
        !seen[0].iter().any(|p| p.contains("cafe\u{0301}")),
        "NFD sibling must not be staged: {:?}",
        seen[0]
    );
}

#[test]
fn codex_adapter_git_commit_ignores_configured_git_filters() {
    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(git_executable())
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    let fixture = Fixture::new();
    let repo_root = fixture.root.join("git-repo-filters");
    fs::create_dir_all(repo_root.join("app")).unwrap();
    fs::write(repo_root.join(".gitignore"), ".storyflow/\n").unwrap();
    fs::write(repo_root.join(".gitattributes"), "app/*.txt filter=evil\n").unwrap();
    fs::write(repo_root.join("app/seed.txt"), "seed\n").unwrap();

    run_git(&repo_root, &["init", "-b", "master"]);
    run_git(&repo_root, &["config", "user.name", "test"]);
    run_git(&repo_root, &["config", "user.email", "test@local"]);
    run_git(
        &repo_root,
        &["add", ".gitignore", ".gitattributes", "app/seed.txt"],
    );
    run_git(&repo_root, &["commit", "-m", "initial baseline"]);
    let base = run_git(&repo_root, &["rev-parse", "HEAD"]);
    fs::create_dir_all(repo_root.join(format!(".storyflow/{STORY}/events"))).unwrap();

    let context = RunContext {
        story: STORY.into(),
        base: base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        blocked: false,
        scope: vec!["app".into()],
        protected: Vec::new(),
    };
    let mut impl_spec = spec("a", &[]);
    impl_spec.stage = TaskStage::Implement;
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base,
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![impl_spec],
    };

    let mut journal = TaskJournal::open(&repo_root, context.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();
    let clock = FakeClock::at(100);
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 2), &clock)
        .unwrap();
    let dispatch = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();

    let coordinator = IntegrationCoordinator::new(&repo_root, context).unwrap();
    let worktree = coordinator
        .prepare_worktree(journal.view(), Some(&graph), "a")
        .unwrap();
    let workdir = repo_root.join(&worktree.path);

    let filter_sentinel = fixture.root.join("git-filter-executed.sentinel");
    let sentinel_posix = filter_sentinel.to_string_lossy().replace('\\', "/");
    let evil_cmd = format!("touch '{sentinel_posix}' && exit 1");
    run_git(&repo_root, &["config", "filter.evil.clean", &evil_cmd]);
    run_git(&repo_root, &["config", "filter.evil.process", &evil_cmd]);
    run_git(&repo_root, &["config", "filter.evil.smudge", &evil_cmd]);
    run_git(&repo_root, &["config", "filter.evil.required", "true"]);

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner.push_ok(0, sandbox_help, "", false, false);
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": dispatch.packet.task.revision,
        "status": "succeeded",
        "report": "wrote raw app/a.txt",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner.set_workspace_mutation("app/a.txt", "raw bytes without filter execution\n");
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
    let req = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: workdir.clone(),
        timeout_ms: 10_000,
        policy: ExecutionPolicy::WorkspaceWrite,
    };
    let outcome = adapter.execute(&req, &cancel);
    let pinned = match &outcome {
        AdapterOutcome::Completed { result, .. } => result
            .commit
            .clone()
            .expect("adapter must commit promoted file without invoking filters"),
        other => panic!("expected Completed outcome, got {other:?}"),
    };
    assert!(
        !filter_sentinel.exists(),
        "host-side git commit must never execute repository filter commands"
    );

    let committed_body = run_git(&workdir, &["show", &format!("{pinned}:app/a.txt")]);
    assert_eq!(committed_body, "raw bytes without filter execution");

    let report = reconcile_outcome(&mut journal, &dispatch, outcome, &clock).unwrap();
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");
    let status = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(
        status.phase,
        IntegrationPhase::Ready,
        "{:?}",
        status.blockers
    );
    assert!(status.candidate_commit.is_some());
    assert!(
        !filter_sentinel.exists(),
        "filter must remain unexecuted across integration check"
    );
}

#[test]
fn read_only_task_with_empty_write_scope_rejects_claimed_commit() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-ro-commit");
    fs::write(workdir.join("app/input.txt"), "read-only input\n").unwrap();

    let mut ro_spec = spec("a", &[]);
    ro_spec.write_scope.clear();
    let ro_graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![ro_spec],
    };

    let clock = FakeClock::at(100);
    let mut journal = TaskJournal::open(&fixture.root, fixture.context.clone()).unwrap();
    journal.initialize(ro_graph).unwrap();
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 3), &clock)
        .unwrap();

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    let forged_commit = "a".repeat(40);
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "read-only task tried claiming a commit",
        "commit": forged_commit,
        "artifacts": []
    })
    .to_string();
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
    let codex_report = run_task(
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
    assert_eq!(codex_report.status, ReconcileStatus::Blocked);
    assert_eq!(codex_report.code, "invalid_output");
    assert!(
        codex_report.detail.contains("write_scope") || codex_report.detail.contains("commit"),
        "{codex_report:?}"
    );
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);

    journal
        .retry_task(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "verify runner rejects empty write_scope commit",
            &clock,
        )
        .unwrap();
    let fake_adapter = FakeAdapter::available("fake").with_default(FakeBehavior::Succeed {
        report: "forged commit from generic adapter".into(),
        commit: Some("b".repeat(40)),
        artifacts: Vec::new(),
    });
    let runner_report = run_task(
        &mut journal,
        &fake_adapter,
        "a",
        "fake-host",
        &workdir,
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(runner_report.status, ReconcileStatus::Blocked);
    assert_eq!(runner_report.code, "invalid_worker_result");
    assert!(
        runner_report.detail.contains("write_scope") || runner_report.detail.contains("commit"),
        "{runner_report:?}"
    );
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);
}

#[test]
fn codex_probe_rejects_ambient_legacy_sandbox_before_reserving_attempt() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-ambient-sandbox");
    fs::write(workdir.join("app/input.txt"), "allowed input\n").unwrap();

    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);
    let rev_before = journal.view().revision.unwrap();

    let help_with_config =
        "Usage: codex exec --cd <DIR> --sandbox <MODE> --config <KEY=VALUE> --json - [PROMPT]\n";
    let runner = Arc::new(
        MockCodexRunner::new(Ok(PathBuf::from("codex")))
            .with_read_root_confinement(false)
            .with_ambient_legacy_sandbox(true),
    );
    // One probe pair for explicit `adapter.probe()`, one probe pair for `dispatch_and_run`.
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, help_with_config, "", false, false);
    }

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner.clone());
    let caps = adapter.probe();
    assert!(!caps.available);
    assert!(
        caps.unavailable_reason
            .as_deref()
            .is_some_and(|r| r.contains("sandbox_mode")),
        "{caps:?}"
    );

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
    assert_eq!(report.status, ReconcileStatus::HostUnavailable);
    assert_eq!(report.attempt_id, None);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Ready);
    assert_eq!(journal.view().revision.unwrap(), rev_before);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
    assert!(runner.observed_exec_paths().is_empty());
}

#[test]
fn non_completed_outcomes_reject_invalid_session_ids() {
    type OutcomeFactory = fn(SessionCorrelation, bool) -> AdapterOutcome;
    let cases: [(&str, OutcomeFactory); 4] = [
        ("cancelled", |correlation, worker_stopped| {
            AdapterOutcome::Cancelled {
                correlation,
                worker_stopped,
                reason: "worker cancelled".into(),
            }
        }),
        ("timed_out", |correlation, worker_stopped| {
            AdapterOutcome::TimedOut {
                correlation,
                worker_stopped,
                reason: "worker timed out".into(),
            }
        }),
        ("host_crashed", |correlation, worker_stopped| {
            AdapterOutcome::HostCrashed {
                correlation,
                worker_stopped,
                exit_code: Some(1),
                detail: "process died".into(),
            }
        }),
        ("invalid_output", |correlation, worker_stopped| {
            AdapterOutcome::InvalidOutput {
                correlation,
                worker_stopped,
                detail: "bad json".into(),
            }
        }),
    ];

    let invalid_ids = [String::new(), "bad\nsession".to_owned(), "x".repeat(300)];

    for (label, build_outcome) in cases {
        for bad_session in &invalid_ids {
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

            let mut bad_corr = SessionCorrelation::from_lease(&dispatch.lease, "valid-placeholder");
            bad_corr.session_id = bad_session.clone();

            let unstopped_report = reconcile_outcome(
                &mut journal,
                &dispatch,
                build_outcome(bad_corr.clone(), false),
                &clock,
            )
            .unwrap();
            assert_eq!(
                unstopped_report.status,
                ReconcileStatus::BlockedUnstopped,
                "case {label} should stay unstopped"
            );
            assert_eq!(unstopped_report.session_id, None);
            assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 1);

            let stopped_report = reconcile_outcome(
                &mut journal,
                &dispatch,
                build_outcome(bad_corr, true),
                &clock,
            )
            .unwrap();
            assert_eq!(
                stopped_report.status,
                ReconcileStatus::Blocked,
                "case {label} should block on invalid session_id"
            );
            assert_eq!(stopped_report.code, "correlation_mismatch");
            assert_eq!(stopped_report.session_id, None);
            assert!(stopped_report.worker_stopped);
            assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);
            assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
        }
    }
}

#[test]
fn codex_adapter_treats_pre_launch_cancellation_as_stopped_without_containment() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-prelaunch-cancel");
    fs::write(workdir.join("app/input.txt"), "allowed input\n").unwrap();

    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))).with_containment(false));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, sandbox_help, "", false, false);
    }
    runner.push_pre_launch_cancelled();

    let adapter = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner);
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
    assert_eq!(report.status, ReconcileStatus::Blocked, "{report:?}");
    assert_eq!(report.code, "worker_cancelled");
    assert!(report.worker_stopped);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
}

#[test]
fn codex_adapter_commits_file_directory_replacements_in_git_worktree() {
    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(git_executable())
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    let fixture = Fixture::new();
    let repo_root = fixture.root.join("git-df-repo");
    fs::create_dir_all(repo_root.join("app")).unwrap();
    fs::write(repo_root.join(".gitignore"), ".storyflow/\n").unwrap();
    fs::write(repo_root.join("app/node"), "file entry\n").unwrap();

    run_git(&repo_root, &["init", "-b", "master"]);
    run_git(&repo_root, &["config", "user.name", "test"]);
    run_git(&repo_root, &["config", "user.email", "test@local"]);
    run_git(&repo_root, &["add", ".gitignore", "app/node"]);
    run_git(&repo_root, &["commit", "-m", "initial baseline"]);
    let base = run_git(&repo_root, &["rev-parse", "HEAD"]);
    fs::create_dir_all(repo_root.join(format!(".storyflow/{STORY}/events"))).unwrap();

    let context = RunContext {
        story: STORY.into(),
        base: base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        blocked: false,
        scope: vec!["app".into()],
        protected: Vec::new(),
    };
    let mut impl_a = spec("a", &[]);
    impl_a.stage = TaskStage::Implement;
    impl_a.write_scope = vec!["app".into()];
    let mut impl_b = spec("b", &["a"]);
    impl_b.stage = TaskStage::Implement;
    impl_b.write_scope = vec!["app".into()];
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base,
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![impl_a, impl_b],
    };

    let mut journal = TaskJournal::open(&repo_root, context.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();
    let clock = FakeClock::at(100);
    journal
        .configure_scheduler(journal.view().revision.unwrap(), policy(1, 3), &clock)
        .unwrap();

    let coordinator = IntegrationCoordinator::new(&repo_root, context).unwrap();
    let sandbox_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json - [PROMPT]\n";

    // Case 1: task `a` replaces tracked file `app/node` with directory `app/node/child.txt`.
    let dispatch1 = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "a",
            task_rev(&journal, "a"),
            "codex-host",
            &clock,
        )
        .unwrap();
    let worktree1 = coordinator
        .prepare_worktree(journal.view(), Some(&graph), "a")
        .unwrap();
    let workdir1 = repo_root.join(&worktree1.path);

    let runner1 = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner1.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner1.push_ok(0, sandbox_help, "", false, false);
    let result_json1 = json!({
        "story": STORY,
        "task": "a",
        "task_revision": dispatch1.packet.task.revision,
        "status": "succeeded",
        "report": "replaced file app/node with directory app/node/child.txt",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner1.set_remove_staged_paths(&["app/node"]);
    runner1.set_workspace_mutation("app/node/child.txt", "nested child\n");
    runner1.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&result_json1).unwrap()
        ),
        "",
        false,
        false,
    );

    let adapter1 = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner1);
    let cancel = CancelToken::new();
    let outcome1 = adapter1.execute(
        &AdapterRequest {
            schema: ADAPTER_SCHEMA,
            dispatch: dispatch1.clone(),
            workdir: workdir1.clone(),
            timeout_ms: 10_000,
            policy: ExecutionPolicy::WorkspaceWrite,
        },
        &cancel,
    );
    let commit1 = match &outcome1 {
        AdapterOutcome::Completed { result, .. } => result
            .commit
            .clone()
            .expect("D->F replacement must produce a commit"),
        other => panic!("expected Completed outcome for file->dir replacement, got {other:?}"),
    };
    let tree1 = run_git(&workdir1, &["ls-tree", "-r", "--name-only", &commit1]);
    let lines1: Vec<&str> = tree1.lines().collect();
    assert!(lines1.contains(&"app/node/child.txt"), "{lines1:?}");
    assert!(!lines1.contains(&"app/node"), "{lines1:?}");

    // Case 2: dependent task `b` performs reverse transition (`app/node/child.txt` -> file `app/node`).
    let report1 = reconcile_outcome(&mut journal, &dispatch1, outcome1, &clock).unwrap();
    assert_eq!(report1.status, ReconcileStatus::Applied);
    let dispatch2 = journal
        .dispatch(
            journal.view().revision.unwrap(),
            "b",
            task_rev(&journal, "b"),
            "codex-host",
            &clock,
        )
        .unwrap();
    let worktree2 = coordinator
        .prepare_worktree(journal.view(), Some(&graph), "b")
        .unwrap();
    let workdir2 = repo_root.join(&worktree2.path);

    let runner2 = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    runner2.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
    runner2.push_ok(0, sandbox_help, "", false, false);
    let result_json2 = json!({
        "story": STORY,
        "task": "b",
        "task_revision": dispatch2.packet.task.revision,
        "status": "succeeded",
        "report": "replaced dir app/node with file app/node",
        "commit": null,
        "artifacts": []
    })
    .to_string();
    runner2.set_remove_staged_paths(&["app/node"]);
    runner2.set_workspace_mutation("app/node", "back to file\n");
    runner2.push_ok(
        0,
        &format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
            serde_json::to_string(&result_json2).unwrap()
        ),
        "",
        false,
        false,
    );

    let adapter2 = CodexAdapter::with_runner(CodexAdapterConfig::default(), runner2);
    let outcome2 = adapter2.execute(
        &AdapterRequest {
            schema: ADAPTER_SCHEMA,
            dispatch: dispatch2.clone(),
            workdir: workdir2.clone(),
            timeout_ms: 10_000,
            policy: ExecutionPolicy::WorkspaceWrite,
        },
        &cancel,
    );
    let commit2 = match &outcome2 {
        AdapterOutcome::Completed { result, .. } => result
            .commit
            .clone()
            .expect("F->D replacement must produce a commit"),
        other => panic!("expected Completed outcome for dir->file replacement, got {other:?}"),
    };
    let tree2 = run_git(&workdir2, &["ls-tree", "-r", "--name-only", &commit2]);
    let lines2: Vec<&str> = tree2.lines().collect();
    assert!(lines2.contains(&"app/node"), "{lines2:?}");
    assert!(!lines2.contains(&"app/node/child.txt"), "{lines2:?}");

    let report2 = reconcile_outcome(&mut journal, &dispatch2, outcome2, &clock).unwrap();
    assert_eq!(report2.status, ReconcileStatus::Applied);
    let status = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(
        status.phase,
        IntegrationPhase::Ready,
        "{:?}",
        status.blockers
    );
    assert!(status.candidate_commit.is_some());
}

#[test]
fn codex_adapter_accepts_large_valid_last_message_with_64_artifacts() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("wt-large-last-msg");
    fs::write(workdir.join("app/input.txt"), "allowed input\n").unwrap();

    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(1, 2), &clock);

    let full_help = "Usage: codex exec --cd <DIR> --sandbox <MODE> --json --output-schema <FILE> --output-last-message <FILE> - [PROMPT]\n";
    let runner = Arc::new(MockCodexRunner::new(Ok(PathBuf::from("codex"))));
    for _ in 0..2 {
        runner.push_ok(0, "codex-cli 0.55.0\n", "", false, false);
        runner.push_ok(0, full_help, "", false, false);
    }

    let artifacts: Vec<serde_json::Value> = (0..64)
        .map(|i| {
            json!({
                "path": format!("app/art-{i:02}-{}.txt", "x".repeat(990)),
                "sha256": "a".repeat(64),
            })
        })
        .collect();
    let result_json = json!({
        "story": STORY,
        "task": "a",
        "task_revision": task_rev(&journal, "a") + 1,
        "status": "succeeded",
        "report": "completed with 64 artifacts",
        "commit": null,
        "artifacts": artifacts,
    })
    .to_string();
    assert!(
        result_json.len() > 65_536,
        "expected > 64 KiB JSON payload, got {} bytes",
        result_json.len()
    );
    runner.set_last_message(&result_json);
    runner.push_ok(
        0,
        "{\"type\":\"thread.started\",\"thread_id\":\"th_large_msg\"}\n",
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
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");
    let rec = &journal.view().tasks["a"];
    assert_eq!(rec.status, TaskStatus::Succeeded);
    assert_eq!(
        rec.result.as_ref().unwrap().artifacts.len(),
        64,
        "all 64 artifacts must survive last-message read"
    );
}

#[test]
#[ignore]
fn codex_live_smoke() {
    let fixture = Fixture::new();
    let workdir = fixture.isolated_workdir("smoke");
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
        &workdir,
        Duration::from_secs(60),
        ExecutionPolicy::ReadOnly,
        &cancel,
        &clock,
    )
    .unwrap();
    assert_eq!(report.status, ReconcileStatus::Applied, "{report:?}");
    assert!(report.worker_stopped);
}
