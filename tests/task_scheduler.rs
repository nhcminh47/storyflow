use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
};
use storyflow::tasks::*;

const STORY: &str = "scheduler-001";
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
            .join("target/task-scheduler-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(root.join(format!(".storyflow/{STORY}/events"))).unwrap();
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
    fn open(&self) -> TaskJournal {
        self.open_at(self.context.clone())
    }
    fn open_at(&self, context: RunContext) -> TaskJournal {
        TaskJournal::open(&self.root, context).unwrap()
    }
    fn events(&self) -> PathBuf {
        self.root.join(format!(".storyflow/{STORY}/tasks/events"))
    }
    fn configured(&self, policy: SchedulerPolicy, clock: &FakeClock) -> TaskJournal {
        let mut journal = self.open();
        journal.initialize(graph()).unwrap();
        journal
            .configure_scheduler(rev(&journal), policy, clock)
            .unwrap();
        journal
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let allowed = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/task-scheduler-tests");
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
        acceptance_criteria: vec!["AC1 verified by fake worker".into()],
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
        lease_ms: 100,
    }
}
fn rev(journal: &TaskJournal) -> u64 {
    journal.view().revision.unwrap()
}
fn task_rev(journal: &TaskJournal, task: &str) -> u64 {
    journal.view().tasks[task].revision
}
fn rejected_unchanged<T>(
    journal: &mut TaskJournal,
    operation: impl FnOnce(&mut TaskJournal) -> TaskResult<T>,
) {
    let before = journal.view().clone();
    assert!(
        operation(journal).is_err(),
        "invalid operation was accepted"
    );
    assert_eq!(journal.view(), &before, "rejected operation changed state");
}

/// A host fixture: assignment and completion are explicit, with no model/process launch.
struct FakeWorker(DispatchPacket);
impl FakeWorker {
    fn launch(journal: &mut TaskJournal, task: &str, clock: &FakeClock) -> Self {
        let packet = journal
            .dispatch(
                rev(journal),
                task,
                task_rev(journal, task),
                "fake-host",
                clock,
            )
            .unwrap();
        Self(packet)
    }
    fn result(&self, status: ResultStatus) -> AttemptResult {
        AttemptResult {
            schema: 1,
            lease: self.0.lease.clone(),
            result: WorkerResult {
                schema: 1,
                story: self.0.packet.story.clone(),
                task: self.0.packet.task.spec.id.clone(),
                task_revision: self.0.packet.task.revision,
                status,
                report: "AC1 fake worker completed and exited".into(),
                commit: None,
                artifacts: Vec::new(),
            },
            worker_stopped: true,
        }
    }
    fn finish(&self, journal: &mut TaskJournal, clock: &FakeClock, status: ResultStatus) {
        journal
            .complete_attempt(rev(journal), self.result(status), clock)
            .unwrap();
    }
}

#[test]
fn two_fake_workers_unlock_dependency_only_after_both_successes_across_restart() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(1_000);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    assert_eq!(
        journal.scheduler_status(&clock).unwrap().dispatchable,
        ["a", "b"]
    );
    rejected_unchanged(&mut journal, |j| {
        j.dispatch(rev(j), "c", task_rev(j, "c"), "host", &clock)
    });
    let a = FakeWorker::launch(&mut journal, "a", &clock);
    let b = FakeWorker::launch(&mut journal, "b", &clock);
    assert_ne!(a.0.lease.attempt_id, b.0.lease.attempt_id);
    assert_ne!(a.0.lease.lease_id, b.0.lease.lease_id);
    assert!(
        journal
            .scheduler_status(&clock)
            .unwrap()
            .dispatchable
            .is_empty()
    );
    b.finish(&mut journal, &clock, ResultStatus::Succeeded);
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Pending);
    let saved = journal.view().clone();
    drop(journal);
    let mut journal = fixture.open();
    assert_eq!(journal.view(), &saved);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 1);
    a.finish(&mut journal, &clock, ResultStatus::Succeeded);
    assert_eq!(
        journal.scheduler_status(&clock).unwrap().dispatchable,
        ["c"]
    );
    let c = FakeWorker::launch(&mut journal, "c", &clock);
    assert_eq!(c.0.packet.prerequisites.len(), 2);
    assert_eq!(
        c.0.packet.prerequisites["a"],
        a.result(ResultStatus::Succeeded).result
    );
    c.finish(&mut journal, &clock, ResultStatus::Succeeded);
    let saved = journal.view().clone();
    drop(journal);
    let mut journal = fixture.open();
    assert_eq!(journal.view(), &saved);
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), a.result(ResultStatus::Succeeded), &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), c.result(ResultStatus::Succeeded), &clock)
    });
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
}

