use super::model::*;
use super::scheduler::{
    self, AttemptPhase, AttemptResult, Clock, DispatchPacket, JournalCapacity, LeaseToken,
    SchedulerBlocker, SchedulerEvent, SchedulerPolicy, SchedulerStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const MAX_EVENT_BYTES: usize = 1_048_576;
const MAX_EVENTS: usize = 4096;
const MAX_JOURNAL_BYTES: usize = 67_108_864;
// Recovery has no variable-size fields. Dispatch only has bounded task/owner
// strings. Tests cover their maximum encoded records; publication checks these
// bounds too so future schema changes cannot silently invalidate the reserve.
const MAX_RECOVER_RECORD_BYTES: usize = 1024;
const MAX_DISPATCH_RECORD_BYTES: usize = 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Body {
    schema: u32,
    revision: u64,
    previous_sha256: Option<String>,
    event: TaskEvent,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    body: Body,
    sha256: String,
}

/// Single-writer task journal. The host must additionally hold the story's run
/// lock for the lifetime of this value; the companion CLI enforces that ordering.
pub struct TaskJournal {
    dir: PathBuf,
    context: RunContext,
    view: TaskView,
    last: Option<String>,
    events: usize,
    bytes: usize,
    graph_revision: u64,
    _lock: File,
}
impl TaskJournal {
    pub fn open(root: &Path, context: RunContext) -> TaskResult<Self> {
        context.validate()?;
        let run = root.join(".storyflow").join(&context.story);
        require(
            run.join("events").is_dir(),
            "task_run_missing",
            "initialize the parent story run first",
        )?;
        regular_dir(&root.join(".storyflow"))?;
        regular_dir(&run)?;
        let lock_path = run.join("tasks.lock");
        safe_existing_file(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|_| error("task_locked", "another task writer owns the lock"))?;
        let dir = run.join("tasks");
        create_dir(&dir)?;
        create_dir(&dir.join("events"))?;
        let mut journal = Self {
            dir,
            view: TaskView::empty(&context),
            context,
            last: None,
            events: 0,
            bytes: 0,
            graph_revision: 0,
            _lock: lock,
        };
        let mut entries = fs::read_dir(journal.dir.join("events"))?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;
        require(
            entries.len() <= MAX_EVENTS + 1,
            "task_journal_limit",
            "too many journal entries",
        )?;
        entries.sort();
        for path in entries {
            if path.file_name().is_some_and(|n| n == "pending.tmp") {
                safe_existing_file(&path)?;
                continue;
            }
            require(
                path == journal.event_path(journal.events),
                "task_integrity",
                "unexpected or missing task journal entry",
            )?;
            require(
                journal.events < MAX_EVENTS,
                "task_journal_limit",
                "task event limit reached",
            )?;
            safe_existing_file(&path)?;
            let bytes = read_document(&path)?;
            journal.bytes += bytes.len();
            require(
                journal.bytes <= MAX_JOURNAL_BYTES,
                "task_journal_limit",
                "task journal exceeds size limit",
            )?;
            let record: Record = serde_json::from_slice(&bytes)?;
            require(
                record.body.schema == SCHEMA
                    && record.body.revision == journal.events as u64
                    && record.body.previous_sha256 == journal.last
                    && record.sha256 == checksum(&record.body)?,
                "task_integrity",
                "task journal checksum or revision mismatch",
            )?;
            require(
                journal.events > 0 || matches!(record.body.event, TaskEvent::Define { .. }),
                "task_integrity",
                "task journal must begin with graph definition",
            )?;
            if matches!(
                record.body.event,
                TaskEvent::Define { .. } | TaskEvent::Replace { .. }
            ) {
                journal.graph_revision = record.body.revision;
            }
            apply(
                &mut journal.view,
                &journal.context,
                &record.body.event,
                journal.graph_revision,
            )?;
            journal.view.revision = Some(record.body.revision);
            journal.events += 1;
            journal.last = Some(record.sha256);
        }
        Ok(journal)
    }

    pub fn view(&self) -> &TaskView {
        &self.view
    }

    pub fn initialize(&mut self, graph: TaskGraph) -> TaskResult<()> {
        require(
            self.view.revision.is_none(),
            "task_already_initialized",
            "task graph already exists",
        )?;
        self.require_current_graph(&graph)?;
        self.append(TaskEvent::Define { graph })
    }

    /// Publish a fresh graph after a story transition, retaining the old events.
    /// The ledger revision and task revisions never reset across replacements.
    pub fn replace(&mut self, revision: u64, graph: TaskGraph) -> TaskResult<()> {
        self.expect_store_revision(revision)?;
        self.require_current_graph(&graph)?;
        self.append(TaskEvent::Replace { graph })
    }

    fn require_current_graph(&self, graph: &TaskGraph) -> TaskResult<()> {
        require(
            !self.context.blocked
                && self.context.stage != TaskStage::Done
                && graph.story_revision == self.context.story_revision
                && graph.stage == self.context.stage,
            "stale_story_context",
            "graph must bind the current unblocked, unfinished story revision/stage",
        )
    }

    pub fn start(
        &mut self,
        revision: u64,
        task: &str,
        task_revision: u64,
    ) -> TaskResult<TaskPacket> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Start {
            task: task.to_owned(),
            task_revision,
        })?;
        self.view.packet(task)
    }

    pub fn complete(&mut self, revision: u64, result: WorkerResult) -> TaskResult<()> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Complete { result })
    }

    pub fn packet(&self, task: &str) -> TaskResult<TaskPacket> {
        self.require_live()?;
        self.view.packet(task)
    }

    /// Opt in once. Existing legacy running tasks must finish first; after this
    /// event legacy start/complete cannot bypass lease and concurrency checks.
    pub fn configure_scheduler(
        &mut self,
        revision: u64,
        policy: SchedulerPolicy,
        clock: &impl Clock,
    ) -> TaskResult<()> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Configure {
                policy,
                at_ms: clock.now_ms(),
            },
        })
    }

    pub fn scheduler_status(&self, clock: &impl Clock) -> TaskResult<SchedulerStatus> {
        let mut status =
            scheduler::status(&self.view, clock.now_ms(), self.require_live().is_ok())?;
        let (reserved_events, reserved_bytes) = reservations(&self.view);
        // Funding another attempt needs its dispatch plus a future recovery and
        // terminal event. A maximum-owner record is a conservative byte bound.
        let draining = !fits(self.events, reserved_events + 3, MAX_EVENTS)
            || !fits(
                self.bytes,
                reserved_bytes
                    + MAX_EVENT_BYTES
                    + MAX_RECOVER_RECORD_BYTES
                    + MAX_DISPATCH_RECORD_BYTES,
                MAX_JOURNAL_BYTES,
            );
        if draining {
            for task in std::mem::take(&mut status.dispatchable) {
                status.blockers.entry(task).or_default().push(SchedulerBlocker {
                    code: "journal_capacity".into(),
                    detail: "journal cannot fund another attempt and its cleanup; drain existing workers and recheck capacity".into(),
                    attempt_id: None,
                    dependency: None,
                });
            }
        }
        status.journal = Some(JournalCapacity {
            max_events: MAX_EVENTS,
            max_bytes: MAX_JOURNAL_BYTES,
            published_events: self.events,
            published_bytes: self.bytes,
            reserved_events,
            reserved_bytes,
            draining,
        });
        Ok(status)
    }

    /// Reserves capacity and publishes a packet. The host must actually start
    /// the provider worker and deduplicate launches by the returned attempt ID.
    pub fn dispatch(
        &mut self,
        revision: u64,
        task: &str,
        task_revision: u64,
        owner: &str,
        clock: &impl Clock,
    ) -> TaskResult<DispatchPacket> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Dispatch {
                task: task.into(),
                task_revision,
                owner: owner.into(),
                at_ms: clock.now_ms(),
            },
        })?;
        scheduler::dispatch_packet(&self.view, task)
    }

    pub fn heartbeat(
        &mut self,
        revision: u64,
        token: &LeaseToken,
        clock: &impl Clock,
    ) -> TaskResult<()> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Heartbeat {
                lease: token.clone(),
                at_ms: clock.now_ms(),
            },
        })
    }

    pub fn complete_attempt(
        &mut self,
        revision: u64,
        result: AttemptResult,
        clock: &impl Clock,
    ) -> TaskResult<()> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Complete {
                result,
                at_ms: clock.now_ms(),
            },
        })
    }

    pub fn cancel_task(
        &mut self,
        revision: u64,
        task: &str,
        task_revision: u64,
        reason: &str,
        clock: &impl Clock,
    ) -> TaskResult<()> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Cancel {
                task: task.into(),
                task_revision,
                reason: reason.into(),
                at_ms: clock.now_ms(),
            },
        })
    }

    /// Explicit expiry policy: fence and block expired attempts. Never infer
    /// worker exit, free capacity, or dispatch a replacement automatically.
    pub fn recover_expired(&mut self, revision: u64, clock: &impl Clock) -> TaskResult<()> {
        self.expect_revision(revision)?;
        let now = clock.now_ms();
        let scheduler = scheduler::configured(&self.view)?;
        scheduler::check_time(scheduler, now)?;
        if !scheduler
            .attempts
            .values()
            .any(|a| a.phase == AttemptPhase::Leased && !a.worker_stopped && now >= a.expires_at_ms)
        {
            return Ok(());
        }
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Recover { at_ms: now },
        })
    }

    /// Cleanup is allowed even after the parent changes, blocks, or finishes.
    /// This records a host assertion of exit; it cannot apply worker results.
    pub fn confirm_stopped(
        &mut self,
        revision: u64,
        token: &LeaseToken,
        reason: &str,
        clock: &impl Clock,
    ) -> TaskResult<()> {
        self.expect_store_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Stopped {
                lease: token.clone(),
                reason: reason.into(),
                at_ms: clock.now_ms(),
            },
        })
    }

    pub fn retry_task(
        &mut self,
        revision: u64,
        task: &str,
        task_revision: u64,
        reason: &str,
        clock: &impl Clock,
    ) -> TaskResult<TaskPacket> {
        self.expect_revision(revision)?;
        self.append(TaskEvent::Scheduler {
            event: SchedulerEvent::Retry {
                task: task.into(),
                task_revision,
                reason: reason.into(),
                at_ms: clock.now_ms(),
            },
        })?;
        self.view.packet(task)
    }

    fn require_live(&self) -> TaskResult<()> {
        require(
            !self.context.blocked
                && self.context.stage != TaskStage::Done
                && self.view.story_revision == self.context.story_revision
                && self.view.stage == self.context.stage,
            "stale_story_context",
            "story changed or is blocked; old task results cannot mutate or dispatch",
        )
    }
    fn expect_revision(&self, revision: u64) -> TaskResult<()> {
        self.require_live()?;
        self.expect_store_revision(revision)
    }
    fn expect_store_revision(&self, revision: u64) -> TaskResult<()> {
        require(
            self.view.revision == Some(revision),
            "stale_task_store_revision",
            "task journal revision changed; read view before retrying",
        )
    }
    fn append(&mut self, event: TaskEvent) -> TaskResult<()> {
        require(
            self.events < MAX_EVENTS,
            "task_journal_limit",
            "task event limit reached",
        )?;
        let mut next = self.view.clone();
        let graph_revision =
            if matches!(event, TaskEvent::Define { .. } | TaskEvent::Replace { .. }) {
                self.events as u64
            } else {
                self.graph_revision
            };
        apply(&mut next, &self.context, &event, graph_revision)?;
        let body = Body {
            schema: SCHEMA,
            revision: self.events as u64,
            previous_sha256: self.last.clone(),
            event,
        };
        let (record, bytes) = encode_record(body)?;
        require(
            bytes.len() <= MAX_EVENT_BYTES && fits(self.bytes, bytes.len(), MAX_JOURNAL_BYTES),
            "task_journal_limit",
            "task event or journal exceeds size limit",
        )?;
        let small_record_bound = match &record.body.event {
            TaskEvent::Scheduler {
                event: SchedulerEvent::Recover { .. },
            } => MAX_RECOVER_RECORD_BYTES,
            TaskEvent::Scheduler {
                event: SchedulerEvent::Dispatch { .. },
            } => MAX_DISPATCH_RECORD_BYTES,
            _ => MAX_EVENT_BYTES,
        };
        require(
            bytes.len() <= small_record_bound,
            "task_journal_capacity",
            "scheduler record exceeds its cleanup/admission size bound",
        )?;
        // Admission is deliberately not part of replay. Older histories remain
        // readable, while every newly admitted attempt funds its entire drain.
        // U unresolved workers each own one terminal record. Every still-leased
        // worker additionally owns one recovery record, including retired graph
        // workers until graph replacement fences their lease.
        let (reserved_events, reserved_bytes) = reservations(&next);
        let funded = fits(self.events, 1 + reserved_events, MAX_EVENTS)
            && fits(self.bytes, bytes.len() + reserved_bytes, MAX_JOURNAL_BYTES);
        let (previous_events, previous_bytes) = reservations(&self.view);
        let previously_funded = fits(self.events, previous_events, MAX_EVENTS)
            && fits(self.bytes, previous_bytes, MAX_JOURNAL_BYTES);
        // Pre-fix histories can already owe more cleanup than their remaining
        // budget. Permit only strictly obligation-reducing cleanup to make
        // progress there, still inside every actual hard limit. No new history
        // admitted by this implementation needs or can enter this exception.
        let old_underfunded_cleanup = !previously_funded
            && reserved_events < previous_events
            && reserved_bytes < previous_bytes
            && matches!(
                record.body.event,
                TaskEvent::Scheduler {
                    event: SchedulerEvent::Complete { .. }
                        | SchedulerEvent::Stopped { .. }
                        | SchedulerEvent::Recover { .. }
                }
            );
        require(
            funded || old_underfunded_cleanup,
            "task_journal_capacity",
            "journal headroom is reserved for finish/recover/stopped; drain existing workers and recheck capacity",
        )?;
        let path = self.event_path(self.events);
        require(
            !path.try_exists()?,
            "task_integrity",
            "refusing to overwrite a published task event",
        )?;
        let pending = path.with_file_name("pending.tmp");
        safe_existing_file(&pending)?;
        {
            let mut file = File::create(&pending)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(pending, &path)?;
        sync_parent(&path)?;
        next.revision = Some(record.body.revision);
        self.view = next;
        self.last = Some(record.sha256);
        self.events += 1;
        self.bytes += bytes.len();
        self.graph_revision = graph_revision;
        Ok(())
    }
    fn event_path(&self, revision: usize) -> PathBuf {
        self.dir.join("events").join(format!("{revision:012}.json"))
    }
}

