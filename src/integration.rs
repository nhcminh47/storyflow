//! Deterministic integration coordinator for completed parallel code tasks.
//!
//! Validates per-task ancestry, base commit, write scope, portable namespace,
//! and explicit worker exit before integrating task commits in dependency order
//! into one pinned story candidate. Conflicts, scope violations, and stale bases
//! block with auditable evidence without automatic conflict resolution.
use crate::{
    config::{BUILTIN_PROTECTED, Origin, Scope, validate_relative_path},
    security::process::{self, CommandSpec},
    tasks::{
        AttemptPhase, ResultStatus, RunContext, SCHEMA, TaskGraph, TaskStage, TaskStatus, TaskView,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use unicode_normalization::UnicodeNormalization;

pub const MAX_INTEGRATION_STATE_BYTES: usize = 1_048_576;
const GIT_TIMEOUT_SECS: u64 = 60;

pub type IntegrationResult<T> = Result<T, IntegrationError>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntegrationError {
    pub code: &'static str,
    pub detail: String,
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for IntegrationError {}

impl From<std::io::Error> for IntegrationError {
    fn from(_: std::io::Error) -> Self {
        error("integration_io", "integration storage operation failed")
    }
}

impl From<serde_json::Error> for IntegrationError {
    fn from(_: serde_json::Error) -> Self {
        error("invalid_integration_json", "invalid integration document")
    }
}

pub fn error(code: &'static str, detail: impl Into<String>) -> IntegrationError {
    IntegrationError {
        code,
        detail: detail.into(),
    }
}

fn require(ok: bool, code: &'static str, detail: impl Into<String>) -> IntegrationResult<()> {
    if ok { Ok(()) } else { Err(error(code, detail)) }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationPhase {
    Pending,
    Ready,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntegrationBlocker {
    pub code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicting_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntegratedInput {
    pub task: String,
    pub task_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    pub base_commit: String,
    pub input_commit: String,
    pub input_ref: String,
    pub integrated_commit: String,
    pub changed_paths: Vec<String>,
    pub reused: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerWorktree {
    pub schema: u32,
    pub story: String,
    pub story_revision: u64,
    pub graph_revision: u64,
    pub task: String,
    pub task_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    pub base_commit: String,
    pub branch_ref: String,
    pub path: String,
    pub write_scope: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntegrationStatus {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub story_revision: u64,
    pub graph_revision: u64,
    pub ledger_revision: u64,
    pub order: Vec<String>,
    pub phase: IntegrationPhase,
    pub candidate_commit: Option<String>,
    pub candidate_ref: Option<String>,
    pub integrated: Vec<IntegratedInput>,
    pub blockers: Vec<IntegrationBlocker>,
    pub worktrees: BTreeMap<String, WorkerWorktree>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredState {
    body: IntegrationStatus,
    sha256: String,
}

/// Deterministic coordinator for worker worktree isolation and candidate integration.
/// Callers must hold the verified story run lock and task journal lock across operations.
pub struct IntegrationCoordinator {
    root: PathBuf,
    run_dir: PathBuf,
    worktree_hash: String,
    context: RunContext,
    graph_revision_override: Option<u64>,
}

impl IntegrationCoordinator {
    pub fn new(root: &Path, context: RunContext) -> IntegrationResult<Self> {
        context.validate().map_err(|e| error(e.code, e.detail))?;
        if !root.is_absolute() {
            return Err(error(
                "invalid_repository",
                "repository root must be absolute",
            ));
        }
        let canonical = fs::canonicalize(root)
            .map_err(|_| error("invalid_repository", "repository root is unavailable"))?;
        let run_dir = canonical.join(".storyflow").join(&context.story);
        require(
            run_dir.join("events").is_dir(),
            "task_run_missing",
            "initialize the parent story run first",
        )?;
        regular_dir(&canonical.join(".storyflow"))?;
        regular_dir(&run_dir)?;
        let worktree_hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        Ok(Self {
            root: canonical,
            run_dir,
            worktree_hash,
            context,
            graph_revision_override: None,
        })
    }

    pub fn with_graph_revision(mut self, graph_revision: u64) -> Self {
        self.graph_revision_override = Some(graph_revision);
        self
    }

    fn graph_revision_for(&self, view: &TaskView) -> u64 {
        self.graph_revision_override
            .or_else(|| view.scheduler.as_ref().map(|s| s.graph_revision))
            .unwrap_or(0)
    }

    fn integration_dir(&self) -> PathBuf {
        self.run_dir.join("integration")
    }

    fn state_path(&self) -> PathBuf {
        self.integration_dir().join("state.json")
    }

    fn worktrees_dir(&self) -> PathBuf {
        self.run_dir.join("worktrees")
    }

    /// Load persisted integration state if present, verifying its checksum and story identity.
    pub fn load_state(&self) -> IntegrationResult<Option<IntegrationStatus>> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(None);
        }
        safe_existing_file(&path)?;
        let mut bytes = Vec::new();
        File::open(&path)?
            .take(MAX_INTEGRATION_STATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        require(
            bytes.len() <= MAX_INTEGRATION_STATE_BYTES,
            "integration_state_limit",
            "integration state exceeds 1 MiB",
        )?;
        let stored: StoredState = serde_json::from_slice(&bytes)
            .map_err(|_| error("integration_integrity", "malformed integration state"))?;
        let expected = checksum(&stored.body)?;
        require(
            stored.body.schema == SCHEMA
                && stored.sha256 == expected
                && stored.body.story == self.context.story
                && stored.body.base == self.context.base,
            "integration_integrity",
            "integration state checksum or identity mismatch",
        )?;
        Ok(Some(stored.body))
    }

    /// Return current status for the active task view without mutating Git refs or state.
    /// Preserves any uncleaned worktrees from earlier story or graph revisions so they remain
    /// reachable for explicit `cleanup_worktree` (`worktree-remove`) calls.
    pub fn status(
        &self,
        view: &TaskView,
        graph: Option<&TaskGraph>,
    ) -> IntegrationResult<IntegrationStatus> {
        let graph_rev = self.graph_revision_for(view);
        let ledger_rev = view.revision.unwrap_or(0);
        let order = dependency_order(view, graph)?;
        match self.load_state()? {
            Some(mut saved)
                if saved.story_revision == view.story_revision
                    && saved.graph_revision == graph_rev =>
            {
                saved.order = order;
                saved.ledger_revision = ledger_rev;
                Ok(saved)
            }
            Some(saved) => Ok(IntegrationStatus {
                schema: SCHEMA,
                story: self.context.story.clone(),
                base: self.context.base.clone(),
                story_revision: view.story_revision,
                graph_revision: graph_rev,
                ledger_revision: ledger_rev,
                order,
                phase: IntegrationPhase::Pending,
                candidate_commit: None,
                candidate_ref: None,
                integrated: Vec::new(),
                blockers: Vec::new(),
                worktrees: saved.worktrees,
            }),
            None => Ok(IntegrationStatus {
                schema: SCHEMA,
                story: self.context.story.clone(),
                base: self.context.base.clone(),
                story_revision: view.story_revision,
                graph_revision: graph_rev,
                ledger_revision: ledger_rev,
                order,
                phase: IntegrationPhase::Pending,
                candidate_commit: None,
                candidate_ref: None,
                integrated: Vec::new(),
                blockers: Vec::new(),
                worktrees: BTreeMap::new(),
            }),
        }
    }

    /// Provision an isolated Git worktree for a running task at its expected base commit.
    /// Rejects non-running tasks, unconfirmed prior workers, or overlapping active worktrees.
    pub fn prepare_worktree(
        &self,
        view: &TaskView,
        graph: Option<&TaskGraph>,
        task_id: &str,
    ) -> IntegrationResult<WorkerWorktree> {
        self.require_live(view)?;
        let ledger_rev = view
            .revision
            .ok_or_else(|| error("task_uninitialized", "initialize the task graph first"))?;
        let graph_rev = self.graph_revision_for(view);
        let task = view
            .tasks
            .get(task_id)
            .ok_or_else(|| error("unknown_task", "task does not exist"))?;
        require(
            task.status == TaskStatus::Running,
            "task_not_running",
            "only running tasks can provision an isolated worker worktree",
        )?;
        let attempt_id = active_attempt_for_running_task(view, task_id, task.revision)?;
        let base_commit = self.expected_task_base(view, graph, task_id)?;

        let mut current = self.status(view, graph)?;
        // Check for conflicting active worktrees with overlapping write scopes.
        for (other_id, existing) in &current.worktrees {
            if other_id == task_id {
                continue;
            }
            let other_active = view
                .tasks
                .get(other_id)
                .is_some_and(|t| t.status == TaskStatus::Running)
                || has_unstopped_worker(view, other_id);
            if other_active && scopes_overlap(&task.spec.write_scope, &existing.write_scope) {
                return Err(error(
                    "write_scope_occupied",
                    format!("task {other_id} owns an active worktree with overlapping write scope"),
                ));
            }
        }

        let wt_dir = self
            .worktrees_dir()
            .join(format!("g{graph_rev}-{task_id}-r{}", task.revision));
        let branch_ref = format!(
            "refs/storyflow/{}/{}/workers/g{}/{}/r{}",
            self.worktree_hash, self.context.story, graph_rev, task_id, task.revision
        );
        let relative_path = format!(
            ".storyflow/{}/worktrees/g{graph_rev}-{task_id}-r{}",
            self.context.story, task.revision
        );

        // Idempotent reuse if the same task revision worktree already exists at the expected base.
        if let Some(existing) = current.worktrees.get(task_id)
            && existing.graph_revision == graph_rev
            && existing.task_revision == task.revision
            && existing.attempt_id == attempt_id
            && existing.base_commit == base_commit
            && wt_dir.is_dir()
        {
            return Ok(existing.clone());
        }

        // Reject replacing/removing any previous worktree for this task while an earlier
        // attempt is unstopped, and keep its cleanup record intact if removal fails.
        if let Some(old) = current.worktrees.get(task_id).cloned() {
            if let Some(old_attempt) = old.attempt_id.as_deref()
                && is_attempt_unstopped(view, old_attempt)
            {
                return Err(error(
                    "worker_stop_required",
                    "prior worker for this task has not been confirmed stopped; keep its worktree until stop is confirmed",
                ));
            }
            if self.root.join(&old.path).exists() {
                if let Some(raw_commit) = task.result.as_ref().and_then(|r| r.commit.as_ref())
                    && let Ok(commit) = self.resolve_commit(raw_commit)
                {
                    let input_ref =
                        self.task_input_ref(old.graph_revision, task_id, old.task_revision);
                    self.git_checked(&["update-ref", &input_ref, &commit])?;
                }
                self.remove_git_worktree(&old.path)?;
            }
            current.worktrees.remove(task_id);
        }
        if wt_dir.exists() {
            self.remove_git_worktree(&relative_path)?;
        }

        create_dir(&self.worktrees_dir())?;
        self.git_checked(&["update-ref", &branch_ref, &base_commit])?;
        self.git_supervised(&[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &relative_path,
            &base_commit,
        ])?;

        let record = WorkerWorktree {
            schema: SCHEMA,
            story: self.context.story.clone(),
            story_revision: view.story_revision,
            graph_revision: graph_rev,
            task: task_id.to_owned(),
            task_revision: task.revision,
            attempt_id,
            base_commit,
            branch_ref,
            path: relative_path,
            write_scope: task.spec.write_scope.clone(),
        };
        current.worktrees.insert(task_id.to_owned(), record.clone());
        current.ledger_revision = ledger_rev;
        self.save_state(&current)?;
        Ok(record)
    }

    /// Remove a provisioned worker worktree after confirming its worker has exited.
    /// Allows cleaning up worktrees from either the current graph or an earlier retired
    /// story/graph revision as long as no unstopped worker owns that task.
    /// If the task in the current graph reported a result commit, pins the task input ref
    /// before removing the detached worktree so Git pruning cannot lose the commit before `integrate`.
    pub fn cleanup_worktree(
        &self,
        view: &TaskView,
        graph: Option<&TaskGraph>,
        task_id: &str,
    ) -> IntegrationResult<IntegrationStatus> {
        let mut current = self.status(view, graph)?;
        let current_task = view.tasks.get(task_id);
        let has_recorded_worktree = current.worktrees.contains_key(task_id);
        require(
            current_task.is_some() || has_recorded_worktree,
            "unknown_task",
            "task does not exist and has no recorded worktree",
        )?;
        require(
            !has_unstopped_worker(view, task_id)
                && current_task.is_none_or(|t| t.status != TaskStatus::Running),
            "worker_stop_required",
            "confirm the worker has exited before removing its worktree",
        )?;
        let graph_rev = self.graph_revision_for(view);
        if let Some(task) = current_task
            && let Some(raw_commit) = task.result.as_ref().and_then(|r| r.commit.as_ref())
        {
            let commit = self.resolve_commit(raw_commit).map_err(|_| {
                error(
                    "invalid_task_commit",
                    format!("task {task_id} commit {raw_commit} does not exist in Git"),
                )
            })?;
            let input_ref = self.task_input_ref(graph_rev, task_id, task.revision);
            self.git_checked(&["update-ref", &input_ref, &commit])?;
        }
        if let Some(removed) = current.worktrees.remove(task_id) {
            if self.root.join(&removed.path).exists() {
                self.remove_git_worktree(&removed.path)?;
            }
            self.save_state(&current)?;
        }
        Ok(current)
    }

    fn task_input_ref(&self, graph_rev: u64, task_id: &str, task_revision: u64) -> String {
        format!(
            "refs/storyflow/{}/{}/tasks/g{}/{}/r{}",
            self.worktree_hash, self.context.story, graph_rev, task_id, task_revision
        )
    }

    /// Validate all tasks in dependency order and integrate their commits into one pinned candidate.
    ///
    /// - Requires all tasks with non-empty `write_scope` (and any task returning a commit) to have
    ///   succeeded and every worker attempt across the journal to have exited (`worker_stopped == true`).
    /// - Validates each task commit's existence, ancestry from its required base commit, non-empty
    ///   diff within its declared `write_scope`, story scope, protected paths, and portable namespace.
    /// - Detects overlapping changed paths or merge conflicts across parallel tasks, blocking with
    ///   exact file paths without automatic conflict resolution.
    /// - Reuses already integrated prefixes idempotently on recovery or re-execution.
    /// - Pins individual task input refs and one combined story candidate ref.
    pub fn integrate(
        &self,
        view: &TaskView,
        graph: Option<&TaskGraph>,
    ) -> IntegrationResult<IntegrationStatus> {
        self.require_live(view)?;
        require(
            self.context.stage == TaskStage::Implement,
            "invalid_integration_stage",
            "parallel commit integration requires the unblocked implement stage",
        )?;
        let ledger_rev = view
            .revision
            .ok_or_else(|| error("task_uninitialized", "initialize the task graph first"))?;
        let graph_rev = self.graph_revision_for(view);
        let order = dependency_order(view, graph)?;

        let loaded = self.load_state()?;
        let worktrees = loaded
            .as_ref()
            .map(|s| s.worktrees.clone())
            .unwrap_or_default();
        let previous = loaded
            .filter(|s| s.story_revision == view.story_revision && s.graph_revision == graph_rev);

        let mut blockers = Vec::new();

        // 1. Verify all worker attempts in the scheduler have exited before integrating any commit.
        if let Some(scheduler) = &view.scheduler {
            for attempt in scheduler.attempts.values().filter(|a| !a.worker_stopped) {
                blockers.push(IntegrationBlocker {
                    code: "worker_stop_required".into(),
                    detail: format!(
                        "worker attempt {} for task {} has not been confirmed stopped",
                        attempt.lease.attempt_id, attempt.task
                    ),
                    task: Some(attempt.task.clone()),
                    conflicting_task: None,
                    commit: attempt.result.as_ref().and_then(|r| r.commit.clone()),
                    paths: Vec::new(),
                });
            }
        }

        // 2. Verify all tasks in the graph have completed and check task results in dependency order.
        for task_id in &order {
            let task = &view.tasks[task_id];
            if task.status != TaskStatus::Succeeded {
                blockers.push(IntegrationBlocker {
                    code: "task_not_completed".into(),
                    detail: format!(
                        "task {task_id} has status {:?}; all tasks must succeed before integration",
                        task.status
                    ),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: None,
                    paths: Vec::new(),
                });
                continue;
            }
            let Some(result) = &task.result else {
                blockers.push(IntegrationBlocker {
                    code: "missing_task_result".into(),
                    detail: format!("task {task_id} has no recorded result"),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: None,
                    paths: Vec::new(),
                });
                continue;
            };
            if result.status != ResultStatus::Succeeded {
                blockers.push(IntegrationBlocker {
                    code: "task_not_completed".into(),
                    detail: format!("task {task_id} result is not succeeded"),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: result.commit.clone(),
                    paths: Vec::new(),
                });
                continue;
            }
            if task.spec.write_scope.is_empty() && result.commit.is_some() {
                blockers.push(IntegrationBlocker {
                    code: "read_only_task_commit".into(),
                    detail: format!(
                        "read-only task {task_id} has empty write_scope but reported a commit"
                    ),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: result.commit.clone(),
                    paths: Vec::new(),
                });
            } else if !task.spec.write_scope.is_empty() && result.commit.is_none() {
                blockers.push(IntegrationBlocker {
                    code: "missing_task_commit".into(),
                    detail: format!("code task {task_id} with write_scope must report a commit ID"),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: None,
                    paths: Vec::new(),
                });
            }
        }

        if !blockers.is_empty() {
            let status = IntegrationStatus {
                schema: SCHEMA,
                story: self.context.story.clone(),
                base: self.context.base.clone(),
                story_revision: view.story_revision,
                graph_revision: graph_rev,
                ledger_revision: ledger_rev,
                order,
                phase: IntegrationPhase::Blocked,
                candidate_commit: None,
                candidate_ref: None,
                integrated: Vec::new(),
                blockers,
                worktrees,
            };
            self.save_state(&status)?;
            return Ok(status);
        }

        // 3. Validate each code task's commit ancestry, base commit, write scope, and changed paths.
        struct ValidatedTask {
            task_id: String,
            task_revision: u64,
            attempt_id: Option<String>,
            base_commit: String,
            input_commit: String,
            input_ref: String,
            changed_paths: Vec<String>,
        }

        let mut validated = Vec::new();
        let mut path_owners: BTreeMap<String, (String, String)> = BTreeMap::new();

        for task_id in &order {
            let task = &view.tasks[task_id];
            let Some(raw_commit) = task.result.as_ref().and_then(|r| r.commit.as_ref()) else {
                continue;
            };
            let commit = match self.resolve_commit(raw_commit) {
                Ok(c) => c,
                Err(_) => {
                    blockers.push(IntegrationBlocker {
                        code: "invalid_task_commit".into(),
                        detail: format!("task {task_id} commit {raw_commit} does not exist in Git"),
                        task: Some(task_id.clone()),
                        conflicting_task: None,
                        commit: Some(raw_commit.clone()),
                        paths: Vec::new(),
                    });
                    continue;
                }
            };

            let expected_base = self.expected_task_base(view, graph, task_id)?;

            // Commit must descend from the pinned story baseline AND the task's expected base.
            if !self.is_strict_descendant(&self.context.base, &commit)?
                || !self.is_strict_descendant(&expected_base, &commit)?
            {
                blockers.push(IntegrationBlocker {
                    code: "stale_task_base".into(),
                    detail: format!(
                        "task {task_id} commit {commit} is not a descendant of required base {expected_base}"
                    ),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: Some(commit),
                    paths: Vec::new(),
                });
                continue;
            }

            // Ensure the task commit does not pull in unrelated side commits outside its base.
            // Specifically, all files changed between `expected_base` and `commit` are evaluated,
            // and if a task has prerequisites, its base must descend from all prerequisite commits,
            // while for independent tasks (`expected_base == story.base`), `merge-base(story.base, commit)`
            // must equal `story.base`, and every parent along `expected_base..commit` must be a
            // linear descendant of `expected_base` (no foreign merge parents).
            if !self.has_only_descendant_parents(&expected_base, &commit)? {
                blockers.push(IntegrationBlocker {
                    code: "stale_task_base".into(),
                    detail: format!(
                        "task {task_id} commit {commit} includes non-descendant merge parents outside base {expected_base}"
                    ),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: Some(commit),
                    paths: Vec::new(),
                });
                continue;
            }

            if let Err(err) = self.validate_candidate_namespace(&expected_base, &commit) {
                blockers.push(IntegrationBlocker {
                    code: "invalid_task_namespace".into(),
                    detail: format!("task {task_id}: {}", err.detail),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: Some(commit),
                    paths: Vec::new(),
                });
                continue;
            }

            let changed = self.changed_paths(&expected_base, &commit)?;
            if changed.is_empty() {
                blockers.push(IntegrationBlocker {
                    code: "empty_task_diff".into(),
                    detail: format!("task {task_id} commit {commit} has no tree changes from base"),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: Some(commit),
                    paths: Vec::new(),
                });
                continue;
            }

            let violation_paths = self.scope_violations(task_id, &task.spec.write_scope, &changed);
            if !violation_paths.is_empty() {
                blockers.push(IntegrationBlocker {
                    code: "task_scope_violation".into(),
                    detail: format!(
                        "task {task_id} modified {} path(s) outside its allowed write_scope or in protected paths",
                        violation_paths.len()
                    ),
                    task: Some(task_id.clone()),
                    conflicting_task: None,
                    commit: Some(commit),
                    paths: violation_paths,
                });
                continue;
            }

            // Check for overlapping changed paths with independent tasks.
            // A dependent task is allowed to touch a path from its prerequisite ONLY if that path
            // is also inside the dependent task's write_scope and the prerequisite is an ancestor of
            // this task in the DAG. Independent tasks must never modify the same or aliased path.
            let ancestors = task_ancestors(view, task_id);
            let mut overlap_by_task: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for path in &changed {
                let key = portable_identity(path);
                if let Some((prev_task, prev_path)) = path_owners.get(&key)
                    && prev_task != task_id
                    && !ancestors.contains(prev_task)
                {
                    let display = if prev_path == path {
                        path.clone()
                    } else {
                        format!("{path} (aliases {prev_path})")
                    };
                    overlap_by_task
                        .entry(prev_task.clone())
                        .or_default()
                        .push(display);
                }
            }
            if !overlap_by_task.is_empty() {
                for (other_task, mut paths) in overlap_by_task {
                    paths.sort();
                    paths.dedup();
                    blockers.push(IntegrationBlocker {
                        code: "overlapping_task_writes".into(),
                        detail: format!(
                            "parallel tasks {other_task} and {task_id} modified overlapping paths"
                        ),
                        task: Some(task_id.clone()),
                        conflicting_task: Some(other_task),
                        commit: Some(commit.clone()),
                        paths,
                    });
                }
                continue;
            }

            for path in &changed {
                path_owners.insert(portable_identity(path), (task_id.clone(), path.clone()));
            }

            let attempt_id = succeeded_attempt_for_task(view, task_id, task.revision);
            let input_ref = self.task_input_ref(graph_rev, task_id, task.revision);
            validated.push(ValidatedTask {
                task_id: task_id.clone(),
                task_revision: task.revision,
                attempt_id,
                base_commit: expected_base,
                input_commit: commit,
                input_ref,
                changed_paths: changed,
            });
        }

        if validated.is_empty() && blockers.is_empty() {
            blockers.push(IntegrationBlocker {
                code: "no_code_tasks".into(),
                detail: "graph contains no completed code tasks to integrate".into(),
                task: None,
                conflicting_task: None,
                commit: None,
                paths: Vec::new(),
            });
        }

        if !blockers.is_empty() {
            let status = IntegrationStatus {
                schema: SCHEMA,
                story: self.context.story.clone(),
                base: self.context.base.clone(),
                story_revision: view.story_revision,
                graph_revision: graph_rev,
                ledger_revision: ledger_rev,
                order,
                phase: IntegrationPhase::Blocked,
                candidate_commit: None,
                candidate_ref: None,
                integrated: Vec::new(),
                blockers,
                worktrees,
            };
            self.save_state(&status)?;
            return Ok(status);
        }

        // 4. Integrate validated task commits in dependency order, reusing matching prefixes idempotently.
        let mut integrated: Vec<IntegratedInput> = Vec::new();
        let mut current_candidate: Option<String> = None;
        let prev_integrated = previous
            .as_ref()
            .map(|p| p.integrated.as_slice())
            .unwrap_or(&[]);
        let mut can_reuse_prefix = true;

        for (index, item) in validated.iter().enumerate() {
            // Pin input ref for auditable evidence before integrating.
            self.git_checked(&["update-ref", &item.input_ref, &item.input_commit])?;

            if can_reuse_prefix
                && let Some(prev) = prev_integrated.get(index)
                && prev.task == item.task_id
                && prev.task_revision == item.task_revision
                && prev.attempt_id == item.attempt_id
                && prev.base_commit == item.base_commit
                && prev.input_commit == item.input_commit
                && prev.changed_paths == item.changed_paths
                && self.resolve_commit(&prev.integrated_commit).is_ok()
            {
                let mut reused = prev.clone();
                reused.reused = true;
                current_candidate = Some(reused.integrated_commit.clone());
                integrated.push(reused);
                continue;
            }
            can_reuse_prefix = false;

            let next_commit = match &current_candidate {
                None => item.input_commit.clone(),
                Some(running) => {
                    if self.is_ancestor(running, &item.input_commit)? {
                        // Dependent task built directly on top of the current integrated commit.
                        item.input_commit.clone()
                    } else if self.is_ancestor(&item.input_commit, running)? {
                        running.clone()
                    } else {
                        // Replay/merge the task's changes onto the running integrated commit
                        // without modifying any working tree or auto-resolving conflicts.
                        match self.merge_trees(
                            &item.base_commit,
                            running,
                            &item.input_commit,
                            &item.task_id,
                            view.story_revision,
                            graph_rev,
                        )? {
                            MergeOutcome::Clean(commit) => commit,
                            MergeOutcome::Conflict(mut conflict_paths) => {
                                conflict_paths.sort();
                                conflict_paths.dedup();
                                blockers.push(IntegrationBlocker {
                                    code: "integration_conflict".into(),
                                    detail: format!(
                                        "integrating task {} commit {} onto candidate {} produced merge conflicts",
                                        item.task_id, item.input_commit, running
                                    ),
                                    task: Some(item.task_id.clone()),
                                    conflicting_task: integrated.last().map(|i| i.task.clone()),
                                    commit: Some(item.input_commit.clone()),
                                    paths: conflict_paths,
                                });
                                let status = IntegrationStatus {
                                    schema: SCHEMA,
                                    story: self.context.story.clone(),
                                    base: self.context.base.clone(),
                                    story_revision: view.story_revision,
                                    graph_revision: graph_rev,
                                    ledger_revision: ledger_rev,
                                    order,
                                    phase: IntegrationPhase::Blocked,
                                    candidate_commit: None,
                                    candidate_ref: None,
                                    integrated,
                                    blockers,
                                    worktrees,
                                };
                                self.save_state(&status)?;
                                return Ok(status);
                            }
                        }
                    }
                }
            };

            current_candidate = Some(next_commit.clone());
            integrated.push(IntegratedInput {
                task: item.task_id.clone(),
                task_revision: item.task_revision,
                attempt_id: item.attempt_id.clone(),
                base_commit: item.base_commit.clone(),
                input_commit: item.input_commit.clone(),
                input_ref: item.input_ref.clone(),
                integrated_commit: next_commit,
                changed_paths: item.changed_paths.clone(),
                reused: false,
            });
        }

        let final_candidate = current_candidate.expect("validated is non-empty");

        // 5. Final validation of the combined candidate against story base, scope, and namespace.
        if let Err(err) = self.validate_candidate_namespace(&self.context.base, &final_candidate) {
            blockers.push(IntegrationBlocker {
                code: "invalid_candidate_namespace".into(),
                detail: err.detail,
                task: None,
                conflicting_task: None,
                commit: Some(final_candidate),
                paths: Vec::new(),
            });
            let status = IntegrationStatus {
                schema: SCHEMA,
                story: self.context.story.clone(),
                base: self.context.base.clone(),
                story_revision: view.story_revision,
                graph_revision: graph_rev,
                ledger_revision: ledger_rev,
                order,
                phase: IntegrationPhase::Blocked,
                candidate_commit: None,
                candidate_ref: None,
                integrated,
                blockers,
                worktrees,
            };
            self.save_state(&status)?;
            return Ok(status);
        }

        let total_changed = self.changed_paths(&self.context.base, &final_candidate)?;
        let expected_union: BTreeSet<String> = integrated
            .iter()
            .flat_map(|i| i.changed_paths.iter().cloned())
            .collect();
        let unexpected: Vec<String> = total_changed
            .iter()
            .filter(|p| !expected_union.contains(*p))
            .cloned()
            .collect();
        if total_changed.is_empty() || !unexpected.is_empty() {
            blockers.push(IntegrationBlocker {
                code: "candidate_scope_mismatch".into(),
                detail: if total_changed.is_empty() {
                    "combined candidate has no tree changes from baseline".into()
                } else {
                    "combined candidate contains changes outside integrated task diffs".into()
                },
                task: None,
                conflicting_task: None,
                commit: Some(final_candidate),
                paths: unexpected,
            });
            let status = IntegrationStatus {
                schema: SCHEMA,
                story: self.context.story.clone(),
                base: self.context.base.clone(),
                story_revision: view.story_revision,
                graph_revision: graph_rev,
                ledger_revision: ledger_rev,
                order,
                phase: IntegrationPhase::Blocked,
                candidate_commit: None,
                candidate_ref: None,
                integrated,
                blockers,
                worktrees,
            };
            self.save_state(&status)?;
            return Ok(status);
        }

        let candidate_ref = format!(
            "refs/storyflow/{}/{}/candidates/story-rev-{}-graph-{}",
            self.worktree_hash, self.context.story, view.story_revision, graph_rev
        );
        self.git_checked(&["update-ref", &candidate_ref, &final_candidate])?;

        let status = IntegrationStatus {
            schema: SCHEMA,
            story: self.context.story.clone(),
            base: self.context.base.clone(),
            story_revision: view.story_revision,
            graph_revision: graph_rev,
            ledger_revision: ledger_rev,
            order,
            phase: IntegrationPhase::Ready,
            candidate_commit: Some(final_candidate),
            candidate_ref: Some(candidate_ref),
            integrated,
            blockers: Vec::new(),
            worktrees,
        };
        self.save_state(&status)?;
        Ok(status)
    }

    fn require_live(&self, view: &TaskView) -> IntegrationResult<()> {
        require(
            !self.context.blocked
                && self.context.stage != TaskStage::Done
                && view.story_revision == self.context.story_revision
                && view.stage == self.context.stage,
            "stale_story_context",
            "story changed or is blocked; integration requires a live matching graph",
        )
    }

    /// Determine the required base commit for a task:
    /// - If the task has no ancestor prerequisites with code commits, its base is the pinned story baseline.
    /// - If the task depends (directly or transitively through read-only tasks) on prerequisites
    ///   with code commits, its base must descend from all ancestor prerequisite commits.
    fn expected_task_base(
        &self,
        view: &TaskView,
        graph: Option<&TaskGraph>,
        task_id: &str,
    ) -> IntegrationResult<String> {
        let _ = view
            .tasks
            .get(task_id)
            .ok_or_else(|| error("unknown_task", "task does not exist"))?;
        let ancestors = task_ancestors(view, task_id);
        let order = dependency_order(view, graph)?;
        let mut dep_commits = Vec::new();
        for dep_id in order.into_iter().filter(|id| ancestors.contains(id)) {
            let dep = view
                .tasks
                .get(&dep_id)
                .ok_or_else(|| error("unknown_task_dependency", "missing prerequisite"))?;
            require(
                dep.status == TaskStatus::Succeeded,
                "dependency_not_completed",
                format!("prerequisite {dep_id} has not succeeded"),
            )?;
            if let Some(commit) = dep.result.as_ref().and_then(|r| r.commit.as_ref()) {
                let resolved = self.resolve_commit(commit).map_err(|_| {
                    error(
                        "invalid_dependency_commit",
                        format!("prerequisite {dep_id} reported unknown commit {commit}"),
                    )
                })?;
                dep_commits.push((dep_id, resolved));
            }
        }
        if dep_commits.is_empty() {
            return Ok(self.context.base.clone());
        }
        if dep_commits.len() == 1 {
            return Ok(dep_commits[0].1.clone());
        }
        // Multiple code prerequisites: if one prerequisite commit already descends from all others,
        // use it; otherwise synthesize/verify a clean prerequisite base commit in dependency order.
        for (_, candidate) in &dep_commits {
            if dep_commits.iter().all(|(_, other)| {
                candidate == other || self.is_ancestor(other, candidate).unwrap_or(false)
            }) {
                return Ok(candidate.clone());
            }
        }
        // Merge prerequisite commits deterministically onto story base to form the task's base.
        let mut running = dep_commits[0].1.clone();
        let graph_rev = self.graph_revision_for(view);
        for (dep_id, commit) in dep_commits.iter().skip(1) {
            if self.is_ancestor(commit, &running)? {
                continue;
            }
            if self.is_ancestor(&running, commit)? {
                running = commit.clone();
                continue;
            }
            match self.merge_trees(
                &self.context.base,
                &running,
                commit,
                &format!("prereq-{dep_id}-for-{task_id}"),
                view.story_revision,
                graph_rev,
            )? {
                MergeOutcome::Clean(merged) => {
                    let prereq_ref = format!(
                        "refs/storyflow/{}/{}/prereqs/g{}/{}/{}",
                        self.worktree_hash, self.context.story, graph_rev, task_id, dep_id
                    );
                    self.git_checked(&["update-ref", &prereq_ref, &merged])?;
                    running = merged;
                }
                MergeOutcome::Conflict(_) => {
                    return Err(error(
                        "prerequisite_conflict",
                        format!(
                            "prerequisites for task {task_id} conflict and cannot form a clean base"
                        ),
                    ));
                }
            }
        }
        Ok(running)
    }

    fn scope_violations(
        &self,
        _task_id: &str,
        task_write_scope: &[String],
        changed: &[String],
    ) -> Vec<String> {
        let mut denied = Vec::new();
        for path in changed {
            let folded = portable_identity(path);
            let is_protected = BUILTIN_PROTECTED
                .iter()
                .copied()
                .chain(self.context.protected.iter().map(String::as_str))
                .any(|prefix| under(&folded, &portable_identity(prefix)));
            let in_story_scope = self.context.scope.iter().any(|prefix| under(path, prefix));
            let in_task_scope = task_write_scope.iter().any(|prefix| under(path, prefix));
            if is_protected || !in_story_scope || !in_task_scope {
                denied.push(path.clone());
            }
        }
        denied
    }

    fn save_state(&self, status: &IntegrationStatus) -> IntegrationResult<()> {
        let dir = self.integration_dir();
        create_dir(&dir)?;
        let stored = StoredState {
            sha256: checksum(status)?,
            body: status.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&stored)?;
        bytes.push(b'\n');
        require(
            bytes.len() <= MAX_INTEGRATION_STATE_BYTES,
            "integration_state_limit",
            "integration state exceeds 1 MiB",
        )?;
        let path = self.state_path();
        let pending = dir.join("state.pending.tmp");
        safe_existing_file(&pending)?;
        safe_existing_file(&path)?;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&pending)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(pending, &path)?;
        sync_parent(&path)?;
        Ok(())
    }

    fn git_command(&self) -> Command {
        let mut command = Command::new(process::git_executable());
        process::configure_environment(&mut command);
        command.args([
            "--no-pager",
            "--no-replace-objects",
            "--no-lazy-fetch",
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.longpaths=true",
        ]);
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(key);
            }
        }
        command
            .current_dir(&self.root)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn git_checked(&self, args: &[&str]) -> IntegrationResult<Vec<u8>> {
        let output = self
            .git_command()
            .args(args)
            .output()
            .map_err(|_| error("git_unavailable", "Git could not be started"))?;
        if !output.status.success() {
            return Err(error(
                "git_failed",
                format!(
                    "git failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(output.stdout)
    }

    fn git_supervised(&self, args: &[&str]) -> IntegrationResult<()> {
        let mut safe_args: Vec<String> = [
            "--no-replace-objects",
            "--no-lazy-fetch",
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.longpaths=true",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        safe_args.extend(args.iter().map(|s| (*s).to_owned()));
        let output = process::run(
            &CommandSpec {
                program: "git".into(),
                args: safe_args,
                timeout: Duration::from_secs(GIT_TIMEOUT_SECS),
            },
            &self.root,
        )
        .map_err(|_| error("git_failed", "bounded Git command failed"))?;
        if output.timed_out || !output.status.is_some_and(|s| s.success()) {
            return Err(error("git_failed", "bounded Git command failed"));
        }
        Ok(())
    }

    fn remove_git_worktree(&self, relative_path: &str) -> IntegrationResult<()> {
        self.git_supervised(&["worktree", "remove", "--force", relative_path])
    }

    fn resolve_commit(&self, rev: &str) -> IntegrationResult<String> {
        require(
            is_commit_id(rev),
            "invalid_commit_id",
            "exact hexadecimal commit ID required",
        )?;
        let out = self.git_checked(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])?;
        let commit = String::from_utf8(out)
            .map_err(|_| error("invalid_commit_id", "non-UTF-8 commit ID"))?
            .trim()
            .to_owned();
        require(
            is_commit_id(&commit) && commit.eq_ignore_ascii_case(rev),
            "invalid_commit_id",
            "invalid commit ID returned by Git",
        )?;
        Ok(commit)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> IntegrationResult<bool> {
        let status = self
            .git_command()
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .status()
            .map_err(|_| error("git_unavailable", "Git merge-base could not be started"))?;
        Ok(status.success())
    }

    fn is_strict_descendant(&self, base: &str, commit: &str) -> IntegrationResult<bool> {
        if base.eq_ignore_ascii_case(commit) {
            return Ok(false);
        }
        self.is_ancestor(base, commit)
    }

    /// Verify that every commit reachable from `commit` but not `base` has parents that either
    /// equal `base` or are themselves descendants of `base`. This rejects commits that merged in
    /// unrelated history or side branches outside `base`.
    fn has_only_descendant_parents(&self, base: &str, commit: &str) -> IntegrationResult<bool> {
        let out = self.git_checked(&["rev-list", "--parents", &format!("{base}..{commit}")])?;
        let text =
            String::from_utf8(out).map_err(|_| error("git_failed", "non-UTF-8 rev-list output"))?;
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(_node) = parts.next() else {
                continue;
            };
            let parents: Vec<_> = parts.collect();
            if parents.is_empty() {
                return Ok(false);
            }
            for parent in parents {
                if !parent.eq_ignore_ascii_case(base) && !self.is_ancestor(base, parent)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn changed_paths(&self, base: &str, candidate: &str) -> IntegrationResult<Vec<String>> {
        let bytes = self.git_checked(&[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--ignore-submodules=none",
            "--name-only",
            "-z",
            base,
            candidate,
            "--",
        ])?;
        let mut paths: Vec<String> = String::from_utf8(bytes)
            .map_err(|_| error("invalid_path", "non-UTF-8 changed path"))?
            .split_terminator('\0')
            .map(str::to_owned)
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    fn validate_candidate_namespace(&self, base: &str, candidate: &str) -> IntegrationResult<()> {
        let mut spellings = BTreeMap::<String, String>::new();
        for commit in [base, candidate] {
            let bytes = self.git_checked(&["ls-tree", "-r", "-z", "--name-only", commit])?;
            let names = String::from_utf8(bytes)
                .map_err(|_| error("invalid_path", "non-UTF-8 candidate namespace"))?;
            for path in names.split_terminator('\0') {
                let at = Origin::new(Scope::Repo, commit, "candidate_path");
                validate_relative_path(path, &at)
                    .map_err(|_| error("invalid_path", "unsafe relative path in tree"))?;
                let parts: Vec<_> = path.split('/').collect();
                for end in 1..=parts.len() {
                    let prefix = parts[..end].join("/");
                    let key = portable_identity(&prefix);
                    if let Some(previous) = spellings.insert(key, prefix.clone())
                        && previous != prefix
                    {
                        return Err(error(
                            "invalid_path",
                            "candidate aliases a baseline or candidate namespace entry",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Perform an in-memory 3-way tree merge using `git merge-tree --write-tree` (requires Git >= 2.38)
    /// and synthesize a deterministic commit object if clean. If conflicts occur, extract the exact
    /// conflicting file paths without modifying any working tree or index.
    fn merge_trees(
        &self,
        merge_base: &str,
        ours: &str,
        theirs: &str,
        task_label: &str,
        story_rev: u64,
        graph_rev: u64,
    ) -> IntegrationResult<MergeOutcome> {
        self.require_git_merge_tree_support()?;
        let output = self
            .git_command()
            .args([
                "merge-tree",
                "--write-tree",
                "--no-messages",
                "--name-only",
                "-z",
                &format!("--merge-base={merge_base}"),
                ours,
                theirs,
            ])
            .output()
            .map_err(|_| error("git_unavailable", "Git merge-tree could not be started"))?;

        if output.status.success() {
            let stdout = String::from_utf8(output.stdout)
                .map_err(|_| error("git_failed", "non-UTF-8 merge-tree output"))?;
            let tree = stdout.split('\0').next().unwrap_or("").trim();
            require(
                is_commit_id(tree),
                "git_failed",
                "merge-tree did not return a valid tree OID",
            )?;
            let commit =
                self.commit_integrated_tree(tree, ours, theirs, task_label, story_rev, graph_rev)?;
            return Ok(MergeOutcome::Clean(commit));
        }

        if output.status.code() == Some(1) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut parts = stdout.split_terminator('\0');
            // First NUL-terminated item is the tree OID; subsequent items are conflicting filenames.
            let _tree_oid = parts.next();
            let mut conflicts: Vec<String> =
                parts.filter(|s| !s.is_empty()).map(str::to_owned).collect();
            if conflicts.is_empty() {
                // Fallback: intersect changed paths between the two branches if merge-tree omitted names.
                let ours_paths: BTreeSet<_> =
                    self.changed_paths(merge_base, ours)?.into_iter().collect();
                for path in self.changed_paths(merge_base, theirs)? {
                    if ours_paths.contains(&path) {
                        conflicts.push(path);
                    }
                }
            }
            return Ok(MergeOutcome::Conflict(conflicts));
        }

        Err(error(
            "git_failed",
            format!(
                "git merge-tree failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }

    fn require_git_merge_tree_support(&self) -> IntegrationResult<()> {
        let out = self.git_checked(&["--version"])?;
        let text = String::from_utf8_lossy(&out);
        if let Some((major, minor)) = parse_git_version(&text) {
            require(
                (major, minor) >= (2, 38),
                "git_version_unsupported",
                format!(
                    "parallel commit integration requires Git 2.38 or newer for `git merge-tree --write-tree` (found {major}.{minor})"
                ),
            )
        } else {
            Err(error(
                "git_version_unsupported",
                "unable to verify Git >= 2.38 for `git merge-tree --write-tree`",
            ))
        }
    }

    /// Create a deterministic commit object from a merged tree with fixed metadata so repeated
    /// integrations over the same inputs produce identical commit IDs.
    fn commit_integrated_tree(
        &self,
        tree: &str,
        parent_ours: &str,
        parent_theirs: &str,
        task_label: &str,
        story_rev: u64,
        graph_rev: u64,
    ) -> IntegrationResult<String> {
        let message = format!(
            "storyflow integrate {}: story-rev {} graph-rev {} task {}\n",
            self.context.story, story_rev, graph_rev, task_label
        );
        let mut child = self
            .git_command()
            .env("GIT_AUTHOR_NAME", "Storyflow Integration")
            .env("GIT_AUTHOR_EMAIL", "storyflow-integration@example.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_NAME", "Storyflow Integration")
            .env(
                "GIT_COMMITTER_EMAIL",
                "storyflow-integration@example.invalid",
            )
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .args(["commit-tree", tree, "-p", parent_ours, "-p", parent_theirs])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| error("git_unavailable", "Git commit-tree could not be started"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(error(
                "git_failed",
                format!(
                    "git commit-tree failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        let commit = String::from_utf8(output.stdout)
            .map_err(|_| error("git_failed", "non-UTF-8 commit-tree output"))?
            .trim()
            .to_owned();
        require(
            is_commit_id(&commit),
            "git_failed",
            "invalid commit ID from commit-tree",
        )?;
        Ok(commit)
    }
}

enum MergeOutcome {
    Clean(String),
    Conflict(Vec<String>),
}

/// Compute a deterministic topological dependency order for the tasks in `view`.
/// If the original `TaskGraph` is provided, ties between independent tasks preserve declared
/// graph order; otherwise ties break lexicographically by task ID.
pub fn dependency_order(
    view: &TaskView,
    graph: Option<&TaskGraph>,
) -> IntegrationResult<Vec<String>> {
    let declared: Vec<String> = if let Some(g) = graph {
        g.tasks.iter().map(|t| t.id.clone()).collect()
    } else {
        view.tasks.keys().cloned().collect()
    };
    let mut visited = BTreeSet::new();
    let mut order = Vec::with_capacity(view.tasks.len());
    loop {
        let mut progressed = false;
        for id in &declared {
            if visited.contains(id) {
                continue;
            }
            let Some(task) = view.tasks.get(id) else {
                continue;
            };
            if task
                .spec
                .prerequisites
                .iter()
                .all(|dep| visited.contains(dep))
            {
                visited.insert(id.clone());
                order.push(id.clone());
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    require(
        order.len() == view.tasks.len(),
        "task_cycle",
        "cannot order tasks with unsatisfied or cyclic dependencies",
    )?;
    Ok(order)
}

fn task_ancestors(view: &TaskView, task_id: &str) -> BTreeSet<String> {
    let mut ancestors = BTreeSet::new();
    let mut stack: Vec<String> = view
        .tasks
        .get(task_id)
        .map(|t| t.spec.prerequisites.clone())
        .unwrap_or_default();
    while let Some(dep) = stack.pop() {
        if ancestors.insert(dep.clone())
            && let Some(dep_task) = view.tasks.get(&dep)
        {
            stack.extend(dep_task.spec.prerequisites.iter().cloned());
        }
    }
    ancestors
}

fn has_unstopped_worker(view: &TaskView, task_id: &str) -> bool {
    view.scheduler.as_ref().is_some_and(|s| {
        s.attempts
            .values()
            .any(|a| a.task == task_id && !a.worker_stopped)
    })
}

fn is_attempt_unstopped(view: &TaskView, attempt_id: &str) -> bool {
    view.scheduler.as_ref().is_some_and(|s| {
        s.attempts
            .get(attempt_id)
            .is_some_and(|a| !a.worker_stopped)
    })
}

fn active_attempt_for_running_task(
    view: &TaskView,
    task_id: &str,
    task_revision: u64,
) -> IntegrationResult<Option<String>> {
    let Some(scheduler) = &view.scheduler else {
        return Ok(None);
    };
    let is_current_leased = |a: &&crate::tasks::AttemptState| {
        a.graph_revision == scheduler.graph_revision
            && a.task == task_id
            && a.task_revision == task_revision
            && a.phase == AttemptPhase::Leased
    };
    let has_other_unstopped = scheduler
        .attempts
        .values()
        .any(|a| a.task == task_id && !a.worker_stopped && !is_current_leased(&a));
    require(
        !has_other_unstopped,
        "worker_stop_required",
        "an earlier worker attempt for this task has not been confirmed stopped",
    )?;
    let active: Vec<_> = scheduler
        .attempts
        .values()
        .filter(|a| is_current_leased(a) && !a.worker_stopped)
        .collect();
    require(
        active.len() == 1,
        "stale_attempt",
        "running scheduler task requires an active leased attempt",
    )?;
    Ok(Some(active[0].lease.attempt_id.clone()))
}

fn succeeded_attempt_for_task(
    view: &TaskView,
    task_id: &str,
    task_revision: u64,
) -> Option<String> {
    let scheduler = view.scheduler.as_ref()?;
    // On completion, task.revision was incremented by 1 from attempt.task_revision.
    scheduler
        .attempts
        .values()
        .find(|a| {
            a.graph_revision == scheduler.graph_revision
                && a.task == task_id
                && a.phase == AttemptPhase::Succeeded
                && a.worker_stopped
                && a.task_revision.saturating_add(1) == task_revision
        })
        .map(|a| a.lease.attempt_id.clone())
}

fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|a_path| {
        let a_fold = portable_identity(a_path);
        b.iter().any(|b_path| {
            let b_fold = portable_identity(b_path);
            under(&a_fold, &b_fold) || under(&b_fold, &a_fold)
        })
    })
}

pub fn portable_identity(path: &str) -> String {
    path.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfc()
        .collect()
}

pub fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    // Handles formats such as "git version 2.47.1.windows.2" or "git version 2.38.0".
    let token = output
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut nums = token.split('.');
    let major = nums.next()?.parse::<u32>().ok()?;
    let minor = nums.next()?.parse::<u32>().ok()?;
    Some((major, minor))
}

fn is_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn checksum(status: &IntegrationStatus) -> IntegrationResult<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(status)?)))
}

fn create_dir(path: &Path) -> IntegrationResult<()> {
    match fs::create_dir(path) {
        Ok(()) => sync_parent(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => regular_dir(path),
        Err(e) => Err(e.into()),
    }
}

fn sync_parent(path: &Path) -> IntegrationResult<()> {
    #[cfg(unix)]
    File::open(path.parent().expect("integration storage has a parent"))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn regular_dir(path: &Path) -> IntegrationResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    require(
        metadata.is_dir() && !link(&metadata),
        "integration_storage_path",
        "integration storage must use physical directories",
    )
}

fn safe_existing_file(path: &Path) -> IntegrationResult<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => require(
            m.is_file() && !link(&m),
            "integration_storage_path",
            "integration storage entry must be a regular file",
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
mod tests {
    use super::parse_git_version;

    #[test]
    fn parse_git_version_handles_standard_windows_and_invalid_strings() {
        assert_eq!(parse_git_version("git version 2.38.0\n"), Some((2, 38)));
        assert_eq!(
            parse_git_version("git version 2.47.1.windows.2\r\n"),
            Some((2, 47))
        );
        assert_eq!(parse_git_version("git version 2.37.3"), Some((2, 37)));
        assert_eq!(parse_git_version("git version 3.0"), Some((3, 0)));
        assert_eq!(parse_git_version("git version unknown"), None);
        assert_eq!(parse_git_version(""), None);
    }
}