#[test]
fn concurrency_capacity_is_enforced_and_failure_does_not_unlock_a_dependency() {
    for outcome in [ResultStatus::Failed, ResultStatus::Blocked] {
        let fixture = Fixture::new();
        let clock = FakeClock::at(0);
        let mut journal = fixture.configured(policy(1, 3), &clock);
        let a = FakeWorker::launch(&mut journal, "a", &clock);
        let status = journal.scheduler_status(&clock).unwrap();
        assert_eq!(status.active_attempts, 1);
        assert!(!status.blockers["b"].is_empty());
        rejected_unchanged(&mut journal, |j| {
            j.dispatch(rev(j), "b", task_rev(j, "b"), "host", &clock)
        });
        a.finish(&mut journal, &clock, outcome);
        let b = FakeWorker::launch(&mut journal, "b", &clock);
        b.finish(&mut journal, &clock, ResultStatus::Succeeded);
        assert!(
            journal
                .scheduler_status(&clock)
                .unwrap()
                .dispatchable
                .is_empty()
        );
        assert_eq!(journal.view().tasks["c"].status, TaskStatus::Pending);
        journal
            .retry_task(
                rev(&journal),
                "a",
                task_rev(&journal, "a"),
                "fixed failure",
                &clock,
            )
            .unwrap();
        FakeWorker::launch(&mut journal, "a", &clock).finish(
            &mut journal,
            &clock,
            ResultStatus::Succeeded,
        );
        assert_eq!(
            journal.scheduler_status(&clock).unwrap().dispatchable,
            ["c"]
        );
    }
}

#[test]
fn overlap_is_bidirectional_case_unicode_normalized_and_respects_slash_boundaries() {
    for (left, right) in [
        ("app/tree", "app/tree/child"),
        ("app/tree/child", "app/tree"),
        ("app/Tree", "app/tree/child"),
        ("app/Caf\u{e9}", "app/Cafe\u{301}/child"),
    ] {
        let fixture = Fixture::new();
        let clock = FakeClock::at(0);
        let mut g = graph();
        g.tasks[0].write_scope = vec![left.into()];
        g.tasks[1].write_scope = vec![right.into()];
        g.tasks[2].prerequisites.clear();
        g.tasks[2].write_scope = vec!["app/tree-sibling".into()];
        let mut journal = fixture.open();
        journal.initialize(g).unwrap();
        journal
            .configure_scheduler(rev(&journal), policy(3, 3), &clock)
            .unwrap();
        let a = FakeWorker::launch(&mut journal, "a", &clock);
        assert!(!journal.scheduler_status(&clock).unwrap().blockers["b"].is_empty());
        rejected_unchanged(&mut journal, |j| {
            j.dispatch(rev(j), "b", task_rev(j, "b"), "host", &clock)
        });
        let c = FakeWorker::launch(&mut journal, "c", &clock);
        c.finish(&mut journal, &clock, ResultStatus::Succeeded);
        a.finish(&mut journal, &clock, ResultStatus::Succeeded);
        FakeWorker::launch(&mut journal, "b", &clock);
    }
}

#[test]
fn callbacks_require_exact_lease_owner_attempt_result_identity_and_stop_assertion() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(10);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let worker = FakeWorker::launch(&mut journal, "a", &clock);
    for key in ["owner", "lease_id", "attempt_id"] {
        let mut token = serde_json::to_value(&worker.0.lease).unwrap();
        token[key] = json!("different-host-or-token");
        let token: LeaseToken = serde_json::from_value(token).unwrap();
        rejected_unchanged(&mut journal, |j| j.heartbeat(rev(j), &token, &clock));
        rejected_unchanged(&mut journal, |j| {
            j.confirm_stopped(rev(j), &token, "host stopped", &clock)
        });
        let mut result = worker.result(ResultStatus::Succeeded);
        result.lease = token;
        rejected_unchanged(&mut journal, |j| j.complete_attempt(rev(j), result, &clock));
    }
    for change in [
        "story",
        "task",
        "task_revision",
        "schema",
        "report",
        "commit",
        "artifact",
        "stopped",
    ] {
        let mut result = worker.result(ResultStatus::Succeeded);
        match change {
            "story" => result.result.story = "other-001".into(),
            "task" => result.result.task = "b".into(),
            "task_revision" => result.result.task_revision += 1,
            "schema" => result.schema = 2,
            "report" => result.result.report.clear(),
            "commit" => result.result.commit = Some("invalid-commit".into()),
            "artifact" => result.result.artifacts.push(Artifact {
                path: "../escape".into(),
                sha256: "0".repeat(64),
            }),
            "stopped" => result.worker_stopped = false,
            _ => unreachable!(),
        }
        rejected_unchanged(&mut journal, |j| j.complete_attempt(rev(j), result, &clock));
    }
    let stale = rev(&journal) - 1;
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(stale, worker.result(ResultStatus::Succeeded), &clock)
    });
    worker.finish(&mut journal, &clock, ResultStatus::Succeeded);
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), worker.result(ResultStatus::Succeeded), &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.heartbeat(rev(j), &worker.0.lease, &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.cancel_task(rev(j), "a", task_rev(j, "a"), "late cancel", &clock)
    });
}

