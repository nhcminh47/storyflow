use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use storyflow::tasks::*;

const STORY: &str = "blockers-001";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct FakeClock(Cell<u64>);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

struct Fixture {
    root: PathBuf,
    context: RunContext,
    graph: TaskGraph,
}
impl Fixture {
    fn new(tasks: Vec<TaskSpec>) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/task-scheduler-blockers-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(root.join(format!(".storyflow/{STORY}/events"))).unwrap();
        let context = RunContext {
            story: STORY.into(),
            base: "1".repeat(40),
            story_revision: 0,
            stage: TaskStage::Plan,
            blocked: false,
            scope: vec!["app".into()],
            protected: Vec::new(),
        };
        let graph = TaskGraph {
            schema: 1,
            story: context.story.clone(),
            base: context.base.clone(),
            story_revision: 0,
            stage: context.stage,
            tasks,
        };
        Self {
            root,
            context,
            graph,
        }
    }
    fn open(&self) -> TaskJournal {
        TaskJournal::open(&self.root, self.context.clone()).unwrap()
    }
    fn configured(&self, clock: &FakeClock) -> TaskJournal {
        let mut journal = self.open();
        journal.initialize(self.graph.clone()).unwrap();
        journal
            .configure_scheduler(
                rev(&journal),
                SchedulerPolicy {
                    max_concurrency: 8,
                    max_attempts: 3,
                    lease_ms: 100,
                },
                clock,
            )
            .unwrap();
        journal
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let allowed =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/task-scheduler-blockers-tests");
        assert!(self.root.starts_with(allowed));
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn task(id: &str, prerequisites: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        story: STORY.into(),
        prerequisites: prerequisites.iter().map(|id| (*id).into()).collect(),
        stage: TaskStage::Plan,
        read_scope: vec!["app".into()],
        write_scope: vec![format!("app/{id}")],
        acceptance_criteria: vec!["AC1 fixture evidence".into()],
        decision_owner: "orchestrator".into(),
    }
}
fn rev(journal: &TaskJournal) -> u64 {
    journal.view().revision.unwrap()
}
fn task_rev(journal: &TaskJournal, id: &str) -> u64 {
    journal.view().tasks[id].revision
}
fn dispatch(journal: &mut TaskJournal, id: &str, clock: &FakeClock) -> DispatchPacket {
    journal
        .dispatch(rev(journal), id, task_rev(journal, id), "host", clock)
        .unwrap()
}
fn finish(
    journal: &mut TaskJournal,
    packet: &DispatchPacket,
    status: ResultStatus,
    clock: &FakeClock,
) {
    journal
        .complete_attempt(
            rev(journal),
            AttemptResult {
                schema: 1,
                lease: packet.lease.clone(),
                result: WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: packet.packet.task.spec.id.clone(),
                    task_revision: packet.packet.task.revision,
                    status,
                    report: "AC1 worker returned and exited".into(),
                    commit: None,
                    artifacts: Vec::new(),
                },
                worker_stopped: true,
            },
            clock,
        )
        .unwrap();
}
fn dependencies(status: &SchedulerStatus, id: &str) -> Vec<(String, String)> {
    status
        .blockers
        .get(id)
        .into_iter()
        .flatten()
        .filter_map(|reason| {
            reason.dependency.as_ref().map(|dependency| {
                assert!(reason.attempt_id.is_none());
                (dependency.clone(), reason.code.clone())
            })
        })
        .collect()
}
fn expected(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(dependency, code)| ((*dependency).into(), (*code).into()))
        .collect()
}

#[test]
fn mixed_prerequisite_blockers_identify_exact_causes_in_sorted_order() {
    let fixture = Fixture::new(vec![
        task("succeeded", &[]),
        task("failed", &[]),
        task("blocked", &[]),
        task("cancelled", &[]),
        task("running", &[]),
        task("ready", &[]),
        task("pending", &["ready"]),
        task(
            "target",
            &[
                "succeeded",
                "running",
                "ready",
                "pending",
                "failed",
                "cancelled",
                "blocked",
            ],
        ),
    ]);
    let clock = FakeClock(Cell::new(0));
    let mut journal = fixture.configured(&clock);
    for (id, status) in [
        ("succeeded", ResultStatus::Succeeded),
        ("failed", ResultStatus::Failed),
        ("blocked", ResultStatus::Blocked),
    ] {
        let packet = dispatch(&mut journal, id, &clock);
        finish(&mut journal, &packet, status, &clock);
    }
    journal
        .cancel_task(
            rev(&journal),
            "cancelled",
            task_rev(&journal, "cancelled"),
            "explicit cancellation",
            &clock,
        )
        .unwrap();
    dispatch(&mut journal, "running", &clock);
    let before = journal.view().clone();
    let status = journal.scheduler_status(&clock).unwrap();
    assert_eq!(
        dependencies(&status, "target"),
        expected(&[
            ("blocked", "dependency_blocked"),
            ("cancelled", "dependency_cancelled"),
            ("failed", "dependency_failed"),
            ("pending", "dependency_pending"),
            ("ready", "dependency_pending"),
            ("running", "dependency_pending"),
        ])
    );
    assert_eq!(journal.view(), &before, "status must remain read-only");
    assert_eq!(journal.view().tasks["target"].status, TaskStatus::Pending);
    assert!(!status.dispatchable.iter().any(|id| id == "target"));
    assert!(status.blockers["target"].iter().all(|reason| {
        reason.code != "dependencies_pending"
            && reason.detail.contains(reason.dependency.as_ref().unwrap())
    }));
    drop(journal);
    let journal = fixture.open();
    assert_eq!(journal.scheduler_status(&clock).unwrap(), status);
}

