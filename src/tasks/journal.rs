use super::model::*;
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
            apply(&mut journal.view, &journal.context, &record.body.event)?;
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
        require(
            !self.context.blocked
                && graph.story_revision == self.context.story_revision
                && graph.stage == self.context.stage,
            "stale_story_context",
            "graph must bind the current unblocked story revision/stage",
        )?;
        self.append(TaskEvent::Define { graph })
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

    fn require_live(&self) -> TaskResult<()> {
        require(
            !self.context.blocked
                && self.view.story_revision == self.context.story_revision
                && self.view.stage == self.context.stage,
            "stale_story_context",
            "story changed or is blocked; old task results cannot mutate or dispatch",
        )
    }
    fn expect_revision(&self, revision: u64) -> TaskResult<()> {
        self.require_live()?;
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
        apply(&mut next, &self.context, &event)?;
        let body = Body {
            schema: SCHEMA,
            revision: self.events as u64,
            previous_sha256: self.last.clone(),
            event,
        };
        let record = Record {
            sha256: checksum(&body)?,
            body,
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        require(
            bytes.len() <= MAX_EVENT_BYTES && self.bytes + bytes.len() <= MAX_JOURNAL_BYTES,
            "task_journal_limit",
            "task event or journal exceeds size limit",
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
        fs::rename(pending, path)?;
        next.revision = Some(record.body.revision);
        self.view = next;
        self.last = Some(record.sha256);
        self.events += 1;
        self.bytes += bytes.len();
        Ok(())
    }
    fn event_path(&self, revision: usize) -> PathBuf {
        self.dir.join("events").join(format!("{revision:012}.json"))
    }
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
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => regular_dir(path),
        Err(e) => Err(e.into()),
    }
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