#[test]
fn heartbeat_renews_lease_without_invalidating_the_workers_task_revision() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let worker = FakeWorker::launch(&mut journal, "a", &clock);
    assert_eq!(worker.0.expires_at_ms, 200);
    let original = worker.0.packet.task.revision;
    clock.set(199);
    journal
        .heartbeat(rev(&journal), &worker.0.lease, &clock)
        .unwrap();
    assert_eq!(task_rev(&journal, "a"), original);
    let status = journal.scheduler_status(&clock).unwrap();
    let attempt = &status.attempts[&worker.0.lease.attempt_id];
    assert_eq!(attempt.expires_at_ms, 299);
    assert_eq!(attempt.heartbeat_at_ms, 199);
    assert_eq!(attempt.lease, worker.0.lease);
    let saved = journal.view().clone();
    drop(journal);
    let mut journal = fixture.open();
    assert_eq!(journal.view(), &saved);
    clock.set(200);
    worker.finish(&mut journal, &clock, ResultStatus::Succeeded);
}

#[test]
fn expiry_fences_callbacks_before_recovery_and_retry_waits_for_confirmed_stop() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(1_000);
    let mut journal = fixture.configured(policy(1, 2), &clock);
    let old = FakeWorker::launch(&mut journal, "a", &clock);
    clock.set(old.0.expires_at_ms);
    rejected_unchanged(&mut journal, |j| j.heartbeat(rev(j), &old.0.lease, &clock));
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), old.result(ResultStatus::Succeeded), &clock)
    });
    journal.recover_expired(rev(&journal), &clock).unwrap();
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);
    let status = journal.scheduler_status(&clock).unwrap();
    assert_eq!(
        status.attempts[&old.0.lease.attempt_id].phase,
        AttemptPhase::Expired
    );
    assert_eq!(status.active_attempts, 1);
    assert!(!status.attempts[&old.0.lease.attempt_id].worker_stopped);
    rejected_unchanged(&mut journal, |j| {
        j.retry_task(rev(j), "a", task_rev(j, "a"), "retry", &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.dispatch(rev(j), "b", task_rev(j, "b"), "host", &clock)
    });
    let saved = journal.view().clone();
    drop(journal);
    let mut journal = fixture.open();
    assert_eq!(journal.view(), &saved);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 1);
    journal
        .confirm_stopped(
            rev(&journal),
            &old.0.lease,
            "host observed worker exit",
            &clock,
        )
        .unwrap();
    rejected_unchanged(&mut journal, |j| {
        j.confirm_stopped(rev(j), &old.0.lease, "duplicate", &clock)
    });
    journal
        .retry_task(
            rev(&journal),
            "a",
            task_rev(&journal, "a"),
            "safe retry",
            &clock,
        )
        .unwrap();
    let fresh = FakeWorker::launch(&mut journal, "a", &clock);
    assert_ne!(fresh.0.lease.attempt_id, old.0.lease.attempt_id);
    assert_ne!(fresh.0.lease.lease_id, old.0.lease.lease_id);
    assert!(fresh.0.packet.task.revision > old.0.packet.task.revision);
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), old.result(ResultStatus::Succeeded), &clock)
    });
    rejected_unchanged(&mut journal, |j| j.heartbeat(rev(j), &old.0.lease, &clock));
    fresh.finish(&mut journal, &clock, ResultStatus::Succeeded);
}

#[test]
fn retry_budget_counts_dispatch_attempts_and_exhaustion_is_durable() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let mut journal = fixture.configured(policy(2, 2), &clock);
    FakeWorker::launch(&mut journal, "a", &clock).finish(
        &mut journal,
        &clock,
        ResultStatus::Failed,
    );
    journal
        .retry_task(
            rev(&journal),
            "a",
            task_rev(&journal, "a"),
            "first retry",
            &clock,
        )
        .unwrap();
    FakeWorker::launch(&mut journal, "a", &clock).finish(
        &mut journal,
        &clock,
        ResultStatus::Blocked,
    );
    rejected_unchanged(&mut journal, |j| {
        j.retry_task(rev(j), "a", task_rev(j, "a"), "budget exhausted", &clock)
    });
    let saved = journal.view().clone();
    drop(journal);
    let mut journal = fixture.open();
    assert_eq!(journal.view(), &saved);
    rejected_unchanged(&mut journal, |j| {
        j.retry_task(
            rev(j),
            "a",
            task_rev(j, "a"),
            "restart cannot reset budget",
            &clock,
        )
    });
    assert_eq!(journal.scheduler_status(&clock).unwrap().attempts.len(), 2);
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Pending);
}