#[test]
fn failure_and_retry_update_dependency_diagnostics_without_unlocking_early() {
    let fixture = Fixture::new(vec![task("a", &[]), task("b", &["a"])]);
    let clock = FakeClock(Cell::new(0));
    let mut journal = fixture.configured(&clock);
    let packet = dispatch(&mut journal, "a", &clock);
    finish(&mut journal, &packet, ResultStatus::Failed, &clock);
    assert_eq!(
        dependencies(&journal.scheduler_status(&clock).unwrap(), "b"),
        expected(&[("a", "dependency_failed")])
    );
    journal
        .retry_task(rev(&journal), "a", task_rev(&journal, "a"), "retry", &clock)
        .unwrap();
    assert_eq!(
        dependencies(&journal.scheduler_status(&clock).unwrap(), "b"),
        expected(&[("a", "dependency_pending")])
    );
    assert_eq!(journal.view().tasks["b"].status, TaskStatus::Pending);
    let packet = dispatch(&mut journal, "a", &clock);
    assert_eq!(
        dependencies(&journal.scheduler_status(&clock).unwrap(), "b"),
        expected(&[("a", "dependency_pending")])
    );
    finish(&mut journal, &packet, ResultStatus::Succeeded, &clock);
    let status = journal.scheduler_status(&clock).unwrap();
    assert!(dependencies(&status, "b").is_empty());
    assert!(!status.blockers.contains_key("b"));
    assert_eq!(status.dispatchable, ["b"]);
}

#[test]
fn expiry_recovery_and_explicit_stop_keep_dependency_blocked_until_retry_success() {
    let fixture = Fixture::new(vec![task("a", &[]), task("b", &["a"])]);
    let clock = FakeClock(Cell::new(0));
    let mut journal = fixture.configured(&clock);
    let packet = dispatch(&mut journal, "a", &clock);
    clock.0.set(packet.expires_at_ms);
    journal.recover_expired(rev(&journal), &clock).unwrap();
    assert_eq!(
        dependencies(&journal.scheduler_status(&clock).unwrap(), "b"),
        expected(&[("a", "dependency_blocked")])
    );
    journal
        .confirm_stopped(rev(&journal), &packet.lease, "host confirmed exit", &clock)
        .unwrap();
    assert_eq!(
        dependencies(&journal.scheduler_status(&clock).unwrap(), "b"),
        expected(&[("a", "dependency_blocked")])
    );
    journal
        .retry_task(rev(&journal), "a", task_rev(&journal, "a"), "retry", &clock)
        .unwrap();
    let packet = dispatch(&mut journal, "a", &clock);
    finish(&mut journal, &packet, ResultStatus::Succeeded, &clock);
    assert_eq!(
        journal.scheduler_status(&clock).unwrap().dispatchable,
        ["b"]
    );
}

#[test]
fn cancellation_reports_the_terminal_prerequisite_in_status_json() {
    let fixture = Fixture::new(vec![task("a", &[]), task("b", &["a"])]);
    let clock = FakeClock(Cell::new(0));
    let mut journal = fixture.configured(&clock);
    journal
        .cancel_task(
            rev(&journal),
            "a",
            task_rev(&journal, "a"),
            "cancel",
            &clock,
        )
        .unwrap();
    let status = journal.scheduler_status(&clock).unwrap();
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(value["blockers"]["b"][0]["dependency"], "a");
    assert_eq!(value["blockers"]["b"][0]["code"], "dependency_cancelled");
    assert!(value["blockers"]["a"][0].get("dependency").is_none());
    assert_eq!(
        serde_json::from_value::<SchedulerStatus>(value).unwrap(),
        status
    );
    assert_eq!(journal.view().tasks["b"].status, TaskStatus::Pending);
    assert!(status.dispatchable.is_empty());
}

#[test]
fn dependency_field_preserves_old_blocker_json_and_rejects_unknown_fields() {
    let old = json!({
        "code": "task_running",
        "detail": "wait for the worker",
        "attempt_id": null
    });
    let blocker: SchedulerBlocker = serde_json::from_value(old.clone()).unwrap();
    assert!(blocker.dependency.is_none());
    assert_eq!(serde_json::to_value(&blocker).unwrap(), old);
    let current = json!({
        "code": "dependency_failed",
        "detail": "a failed",
        "attempt_id": null,
        "dependency": "a"
    });
    let blocker: SchedulerBlocker = serde_json::from_value(current.clone()).unwrap();
    assert_eq!(blocker.dependency.as_deref(), Some("a"));
    assert_eq!(serde_json::to_value(&blocker).unwrap(), current);
    let mut invalid: Value = current;
    invalid["unknown"] = json!(true);
    assert!(serde_json::from_value::<SchedulerBlocker>(invalid).is_err());
}
