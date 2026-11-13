use super::scheduler::{self, SchedulerEvent, SchedulerState};
use crate::config::{Origin, Scope, validate_relative_path};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use unicode_normalization::UnicodeNormalization;

pub const SCHEMA: u32 = 1;
pub const MAX_TASKS: usize = 256;
pub const MAX_REPORT_BYTES: usize = 4096;
pub type TaskResult<T> = Result<T, TaskError>;

#[derive(Debug)]
pub struct TaskError {
    pub code: &'static str,
    pub detail: String,
}
impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for TaskError {}
impl From<std::io::Error> for TaskError {
    fn from(_: std::io::Error) -> Self {
        error("task_io", "task storage operation failed")
    }
}
impl From<serde_json::Error> for TaskError {
    fn from(_: serde_json::Error) -> Self {
        error("invalid_task_json", "invalid or unsupported task document")
    }
}
pub(crate) fn error(code: &'static str, detail: impl Into<String>) -> TaskError {
    TaskError {
        code,
        detail: detail.into(),
    }
}
pub(crate) fn require(ok: bool, code: &'static str, detail: &str) -> TaskResult<()> {
    if ok { Ok(()) } else { Err(error(code, detail)) }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStage {
    Plan,
    Implement,
    Gate,
    Review,
    Done,
}

/// Hosts must obtain these inputs while holding the verified story's run lock.
/// The companion CLI performs that validation; constructing a value grants no authority.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    pub story: String,
    pub base: String,
    pub story_revision: u64,
    pub stage: TaskStage,
    pub blocked: bool,
    pub scope: Vec<String>,
    pub protected: Vec<String>,
}
impl RunContext {
    pub fn validate(&self) -> TaskResult<()> {
        valid_id(&self.story)?;
        require(
            commit_id(&self.base),
            "invalid_task_context",
            "invalid pinned base",
        )?;
        paths(&self.scope)?;
        require(
            self.protected.len() <= 1024,
            "invalid_task_context",
            "too many protected paths",
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
}
impl Artifact {
    fn validate(&self) -> TaskResult<()> {
        path(&self.path)?;
        require(
            digest(&self.sha256),
            "invalid_task_artifact",
            "artifact requires lowercase SHA-256",
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    pub story: String,
    pub prerequisites: Vec<String>,
    pub stage: TaskStage,
    pub read_scope: Vec<String>,
    pub write_scope: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub decision_owner: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskGraph {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub story_revision: u64,
    pub stage: TaskStage,
    pub tasks: Vec<TaskSpec>,
}
impl TaskGraph {
    pub fn validate(&self, context: &RunContext) -> TaskResult<()> {
        context.validate()?;
        require(
            self.schema == SCHEMA,
            "task_schema",
            "unsupported task graph schema",
        )?;
        require(
            self.story == context.story && self.base == context.base,
            "task_identity",
            "graph differs from pinned story/base",
        )?;
        require(
            !self.tasks.is_empty() && self.tasks.len() <= MAX_TASKS,
            "invalid_task_graph",
            "graph must contain 1..256 tasks",
        )?;
        let mut ids = BTreeSet::new();
        for task in &self.tasks {
            valid_id(&task.id)?;
            require(
                ids.insert(task.id.clone()),
                "duplicate_task",
                "duplicate task ID",
            )?;
            require(
                task.story == self.story && task.stage == self.stage,
                "task_identity",
                "task parent/stage differs from graph",
            )?;
            require(
                task.prerequisites.len() <= MAX_TASKS,
                "invalid_task_graph",
                "too many dependencies",
            )?;
            let mut deps = BTreeSet::new();
            for dep in &task.prerequisites {
                valid_id(dep)?;
                require(
                    dep != &task.id && deps.insert(dep),
                    "invalid_task_dependency",
                    "self or duplicate dependency",
                )?;
            }
            paths(&task.read_scope)?;
            paths(&task.write_scope)?;
            for write in &task.write_scope {
                require(
                    context.scope.iter().any(|p| within(write, p)),
                    "task_scope",
                    "write scope exceeds pinned story scope",
                )?;
                let protected = context
                    .protected
                    .iter()
                    .map(String::as_str)
                    .chain(crate::config::BUILTIN_PROTECTED.iter().copied());
                require(
                    !protected
                        .into_iter()
                        .any(|p| within(&fold(write), &fold(p))),
                    "task_protected",
                    "write scope includes protected path",
                )?;
            }
            require(
                !task.acceptance_criteria.is_empty()
                    && task.acceptance_criteria.len() <= 64
                    && task
                        .acceptance_criteria
                        .iter()
                        .all(|s| !s.trim().is_empty() && s.len() <= 512),
                "invalid_task_graph",
                "task requires bounded acceptance criteria",
            )?;
            require(
                !task.decision_owner.trim().is_empty() && task.decision_owner.len() <= 128,
                "invalid_task_graph",
                "task requires bounded decision owner",
            )?;
        }
        for task in &self.tasks {
            require(
                task.prerequisites.iter().all(|d| ids.contains(d)),
                "unknown_task_dependency",
                "dependency does not exist",
            )?;
        }
        let mut visited = BTreeSet::new();
        loop {
            let before = visited.len();
            for task in &self.tasks {
                if task.prerequisites.iter().all(|d| visited.contains(d)) {
                    visited.insert(task.id.clone());
                }
            }
            if before == visited.len() {
                break;
            }
        }
        require(
            visited.len() == self.tasks.len(),
            "task_cycle",
            "dependencies contain a cycle",
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Succeeded,
    Failed,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerResult {
    pub schema: u32,
    pub story: String,
    pub task: String,
    /// Revision from the running task packet, not the ready packet.
    pub task_revision: u64,
    pub status: ResultStatus,
    pub report: String,
    pub commit: Option<String>,
    pub artifacts: Vec<Artifact>,
}
impl WorkerResult {
    pub(crate) fn validate(&self) -> TaskResult<()> {
        require(
            self.schema == SCHEMA,
            "task_schema",
            "unsupported result schema",
        )?;
        valid_id(&self.story)?;
        valid_id(&self.task)?;
        require(
            !self.report.trim().is_empty() && self.report.len() <= MAX_REPORT_BYTES,
            "invalid_task_result",
            "result requires nonblank report of at most 4096 bytes",
        )?;
        require(
            self.commit.as_ref().is_none_or(|c| commit_id(c)),
            "invalid_task_result",
            "invalid result commit ID",
        )?;
        require(
            self.artifacts.len() <= 64,
            "invalid_task_result",
            "too many artifacts",
        )?;
        let mut paths = BTreeSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            require(
                paths.insert(&artifact.path),
                "invalid_task_artifact",
                "duplicate artifact path",
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskState {
    pub spec: TaskSpec,
    pub revision: u64,
    pub status: TaskStatus,
    pub result: Option<WorkerResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskPacket {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub story_revision: u64,
    pub ledger_revision: u64,
    pub task: TaskState,
    pub prerequisites: BTreeMap<String, WorkerResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskView {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub story_revision: u64,
    pub stage: TaskStage,
    pub revision: Option<u64>,
    pub tasks: BTreeMap<String, TaskState>,
    pub ready: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<SchedulerState>,
}
impl TaskView {
    pub(crate) fn empty(context: &RunContext) -> Self {
        Self {
            schema: SCHEMA,
            story: context.story.clone(),
            base: context.base.clone(),
            story_revision: context.story_revision,
            stage: context.stage,
            revision: None,
            tasks: BTreeMap::new(),
            ready: Vec::new(),
            scheduler: None,
        }
    }
    pub(crate) fn refresh(&mut self) {
        let succeeded: BTreeSet<_> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Succeeded)
            .map(|t| t.spec.id.clone())
            .collect();
        for task in self.tasks.values_mut() {
            if matches!(task.status, TaskStatus::Pending | TaskStatus::Ready) {
                task.status = if task
                    .spec
                    .prerequisites
                    .iter()
                    .all(|d| succeeded.contains(d))
                {
                    TaskStatus::Ready
                } else {
                    TaskStatus::Pending
                };
            }
        }
        self.ready = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Ready)
            .map(|t| t.spec.id.clone())
            .collect();
    }
    pub(crate) fn packet(&self, id: &str) -> TaskResult<TaskPacket> {
        let task = self
            .tasks
            .get(id)
            .ok_or_else(|| error("unknown_task", "task does not exist"))?;
        require(
            matches!(task.status, TaskStatus::Ready | TaskStatus::Running),
            "task_not_ready",
            "only ready/running tasks have packets",
        )?;
        let prerequisites = task
            .spec
            .prerequisites
            .iter()
            .map(|id| {
                let dep = self
                    .tasks
                    .get(id)
                    .ok_or_else(|| error("unknown_task_dependency", "missing prerequisite"))?;
                require(
                    dep.status == TaskStatus::Succeeded,
                    "task_not_ready",
                    "prerequisite has not succeeded",
                )?;
                Ok((
                    id.clone(),
                    dep.result
                        .clone()
                        .ok_or_else(|| error("task_integrity", "missing dependency evidence"))?,
                ))
            })
            .collect::<TaskResult<_>>()?;
        Ok(TaskPacket {
            schema: SCHEMA,
            story: self.story.clone(),
            base: self.base.clone(),
            story_revision: self.story_revision,
            ledger_revision: self
                .revision
                .ok_or_else(|| error("task_uninitialized", "graph not initialized"))?,
            task: task.clone(),
            prerequisites,
        })
    }
    pub fn markdown(&self) -> String {
        let mut out = format!(
            "# Tasks for {}\n\nBase: `{}`; task revision: {:?}; story revision: {}.\n\n| Task | Stage | Revision | Status | Dependencies |\n| --- | --- | --- | --- | --- |\n",
            self.story, self.base, self.revision, self.story_revision
        );
        for task in self.tasks.values() {
            out.push_str(&format!(
                "| {} | {:?} | {} | {:?} | {} |\n",
                task.spec.id,
                task.spec.stage,
                task.revision,
                task.status,
                task.spec.prerequisites.join(", ")
            ));
        }
        out
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskEvent {
    Define { graph: TaskGraph },
    Replace { graph: TaskGraph },
    Start { task: String, task_revision: u64 },
    Complete { result: WorkerResult },
    Scheduler { event: SchedulerEvent },
}

pub(crate) fn apply(
    view: &mut TaskView,
    context: &RunContext,
    event: &TaskEvent,
    graph_revision: u64,
) -> TaskResult<()> {
    match event {
        TaskEvent::Define { graph } | TaskEvent::Replace { graph } => {
            let task_revision = if matches!(event, TaskEvent::Define { .. }) {
                require(
                    view.revision.is_none(),
                    "task_already_initialized",
                    "graph is immutable within a story revision",
                )?;
                0
            } else {
                let revision = view.revision.ok_or_else(|| {
                    error("task_uninitialized", "initialize the task graph first")
                })?;
                require(
                    graph.story_revision > view.story_revision,
                    "stale_story_context",
                    "replacement graph requires a newer story revision",
                )?;
                // Every earlier task revision is at most the previous ledger
                // revision. Seeding from the replacement event prevents an old
                // result matching a reused task ID even with a fresh store REV.
                revision + 1
            };
            graph.validate(context)?;
            require(
                graph.story_revision <= context.story_revision && graph.stage != TaskStage::Done,
                "stale_story_context",
                "graph cannot bind a future revision or completed stage",
            )?;
            view.story_revision = graph.story_revision;
            view.stage = graph.stage;
            scheduler::retire_graph(view, graph_revision);
            view.tasks.clear();
            for spec in &graph.tasks {
                view.tasks.insert(
                    spec.id.clone(),
                    TaskState {
                        spec: spec.clone(),
                        revision: task_revision,
                        status: TaskStatus::Pending,
                        result: None,
                    },
                );
            }
        }
        TaskEvent::Start {
            task,
            task_revision,
        } => {
            require(
                view.scheduler.is_none(),
                "scheduler_managed",
                "use dispatch for scheduler-managed tasks",
            )?;
            let state = view
                .tasks
                .get_mut(task)
                .ok_or_else(|| error("unknown_task", "task does not exist"))?;
            require(
                state.revision == *task_revision,
                "stale_task_revision",
                "task revision changed",
            )?;
            require(
                state.status == TaskStatus::Ready,
                "task_not_ready",
                "only ready tasks can start",
            )?;
            state.status = TaskStatus::Running;
            state.revision += 1;
        }
        TaskEvent::Complete { result } => {
            require(
                view.scheduler.is_none(),
                "scheduler_managed",
                "use complete_attempt for scheduler-managed tasks",
            )?;
            result.validate()?;
            require(
                result.story == view.story,
                "task_identity",
                "result has different parent story",
            )?;
            let state = view
                .tasks
                .get_mut(&result.task)
                .ok_or_else(|| error("unknown_task", "task does not exist"))?;
            require(
                state.revision == result.task_revision,
                "stale_task_revision",
                "task revision changed",
            )?;
            require(
                state.status == TaskStatus::Running,
                "invalid_task_transition",
                "only running tasks can complete",
            )?;
            state.status = match result.status {
                ResultStatus::Succeeded => TaskStatus::Succeeded,
                ResultStatus::Failed => TaskStatus::Failed,
                ResultStatus::Blocked => TaskStatus::Blocked,
            };
            state.revision += 1;
            state.result = Some(result.clone());
        }
        TaskEvent::Scheduler { event } => scheduler::apply(view, graph_revision, event)?,
    }
    view.refresh();
    Ok(())
}

pub(crate) fn valid_id(id: &str) -> TaskResult<()> {
    require(
        !id.is_empty()
            && id.len() <= 40
            && id.as_bytes()[0].is_ascii_lowercase()
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !id.ends_with('-'),
        "invalid_task_id",
        "IDs require lowercase ASCII letters, digits and internal hyphens",
    )?;
    path(id)
}
fn paths(values: &[String]) -> TaskResult<()> {
    require(
        values.len() <= 256,
        "invalid_task_scope",
        "too many scope paths",
    )?;
    let mut seen = BTreeSet::new();
    for value in values {
        path(value)?;
        require(
            seen.insert(fold(value)),
            "invalid_task_scope",
            "duplicate or aliased scope path",
        )?;
    }
    Ok(())
}
fn path(value: &str) -> TaskResult<()> {
    require(value.len() <= 1024, "invalid_task_scope", "path too long")?;
    validate_relative_path(value, &Origin::new(Scope::Repo, "task", "path"))
        .map_err(|_| error("invalid_task_scope", "unsafe relative task path"))
}
pub(crate) fn within(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('/'))
}
pub(crate) fn fold(s: &str) -> String {
    s.nfc().flat_map(char::to_lowercase).collect()
}
pub(crate) fn commit_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub(crate) fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