#[test]
fn cancellation_is_terminal_and_does_not_assume_the_worker_has_stopped() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let mut journal = fixture.configured(policy(1, 3), &clock);
    let worker = FakeWorker::launch(&mut journal, "a", &clock);
    journal
        .cancel_task(
            rev(&journal),
            "a",
            task_rev(&journal, "a"),
            "requested cancellation",
            &clock,
        )
        .unwrap();
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Cancelled);
    let status = journal.scheduler_status(&clock).unwrap();
    assert_eq!(status.active_attempts, 1);
    assert_eq!(
        status.attempts[&worker.0.lease.attempt_id].phase,
        AttemptPhase::Cancelled
    );
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), worker.result(ResultStatus::Succeeded), &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.heartbeat(rev(j), &worker.0.lease, &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.dispatch(rev(j), "b", task_rev(j, "b"), "host", &clock)
    });
    journal
        .confirm_stopped(
            rev(&journal),
            &worker.0.lease,
            "host observed cancellation exit",
            &clock,
        )
        .unwrap();
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Cancelled);
    rejected_unchanged(&mut journal, |j| {
        j.retry_task(
            rev(j),
            "a",
            task_rev(j, "a"),
            "cannot undo terminal cancellation",
            &clock,
        )
    });
    FakeWorker::launch(&mut journal, "b", &clock).finish(
        &mut journal,
        &clock,
        ResultStatus::Succeeded,
    );
    assert_eq!(journal.view().tasks["c"].status, TaskStatus::Pending);
}

#[test]
fn clock_regression_and_expiry_overflow_cannot_publish_events() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let worker = FakeWorker::launch(&mut journal, "a", &clock);
    clock.set(99);
    assert!(journal.scheduler_status(&clock).is_err());
    rejected_unchanged(&mut journal, |j| {
        j.heartbeat(rev(j), &worker.0.lease, &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), worker.result(ResultStatus::Succeeded), &clock)
    });
    rejected_unchanged(&mut journal, |j| j.recover_expired(rev(j), &clock));
    clock.set(100);
    worker.finish(&mut journal, &clock, ResultStatus::Succeeded);
    let saved = journal.view().clone();
    drop(journal);
    // Replay cannot consult the supplied clock: reopening takes no clock at all.
    clock.set(0);
    let journal = fixture.open();
    assert_eq!(journal.view(), &saved);

    let overflow = Fixture::new();
    let clock = FakeClock::at(u64::MAX - 10);
    let mut journal = overflow.configured(policy(1, 1), &clock);
    rejected_unchanged(&mut journal, |j| {
        j.dispatch(rev(j), "a", task_rev(j, "a"), "host", &clock)
    });
}

#[test]
fn graph_replacement_keeps_old_worker_occupancy_and_rejects_old_callbacks() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(100);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let old = FakeWorker::launch(&mut journal, "a", &clock);
    let old_generation = journal.scheduler_status(&clock).unwrap().graph_revision;
    drop(journal);
    let mut context = fixture.context.clone();
    context.story_revision = 1;
    context.stage = TaskStage::Implement;
    let mut journal = fixture.open_at(context.clone());
    assert!(!journal.scheduler_status(&clock).unwrap().current);
    let mut replacement = graph();
    replacement.story_revision = 1;
    replacement.stage = TaskStage::Implement;
    for task in &mut replacement.tasks {
        task.stage = TaskStage::Implement;
    }
    journal.replace(rev(&journal), replacement).unwrap();
    let status = journal.scheduler_status(&clock).unwrap();
    assert!(status.current);
    assert!(status.graph_revision > old_generation);
    assert_eq!(status.active_attempts, 1);
    assert_eq!(
        status.attempts[&old.0.lease.attempt_id].phase,
        AttemptPhase::Retired
    );
    assert!(!status.blockers["a"].is_empty());
    rejected_unchanged(&mut journal, |j| {
        j.dispatch(rev(j), "a", task_rev(j, "a"), "new-host", &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), old.result(ResultStatus::Succeeded), &clock)
    });
    let b = FakeWorker::launch(&mut journal, "b", &clock);
    journal
        .confirm_stopped(rev(&journal), &old.0.lease, "retired worker exited", &clock)
        .unwrap();
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Ready);
    let fresh = FakeWorker::launch(&mut journal, "a", &clock);
    assert!(fresh.0.packet.task.revision > old.0.packet.task.revision);
    assert_ne!(fresh.0.lease.attempt_id, old.0.lease.attempt_id);
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), old.result(ResultStatus::Succeeded), &clock)
    });
    b.finish(&mut journal, &clock, ResultStatus::Succeeded);
    fresh.finish(&mut journal, &clock, ResultStatus::Succeeded);
    assert_eq!(
        journal.scheduler_status(&clock).unwrap().dispatchable,
        ["c"]
    );
    let saved = journal.view().clone();
    drop(journal);
    assert_eq!(fixture.open_at(context).view(), &saved);
}