fn reservations(view: &TaskView) -> (usize, usize) {
    let Some(scheduler) = &view.scheduler else {
        return (0, 0);
    };
    let mut unresolved = 0;
    let mut leased = 0;
    for attempt in scheduler.attempts.values().filter(|a| !a.worker_stopped) {
        unresolved += 1;
        leased += usize::from(attempt.phase == AttemptPhase::Leased);
    }
    (
        unresolved + leased,
        unresolved * MAX_EVENT_BYTES + leased * MAX_RECOVER_RECORD_BYTES,
    )
}

fn fits(used: usize, additional: usize, limit: usize) -> bool {
    used.checked_add(additional).is_some_and(|n| n <= limit)
}

fn encode_record(body: Body) -> TaskResult<(Record, Vec<u8>)> {
    let record = Record {
        sha256: checksum(&body)?,
        body,
    };
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    Ok((record, bytes))
}

pub fn read_document(path: &Path) -> TaskResult<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_EVENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() <= MAX_EVENT_BYTES,
        "task_document_limit",
        "task document exceeds 1 MiB",
    )?;
    Ok(bytes)
}
fn checksum(body: &Body) -> TaskResult<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(body)?)))
}
fn create_dir(path: &Path) -> TaskResult<()> {
    match fs::create_dir(path) {
        Ok(()) => sync_parent(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => regular_dir(path),
        Err(e) => Err(e.into()),
    }
}
fn sync_parent(path: &Path) -> TaskResult<()> {
    #[cfg(unix)]
    File::open(path.parent().expect("task storage has a parent"))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn regular_dir(path: &Path) -> TaskResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    require(
        metadata.is_dir() && !link(&metadata),
        "task_storage_path",
        "task storage must use physical directories",
    )
}
fn safe_existing_file(path: &Path) -> TaskResult<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => require(
            m.is_file() && !link(&m),
            "task_storage_path",
            "task storage entry must be a regular file",
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use crate::tasks::Artifact;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const STORY: &str = "capacity-unit";
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
                .join("target/task-capacity-unit")
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
        fn graph(&self, count: usize) -> TaskGraph {
            TaskGraph {
                schema: SCHEMA,
                story: STORY.into(),
                base: self.context.base.clone(),
                story_revision: self.context.story_revision,
                stage: self.context.stage,
                tasks: (0..count)
                    .map(|i| TaskSpec {
                        id: format!("task-{i:03}"),
                        story: STORY.into(),
                        prerequisites: Vec::new(),
                        stage: self.context.stage,
                        read_scope: Vec::new(),
                        write_scope: vec![format!("app/{i}")],
                        acceptance_criteria: vec!["AC1".into()],
                        decision_owner: "orchestrator".into(),
                    })
                    .collect(),
            }
        }
        fn configured(&self, count: usize, clock: &FakeClock) -> TaskJournal {
            let mut journal = TaskJournal::open(&self.root, self.context.clone()).unwrap();
            journal.initialize(self.graph(count)).unwrap();
            journal
                .configure_scheduler(
                    journal.view.revision.unwrap(),
                    SchedulerPolicy {
                        max_concurrency: 256,
                        max_attempts: 100,
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
            assert!(self.root.starts_with(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("target/task-capacity-unit")
            ));
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn launch(journal: &mut TaskJournal, task: &str, clock: &FakeClock) -> DispatchPacket {
        journal
            .dispatch(
                journal.view.revision.unwrap(),
                task,
                journal.view.tasks[task].revision,
                &"\\".repeat(128),
                clock,
            )
            .unwrap()
    }
    fn maximum_result(packet: &DispatchPacket) -> AttemptResult {
        AttemptResult {
            schema: SCHEMA,
            lease: packet.lease.clone(),
            worker_stopped: true,
            result: WorkerResult {
                schema: SCHEMA,
                story: STORY.into(),
                task: packet.packet.task.spec.id.clone(),
                task_revision: packet.packet.task.revision,
                status: ResultStatus::Succeeded,
                report: "\0".repeat(MAX_REPORT_BYTES),
                commit: Some("a".repeat(64)),
                artifacts: (0..64)
                    .map(|i| {
                        let prefix = format!("app/{i:02}-");
                        Artifact {
                            path: format!("{prefix}{}", "x".repeat(1024 - prefix.len())),
                            sha256: "f".repeat(64),
                        }
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn bounded_recovery_and_maximum_owner_dispatch_records_fit_reserved_allowances() {
        let bytes_for = |event| {
            encode_record(Body {
                schema: SCHEMA,
                revision: u64::MAX,
                previous_sha256: Some("f".repeat(64)),
                event: TaskEvent::Scheduler { event },
            })
            .unwrap()
            .1
            .len()
        };
        assert!(bytes_for(SchedulerEvent::Recover { at_ms: u64::MAX }) <= MAX_RECOVER_RECORD_BYTES);
        assert!(
            bytes_for(SchedulerEvent::Dispatch {
                task: "a".repeat(40),
                task_revision: u64::MAX,
                owner: "\\".repeat(128),
                at_ms: u64::MAX,
            }) <= MAX_DISPATCH_RECORD_BYTES
        );
    }

    #[test]
    fn byte_reserve_preserves_maximum_result_after_heartbeat_admission_stops() {
        let fixture = Fixture::new();
        let clock = FakeClock(Cell::new(1_000));
        let mut journal = fixture.configured(2, &clock);
        let packet = launch(&mut journal, "task-000", &clock);
        let result = maximum_result(&packet);
        result.result.validate().unwrap();
        let expected = result.clone();
        let before = journal.view.clone();
        // Model cumulative prior storage independently of event count, so the
        // byte guard (not the event guard) determines admission at this edge.
        journal.bytes = MAX_JOURNAL_BYTES - reservations(&journal.view).1;
        assert_eq!(
            journal
                .heartbeat(journal.view.revision.unwrap(), &packet.lease, &clock)
                .unwrap_err()
                .code,
            "task_journal_capacity"
        );
        assert_eq!(journal.view, before);
        let status = journal.scheduler_status(&clock).unwrap();
        assert!(status.journal.unwrap().draining);
        assert!(
            status.blockers["task-001"]
                .iter()
                .any(|b| b.code == "journal_capacity")
        );
        journal
            .complete_attempt(journal.view.revision.unwrap(), result, &clock)
            .unwrap();
        let bytes = fs::read(journal.event_path(journal.events - 1)).unwrap();
        assert!(
            bytes.len() > 90_000,
            "test must exercise a large escaped result"
        );
        assert!(bytes.len() <= MAX_EVENT_BYTES);
        let record: Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.sha256, checksum(&record.body).unwrap());
        assert_eq!(
            journal.view.tasks["task-000"].result.as_ref(),
            Some(&expected.result)
        );
        assert_eq!(reservations(&journal.view), (0, 0));
        assert!(journal.bytes <= MAX_JOURNAL_BYTES);
    }

    #[test]
    fn byte_reserve_funds_separate_recoveries_and_maximum_stop_reason() {
        let fixture = Fixture::new();
        let clock = FakeClock(Cell::new(1_000));
        let mut journal = fixture.configured(2, &clock);
        let a = launch(&mut journal, "task-000", &clock);
        clock.0.set(1_001);
        let b = launch(&mut journal, "task-001", &clock);
        journal.bytes = MAX_JOURNAL_BYTES - reservations(&journal.view).1;
        clock.0.set(1_100);
        journal
            .recover_expired(journal.view.revision.unwrap(), &clock)
            .unwrap();
        assert_eq!(reservations(&journal.view).0, 3);
        journal
            .confirm_stopped(
                journal.view.revision.unwrap(),
                &a.lease,
                &"\\".repeat(MAX_REPORT_BYTES),
                &clock,
            )
            .unwrap();
        clock.0.set(1_101);
        journal
            .recover_expired(journal.view.revision.unwrap(), &clock)
            .unwrap();
        journal
            .confirm_stopped(journal.view.revision.unwrap(), &b.lease, "exited", &clock)
            .unwrap();
        assert_eq!(reservations(&journal.view), (0, 0));
        assert!(journal.bytes <= MAX_JOURNAL_BYTES);
    }

    #[test]
    fn event_reserve_permits_large_completion_at_the_admission_boundary() {
        let fixture = Fixture::new();
        let clock = FakeClock(Cell::new(1_000));
        let mut journal = fixture.configured(1, &clock);
        let worker = launch(&mut journal, "task-000", &clock);
        journal.events = MAX_EVENTS - 2;
        assert_eq!(
            journal
                .heartbeat(journal.view.revision.unwrap(), &worker.lease, &clock)
                .unwrap_err()
                .code,
            "task_journal_capacity"
        );
        journal
            .complete_attempt(
                journal.view.revision.unwrap(),
                maximum_result(&worker),
                &clock,
            )
            .unwrap();
        assert_eq!(journal.events, MAX_EVENTS - 1);
        assert_eq!(journal.view.tasks["task-000"].status, TaskStatus::Succeeded);
        assert_eq!(reservations(&journal.view), (0, 0));
    }

    #[test]
    fn retired_and_cancelled_workers_keep_terminal_reserves() {
        let mut fixture = Fixture::new();
        let clock = FakeClock(Cell::new(1_000));
        let mut journal = fixture.configured(2, &clock);
        let a = launch(&mut journal, "task-000", &clock);
        let b = launch(&mut journal, "task-001", &clock);
        journal.events = MAX_EVENTS - 4;
        journal
            .cancel_task(
                journal.view.revision.unwrap(),
                "task-000",
                a.packet.task.revision,
                "cancel",
                &clock,
            )
            .unwrap();
        assert_eq!(
            reservations(&journal.view),
            (3, 2 * MAX_EVENT_BYTES + MAX_RECOVER_RECORD_BYTES)
        );
        // Stop remains possible even when the parent story is blocked/stale.
        journal.context.blocked = true;
        journal
            .confirm_stopped(journal.view.revision.unwrap(), &a.lease, "exited", &clock)
            .unwrap();
        journal.context.blocked = false;
        fixture.context.story_revision += 1;
        journal.context = fixture.context.clone();
        journal
            .replace(journal.view.revision.unwrap(), fixture.graph(2))
            .unwrap();
        assert_eq!(reservations(&journal.view), (1, MAX_EVENT_BYTES));
        journal.context.stage = TaskStage::Done;
        journal
            .confirm_stopped(
                journal.view.revision.unwrap(),
                &b.lease,
                "retired worker exited",
                &clock,
            )
            .unwrap();
        assert_eq!(journal.events, MAX_EVENTS);
        assert_eq!(reservations(&journal.view), (0, 0));
    }

    // Generate a genuine pre-fix history using the old publication contract:
    // apply validated events, encode the identical record and retain its chain.
    // This helper deliberately omits only the new reserve admission check.
    fn append_before_headroom(journal: &mut TaskJournal, event: TaskEvent) {
        let mut next = journal.view.clone();
        apply(&mut next, &journal.context, &event, journal.graph_revision).unwrap();
        let (record, bytes) = encode_record(Body {
            schema: SCHEMA,
            revision: journal.events as u64,
            previous_sha256: journal.last.clone(),
            event,
        })
        .unwrap();
        assert!(journal.events < MAX_EVENTS && journal.bytes + bytes.len() <= MAX_JOURNAL_BYTES);
        fs::write(journal.event_path(journal.events), &bytes).unwrap();
        next.revision = Some(record.body.revision);
        journal.view = next;
        journal.last = Some(record.sha256);
        journal.events += 1;
        journal.bytes += bytes.len();
    }

    #[test]
    fn old_underfunded_histories_replay_and_allow_only_obligation_reducing_cleanup() {
        let fixture = Fixture::new();
        let clock = FakeClock(Cell::new(1_000));
        let mut journal = fixture.configured(101, &clock);
        for i in 0..100 {
            append_before_headroom(
                &mut journal,
                TaskEvent::Scheduler {
                    event: SchedulerEvent::Dispatch {
                        task: format!("task-{i:03}"),
                        task_revision: 0,
                        owner: "old-host".into(),
                        at_ms: 1_000,
                    },
                },
            );
        }
        let old = journal.view.clone();
        let first = fs::read(journal.event_path(0)).unwrap();
        drop(journal);
        let mut journal = TaskJournal::open(&fixture.root, fixture.context.clone()).unwrap();
        assert_eq!(journal.view, old);
        let leases: Vec<_> = journal
            .view
            .scheduler
            .as_ref()
            .unwrap()
            .attempts
            .values()
            .map(|a| a.lease.clone())
            .collect();
        assert!(
            journal
                .scheduler_status(&clock)
                .unwrap()
                .journal
                .unwrap()
                .draining
        );
        assert_eq!(
            journal
                .heartbeat(journal.view.revision.unwrap(), &leases[0], &clock)
                .unwrap_err()
                .code,
            "task_journal_capacity"
        );
        assert_eq!(
            journal
                .cancel_task(
                    journal.view.revision.unwrap(),
                    "task-000",
                    1,
                    "cancel",
                    &clock
                )
                .unwrap_err()
                .code,
            "task_journal_capacity"
        );
        assert_eq!(
            journal
                .dispatch(
                    journal.view.revision.unwrap(),
                    "task-100",
                    0,
                    "new-host",
                    &clock
                )
                .unwrap_err()
                .code,
            "task_journal_capacity"
        );
        assert_eq!(journal.view, old);
        for lease in leases.iter().take(38) {
            journal
                .confirm_stopped(
                    journal.view.revision.unwrap(),
                    lease,
                    "old worker exited",
                    &clock,
                )
                .unwrap();
        }
        let status = journal.scheduler_status(&clock).unwrap();
        assert_eq!(status.active_attempts, 62);
        assert!(!status.journal.unwrap().draining);
        let after = journal.view.clone();
        drop(journal);
        let journal = TaskJournal::open(&fixture.root, fixture.context.clone()).unwrap();
        assert_eq!(journal.view, after);
        assert_eq!(fs::read(journal.event_path(0)).unwrap(), first);
    }
}
