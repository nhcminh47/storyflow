use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use storyflow::tasks::*;

const STORY: &str = "capacity-001";
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
}
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/task-capacity-tests")
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
                protected: Vec::new(),
            },
        }
    }
    fn open(&self) -> TaskJournal {
        TaskJournal::open(&self.root, self.context.clone()).unwrap()
    }
    fn events(&self) -> PathBuf {
        self.root.join(format!(".storyflow/{STORY}/tasks/events"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(
            self.root.starts_with(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("target/task-capacity-tests")
            )
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn graph() -> TaskGraph {
    TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: ["a", "b", "done", "failed", "fresh"]
            .into_iter()
            .map(|id| TaskSpec {
                id: id.into(),
                story: STORY.into(),
                prerequisites: Vec::new(),
                stage: TaskStage::Plan,
                read_scope: Vec::new(),
                write_scope: vec![format!("app/{id}")],
                acceptance_criteria: vec!["AC1".into()],
                decision_owner: "orchestrator".into(),
            })
            .collect(),
    }
}
fn rev(journal: &TaskJournal) -> u64 {
    journal.view().revision.unwrap()
}
fn launch(journal: &mut TaskJournal, task: &str, clock: &FakeClock) -> DispatchPacket {
    journal
        .dispatch(
            rev(journal),
            task,
            journal.view().tasks[task].revision,
            "host",
            clock,
        )
        .unwrap()
}
fn finish(packet: &DispatchPacket, status: ResultStatus) -> AttemptResult {
    AttemptResult {
        schema: 1,
        lease: packet.lease.clone(),
        result: WorkerResult {
            schema: 1,
            story: STORY.into(),
            task: packet.packet.task.spec.id.clone(),
            task_revision: packet.packet.task.revision,
            status,
            report: "AC1 finished and stopped".into(),
            commit: None,
            artifacts: Vec::new(),
        },
        worker_stopped: true,
    }
}

#[test]
fn heartbeat_limit_preserves_staggered_recovery_and_stop_across_restart() {
    let fixture = Fixture::new();
    let clock = FakeClock(Cell::new(1_000));
    let mut journal = fixture.open();
    journal.initialize(graph()).unwrap();
    journal
        .configure_scheduler(
            rev(&journal),
            SchedulerPolicy {
                max_concurrency: 3,
                max_attempts: 3,
                lease_ms: 100,
            },
            &clock,
        )
        .unwrap();
    let done = launch(&mut journal, "done", &clock);
    journal
        .complete_attempt(
            rev(&journal),
            finish(&done, ResultStatus::Succeeded),
            &clock,
        )
        .unwrap();
    let failed = launch(&mut journal, "failed", &clock);
    journal
        .complete_attempt(rev(&journal), finish(&failed, ResultStatus::Failed), &clock)
        .unwrap();
    let a = launch(&mut journal, "a", &clock);
    clock.0.set(1_001);
    let b = launch(&mut journal, "b", &clock);
    let first = fs::read(fixture.events().join("000000000000.json")).unwrap();

    // Exercise real publication, file sync and checksums at the actual 4096
    // event boundary. Neither clock time nor worker expiry depends on sleeps.
    loop {
        let before = journal.view().clone();
        match journal.heartbeat(rev(&journal), &b.lease, &clock) {
            Ok(()) => (),
            Err(error) => {
                assert_eq!(error.code, "task_journal_capacity");
                assert_eq!(journal.view(), &before);
                break;
            }
        }
    }
    let status = journal.scheduler_status(&clock).unwrap();
    let capacity = status.journal.unwrap();
    assert_eq!(
        capacity.published_events + capacity.reserved_events,
        capacity.max_events
    );
    assert_eq!(capacity.reserved_events, 4);
    assert_eq!(capacity.reserved_bytes, 2 * MAX_EVENT_BYTES + 2 * 1024);
    assert!(capacity.draining);
    assert!(!status.dispatchable.contains(&"fresh".into()));
    assert!(
        status.blockers["fresh"]
            .iter()
            .any(|b| b.code == "journal_capacity")
    );
    let before = journal.view().clone();
    let event_count = fs::read_dir(fixture.events()).unwrap().count();
    assert_eq!(
        journal
            .dispatch(rev(&journal), "fresh", 0, "host", &clock)
            .unwrap_err()
            .code,
        "task_journal_capacity"
    );
    assert_eq!(
        journal
            .retry_task(
                rev(&journal),
                "failed",
                journal.view().tasks["failed"].revision,
                "try again",
                &clock,
            )
            .unwrap_err()
            .code,
        "task_journal_capacity"
    );
    assert_eq!(journal.view(), &before);
    assert_eq!(fs::read_dir(fixture.events()).unwrap().count(), event_count);
    assert!(!fixture.events().join("pending.tmp").exists());
    drop(journal);

    let mut journal = fixture.open();
    assert_eq!(journal.view(), &before);
    clock.0.set(1_100);
    journal.recover_expired(rev(&journal), &clock).unwrap();
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Blocked);
    assert_eq!(journal.view().tasks["b"].status, TaskStatus::Running);
    journal
        .confirm_stopped(rev(&journal), &a.lease, &"\\".repeat(4096), &clock)
        .unwrap();
    clock.0.set(1_101);
    journal.recover_expired(rev(&journal), &clock).unwrap();
    journal
        .confirm_stopped(rev(&journal), &b.lease, "host confirmed exit", &clock)
        .unwrap();
    let status = journal.scheduler_status(&clock).unwrap();
    let capacity = status.journal.unwrap();
    assert_eq!(capacity.published_events, capacity.max_events);
    assert_eq!(capacity.reserved_events, 0);
    assert_eq!(capacity.reserved_bytes, 0);
    assert_eq!(status.active_attempts, 0);
    assert_eq!(journal.view().tasks["done"].status, TaskStatus::Succeeded);
    let drained = journal.view().clone();
    drop(journal);
    let journal = fixture.open();
    assert_eq!(journal.view(), &drained);
    assert_eq!(
        fs::read(fixture.events().join("000000000000.json")).unwrap(),
        first
    );
    assert_eq!(journal.scheduler_status(&clock).unwrap().active_attempts, 0);
}

#[test]
fn byte_pressure_from_legacy_record_encoding_still_funds_completion_after_reopen() {
    let fixture = Fixture::new();
    let clock = FakeClock(Cell::new(1_000));
    let mut journal = fixture.open();
    journal.initialize(graph()).unwrap();
    journal
        .configure_scheduler(
            rev(&journal),
            SchedulerPolicy {
                max_concurrency: 3,
                max_attempts: 3,
                lease_ms: 100,
            },
            &clock,
        )
        .unwrap();
    let worker = launch(&mut journal, "a", &clock);
    for _ in 0..61 {
        journal
            .heartbeat(rev(&journal), &worker.lease, &clock)
            .unwrap();
    }
    let capacity = journal.scheduler_status(&clock).unwrap().journal.unwrap();
    let target_bytes = capacity.max_bytes - capacity.reserved_bytes;
    drop(journal);

    // Older record encodings can have trailing whitespace. Hashes bind the
    // parsed body, so padding gives genuine byte pressure without rewriting
    // event contents/checksums or requiring tens of thousands of heartbeats.
    let mut paths: Vec<_> = fs::read_dir(fixture.events())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    let mut padding = target_bytes - capacity.published_bytes;
    for path in &paths {
        let mut bytes = fs::read(path).unwrap();
        let added = padding.min(MAX_EVENT_BYTES - bytes.len());
        bytes.resize(bytes.len() + added, b' ');
        fs::write(path, bytes).unwrap();
        padding -= added;
    }
    assert_eq!(padding, 0);
    let first = fs::read(&paths[0]).unwrap();
    let mut journal = fixture.open();
    let before = journal.view().clone();
    let capacity = journal.scheduler_status(&clock).unwrap().journal.unwrap();
    assert_eq!(
        capacity.published_bytes + capacity.reserved_bytes,
        capacity.max_bytes
    );
    assert!(
        capacity.published_events < 100,
        "byte limit must act independently of event count"
    );
    assert!(capacity.draining);
    assert_eq!(
        journal
            .heartbeat(rev(&journal), &worker.lease, &clock)
            .unwrap_err()
            .code,
        "task_journal_capacity"
    );
    assert_eq!(journal.view(), &before);
    assert_eq!(fs::read_dir(fixture.events()).unwrap().count(), paths.len());
    journal
        .complete_attempt(
            rev(&journal),
            finish(&worker, ResultStatus::Succeeded),
            &clock,
        )
        .unwrap();
    let after = journal.view().clone();
    drop(journal);
    let journal = fixture.open();
    assert_eq!(journal.view(), &after);
    assert_eq!(journal.view().tasks["a"].status, TaskStatus::Succeeded);
    assert_eq!(fs::read(&paths[0]).unwrap(), first);
    let capacity = journal.scheduler_status(&clock).unwrap().journal.unwrap();
    assert_eq!(capacity.reserved_events, 0);
    assert!(capacity.published_bytes <= capacity.max_bytes);
}