#[test]
fn old_read_only_task_id_still_blocks_redispatch_until_stopped() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let mut g = graph();
    g.tasks[0].write_scope.clear();
    let mut journal = fixture.open();
    journal.initialize(g.clone()).unwrap();
    journal
        .configure_scheduler(rev(&journal), policy(3, 3), &clock)
        .unwrap();
    let old = FakeWorker::launch(&mut journal, "a", &clock);
    drop(journal);
    let mut context = fixture.context.clone();
    context.story_revision = 1;
    let mut journal = fixture.open_at(context);
    g.story_revision = 1;
    journal.replace(rev(&journal), g).unwrap();
    rejected_unchanged(&mut journal, |j| {
        j.dispatch(rev(j), "a", task_rev(j, "a"), "host", &clock)
    });
    journal
        .confirm_stopped(
            rev(&journal),
            &old.0.lease,
            "read-only worker exited",
            &clock,
        )
        .unwrap();
    FakeWorker::launch(&mut journal, "a", &clock);
}

#[test]
fn blocked_stale_and_done_stories_allow_cleanup_but_never_old_results_or_dispatch() {
    for changed in ["blocked", "advanced", "done"] {
        let fixture = Fixture::new();
        let clock = FakeClock::at(0);
        let mut journal = fixture.configured(policy(2, 3), &clock);
        let old = FakeWorker::launch(&mut journal, "a", &clock);
        drop(journal);
        let mut context = fixture.context.clone();
        context.story_revision = 1;
        match changed {
            "blocked" => context.blocked = true,
            "advanced" => context.stage = TaskStage::Implement,
            "done" => context.stage = TaskStage::Done,
            _ => unreachable!(),
        }
        let mut journal = fixture.open_at(context.clone());
        let status = journal.scheduler_status(&clock).unwrap();
        assert!(!status.current);
        assert!(status.dispatchable.is_empty());
        rejected_unchanged(&mut journal, |j| {
            j.dispatch(rev(j), "b", task_rev(j, "b"), "host", &clock)
        });
        rejected_unchanged(&mut journal, |j| j.heartbeat(rev(j), &old.0.lease, &clock));
        rejected_unchanged(&mut journal, |j| {
            j.complete_attempt(rev(j), old.result(ResultStatus::Succeeded), &clock)
        });
        journal
            .confirm_stopped(
                rev(&journal),
                &old.0.lease,
                "host cleanup after transition",
                &clock,
            )
            .unwrap();
        assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
        assert!(journal.view().tasks["a"].result.is_none());
        let saved = journal.view().clone();
        drop(journal);
        assert_eq!(fixture.open_at(context).view(), &saved);
    }
}

#[test]
fn interrupted_pending_publication_preserves_completed_and_inflight_workers() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    let a = FakeWorker::launch(&mut journal, "a", &clock);
    let b = FakeWorker::launch(&mut journal, "b", &clock);
    a.finish(&mut journal, &clock, ResultStatus::Succeeded);
    let saved = journal.view().clone();
    drop(journal);
    fs::write(
        fixture.events().join("pending.tmp"),
        b"{\"body\": interrupted",
    )
    .unwrap();
    let mut journal = fixture.open();
    assert_eq!(journal.view(), &saved);
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 1);
    rejected_unchanged(&mut journal, |j| {
        j.complete_attempt(rev(j), a.result(ResultStatus::Succeeded), &clock)
    });
    b.finish(&mut journal, &clock, ResultStatus::Succeeded);
    assert_eq!(
        journal.scheduler_status(&clock).unwrap().dispatchable,
        ["c"]
    );
    let saved = journal.view().clone();
    drop(journal);
    assert_eq!(fixture.open().view(), &saved);
}

#[test]
fn existing_schema_one_journal_bytes_survive_opt_in_and_legacy_paths_cannot_bypass_leases() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let mut journal = fixture.open();
    journal.initialize(graph()).unwrap();
    assert!(
        serde_json::to_value(journal.view())
            .unwrap()
            .get("scheduler")
            .is_none()
    );
    let packet = journal
        .start(rev(&journal), "a", task_rev(&journal, "a"))
        .unwrap();
    rejected_unchanged(&mut journal, |j| {
        j.configure_scheduler(rev(j), policy(2, 3), &clock)
    });
    let legacy_result = WorkerResult {
        schema: 1,
        story: STORY.into(),
        task: "a".into(),
        task_revision: packet.task.revision,
        status: ResultStatus::Succeeded,
        report: "legacy result".into(),
        commit: None,
        artifacts: Vec::new(),
    };
    journal
        .complete(rev(&journal), legacy_result.clone())
        .unwrap();
    let history: Vec<_> = (0..=rev(&journal))
        .map(|r| fs::read(fixture.events().join(format!("{r:012}.json"))).unwrap())
        .collect();
    drop(journal);
    let mut journal = fixture.open();
    journal
        .configure_scheduler(rev(&journal), policy(2, 3), &clock)
        .unwrap();
    rejected_unchanged(&mut journal, |j| {
        j.configure_scheduler(rev(j), policy(1, 1), &clock)
    });
    rejected_unchanged(&mut journal, |j| j.start(rev(j), "b", task_rev(j, "b")));
    rejected_unchanged(&mut journal, |j| j.complete(rev(j), legacy_result));
    FakeWorker::launch(&mut journal, "b", &clock).finish(
        &mut journal,
        &clock,
        ResultStatus::Succeeded,
    );
    assert_eq!(
        journal.scheduler_status(&clock).unwrap().dispatchable,
        ["c"]
    );
    for (revision, bytes) in history.iter().enumerate() {
        assert_eq!(
            &fs::read(fixture.events().join(format!("{revision:012}.json"))).unwrap(),
            bytes
        );
    }
}

#[test]
fn invalid_policies_owner_and_reasons_are_rejected_without_publishing() {
    for invalid in [
        SchedulerPolicy {
            max_concurrency: 0,
            ..policy(1, 1)
        },
        SchedulerPolicy {
            max_concurrency: 257,
            ..policy(1, 1)
        },
        SchedulerPolicy {
            max_attempts: 0,
            ..policy(1, 1)
        },
        SchedulerPolicy {
            max_attempts: 101,
            ..policy(1, 1)
        },
        SchedulerPolicy {
            lease_ms: 0,
            ..policy(1, 1)
        },
        SchedulerPolicy {
            lease_ms: 86_400_001,
            ..policy(1, 1)
        },
    ] {
        let fixture = Fixture::new();
        let clock = FakeClock::at(0);
        let mut journal = fixture.open();
        journal.initialize(graph()).unwrap();
        rejected_unchanged(&mut journal, |j| {
            j.configure_scheduler(rev(j), invalid, &clock)
        });
    }
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let mut journal = fixture.configured(policy(2, 3), &clock);
    for owner in ["", "   "] {
        rejected_unchanged(&mut journal, |j| {
            j.dispatch(rev(j), "a", task_rev(j, "a"), owner, &clock)
        });
    }
    let a = FakeWorker::launch(&mut journal, "a", &clock);
    rejected_unchanged(&mut journal, |j| {
        j.cancel_task(rev(j), "a", task_rev(j, "a"), "  ", &clock)
    });
    rejected_unchanged(&mut journal, |j| {
        j.confirm_stopped(rev(j), &a.0.lease, "", &clock)
    });
}

#[test]
fn competing_fake_hosts_cannot_publish_two_dispatches_at_the_same_revision() {
    let fixture = Fixture::new();
    let clock = FakeClock::at(0);
    let journal = fixture.configured(policy(1, 3), &clock);
    let revision = rev(&journal);
    let task_revision = task_rev(&journal, "a");
    drop(journal);
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for owner in ["host-one", "host-two"] {
        let root = fixture.root.clone();
        let context = fixture.context.clone();
        let barrier = barrier.clone();
        let clock = clock.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            TaskJournal::open(&root, context)
                .and_then(|mut j| j.dispatch(revision, "a", task_revision, owner, &clock))
        }));
    }
    let results: Vec<_> = threads.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let journal = fixture.open();
    assert_eq!(rev(&journal), revision + 1);
    let status = journal.scheduler_status(&clock).unwrap();
    assert_eq!(status.attempts.len(), 1);
    assert_eq!(status.active_attempts, 1);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Running);
}

struct CliFixture {
    fixture: Fixture,
    base: String,
}
impl CliFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        git(&fixture.root, &["init", "-q"]);
        git(&fixture.root, &["config", "user.name", "Scheduler Tests"]);
        git(
            &fixture.root,
            &["config", "user.email", "scheduler@example.invalid"],
        );
        git(&fixture.root, &["config", "core.autocrlf", "false"]);
        for directory in ["agents".into(), format!("stories/{STORY}"), "app".into()] {
            fs::create_dir_all(fixture.root.join(directory)).unwrap();
        }
        fs::write(
            fixture.root.join(".gitignore"),
            ".storyflow/\n*.json\nreport.md\n!storyflow.json\n",
        )
        .unwrap();
        for role in ["orchestrator", "planner", "implementer", "reviewer"] {
            fs::write(
                fixture.root.join(format!("agents/{role}.md")),
                format!("# {role}\n"),
            )
            .unwrap();
        }
        fs::write(
            fixture.root.join(format!("stories/{STORY}/story.md")),
            "# Scheduler fixture\nAC1\n",
        )
        .unwrap();
        fs::write(
            fixture.root.join(format!("stories/{STORY}/story.json")),
            "{\"scope\":[\"app\"]}",
        )
        .unwrap();
        fs::write(fixture.root.join("app/value"), "baseline\n").unwrap();
        fs::write(fixture.root.join("storyflow.json"), serde_json::to_vec(&json!({
            "version":1,"protected":[],"checks":[{"id":"check","program":"git","args":["--version"],"timeout_seconds":10}]
        })).unwrap()).unwrap();
        git(&fixture.root, &["add", "--force", "."]);
        git(
            &fixture.root,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=.git/no-hooks",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        let base = git(&fixture.root, &["rev-parse", "HEAD"]);
        ok(cli(&fixture.root, false, &["init", STORY, "HEAD"]));
        Self { fixture, base }
    }
    fn write_json(&self, filename: &str, value: &impl serde::Serialize) -> PathBuf {
        let path = self.fixture.root.join(filename);
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }
    fn initialize(&self, concurrency: u16) {
        let mut g = graph();
        g.base = self.base.clone();
        let path = self.write_json("graph.json", &g);
        ok(self.tasks(&["init", STORY, path.to_str().unwrap()]));
        let path = self.write_json(
            "policy.json",
            &SchedulerPolicy {
                lease_ms: 86_400_000,
                ..policy(concurrency, 3)
            },
        );
        ok(self.tasks(&["configure", STORY, "0", path.to_str().unwrap()]));
    }
    fn tasks(&self, args: &[&str]) -> Output {
        cli(&self.fixture.root, true, args)
    }
    fn status(&self) -> Value {
        ok(self.tasks(&["status", STORY]))
    }
}
fn clean_command(program: &str, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .env_remove("STORYFLOW_HOST_CONTEXT")
        .env_remove("STORYFLOW_HOST_CONTEXT_ROOT");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1").env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    command
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = clean_command("git", root).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn cli(root: &Path, task_cli: bool, args: &[&str]) -> Output {
    clean_command(
        if task_cli {
            env!("CARGO_BIN_EXE_storyflow-tasks")
        } else {
            env!("CARGO_BIN_EXE_storyflow")
        },
        root,
    )
    .args(args)
    .output()
    .unwrap()
}
fn ok(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn cli_rejected(output: Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn companion_scheduler_keeps_story_bytes_pinned_and_checks_lock_lease_and_result() {
    let f = CliFixture::new();
    let story_event = f
        .fixture
        .root
        .join(format!(".storyflow/{STORY}/events/000000000000.json"));
    let original = fs::read(&story_event).unwrap();
    f.initialize(2);
    let worker = FakeWorker(
        serde_json::from_value(ok(f.tasks(&["dispatch", STORY, "1", "a", "0", "host"]))).unwrap(),
    );
    let token = f.write_json("lease.json", &worker.0.lease);
    cli_rejected(f.tasks(&["start", STORY, "2", "b", "0"]));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.fixture.root.join(format!(".storyflow/{STORY}/run.lock")))
        .unwrap();
    lock.try_lock().unwrap();
    cli_rejected(f.tasks(&["heartbeat", STORY, "2", token.to_str().unwrap()]));
    drop(lock);
    let status = ok(f.tasks(&["heartbeat", STORY, "2", token.to_str().unwrap()]));
    assert_eq!(status["ledger_revision"], 3);
    assert_eq!(status["active_attempts"], 1);
    let path = f.write_json(
        "attempt-result.json",
        &worker.result(ResultStatus::Succeeded),
    );
    ok(f.tasks(&["finish", STORY, "3", path.to_str().unwrap()]));
    cli_rejected(f.tasks(&["finish", STORY, "4", path.to_str().unwrap()]));
    let status = f.status();
    assert_eq!(status["ledger_revision"], 4);
    assert_eq!(status["active_attempts"], 0);
    assert_eq!(fs::read(&story_event).unwrap(), original);
    assert_eq!(
        ok(cli(&f.fixture.root, false, &["status", STORY]))["revision"],
        0
    );
    let view = ok(f.tasks(&["view", STORY]));
    assert_eq!(view["tasks"]["a"]["status"], "succeeded");
}

#[test]
fn concurrent_cli_hosts_cannot_dispatch_past_capacity_or_reuse_a_ledger_revision() {
    let f = CliFixture::new();
    f.initialize(1);
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for task in ["a", "b"] {
        let root = f.fixture.root.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            cli(&root, true, &["dispatch", STORY, "1", task, "0", "host"])
        }));
    }
    let outputs: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1
    );
    assert_eq!(f.status()["active_attempts"], 1);
    assert_eq!(f.status()["ledger_revision"], 2);
}

#[test]
fn companion_block_resume_replacement_requires_stop_and_fences_prior_graph_results() {
    let f = CliFixture::new();
    f.initialize(1);
    let old = FakeWorker(
        serde_json::from_value(ok(f.tasks(&["dispatch", STORY, "1", "a", "0", "host"]))).unwrap(),
    );
    let token = f.write_json("lease.json", &old.0.lease);
    let result = f.write_json("attempt-result.json", &old.result(ResultStatus::Succeeded));
    fs::write(
        f.fixture.root.join("report.md"),
        "AC1 block/resume fixture\n",
    )
    .unwrap();
    ok(cli(
        &f.fixture.root,
        false,
        &["submit", STORY, "0", "block", "report.md"],
    ));
    assert_eq!(f.status()["current"], false);
    cli_rejected(f.tasks(&["finish", STORY, "2", result.to_str().unwrap()]));
    ok(cli(
        &f.fixture.root,
        false,
        &["submit", STORY, "1", "resume", "report.md"],
    ));
    assert_eq!(f.status()["current"], false);
    let mut replacement = graph();
    replacement.base = f.base.clone();
    replacement.story_revision = 2;
    let path = f.write_json("replacement.json", &replacement);
    ok(f.tasks(&["replace", STORY, "2", path.to_str().unwrap()]));
    let view = ok(f.tasks(&["view", STORY]));
    let revision = view["tasks"]["a"]["revision"].as_u64().unwrap().to_string();
    cli_rejected(f.tasks(&["finish", STORY, "3", result.to_str().unwrap()]));
    cli_rejected(f.tasks(&["dispatch", STORY, "3", "a", &revision, "host"]));
    ok(f.tasks(&[
        "stopped",
        STORY,
        "3",
        token.to_str().unwrap(),
        "host observed old worker exit",
    ]));
    let fresh = ok(f.tasks(&["dispatch", STORY, "4", "a", &revision, "host"]));
    assert_ne!(fresh["lease"]["attempt_id"], json!(old.0.lease.attempt_id));
    cli_rejected(f.tasks(&["finish", STORY, "5", result.to_str().unwrap()]));
    assert_eq!(f.status()["active_attempts"], 1);
}

#[test]
fn invalid_scheduler_cli_grammar_never_creates_a_run() {
    let fixture = Fixture::new();
    git(&fixture.root, &["init", "-q"]);
    for args in [
        vec!["configure", "invented-001"],
        vec!["status", "invented-001", "--clock", "0"],
        vec!["dispatch", "invented-001", "0", "a"],
        vec!["stopped", "invented-001", "0", "token.json"],
    ] {
        cli_rejected(cli(&fixture.root, true, &args));
    }
    assert!(!fixture.root.join(".storyflow/invented-001").exists());
}

fn strict_fixture<T>(filename: &str) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug + PartialEq,
{
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/task-fixtures")
        .join(filename);
    let bytes = fs::read(path).unwrap();
    let value: T = serde_json::from_slice(&bytes).unwrap();
    let roundtrip: T = serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(roundtrip, value);
    let mut invalid = serde_json::to_value(&value).unwrap();
    invalid["bypass"] = json!(true);
    assert!(serde_json::from_value::<T>(invalid).is_err());
    value
}

#[test]
fn scheduler_wire_fixtures_roundtrip_and_reject_unknown_fields() {
    let policy: SchedulerPolicy = strict_fixture("scheduler-policy.json");
    let lease: LeaseToken = strict_fixture("scheduler-lease.json");
    let packet: DispatchPacket = strict_fixture("scheduler-dispatch.json");
    let result: AttemptResult = strict_fixture("scheduler-result.json");
    assert!(policy.max_concurrency > 0 && policy.max_attempts > 0 && policy.lease_ms > 0);
    assert_eq!(packet.lease, lease);
    assert_eq!(result.lease, lease);
    assert_eq!(result.result.task, packet.packet.task.spec.id);
    assert_eq!(result.result.task_revision, packet.packet.task.revision);
    assert!(result.worker_stopped);
    let mut invalid = serde_json::to_value(&result).unwrap();
    invalid["lease"]["bypass"] = json!(true);
    assert!(serde_json::from_value::<AttemptResult>(invalid).is_err());
}
