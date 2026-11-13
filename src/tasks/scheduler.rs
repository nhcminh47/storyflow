//! Durable reservations for a provider-neutral host. A lease is not evidence
//! that a worker was launched; stopping a worker remains an explicit host claim.
use super::model::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub trait Clock {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchedulerPolicy {
    pub max_concurrency: u16,
    pub max_attempts: u16,
    pub lease_ms: u64,
}
impl SchedulerPolicy {
    fn validate(&self) -> TaskResult<()> {
        require(
            (1..=256).contains(&self.max_concurrency)
                && (1..=100).contains(&self.max_attempts)
                && (1..=86_400_000).contains(&self.lease_ms),
            "scheduler_policy",
            "policy requires concurrency 1..256, attempts 1..100 and lease 1..86400000 ms",
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LeaseToken {
    pub attempt_id: String,
    pub lease_id: String,
    pub owner: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    Leased,
    Succeeded,
    Failed,
    Blocked,
    Expired,
    Cancelled,
    Retired,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttemptState {
    pub lease: LeaseToken,
    pub graph_revision: u64,
    pub story_revision: u64,
    pub task: String,
    pub task_revision: u64,
    pub write_scope: Vec<String>,
    pub phase: AttemptPhase,
    pub dispatched_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub expires_at_ms: u64,
    pub worker_stopped: bool,
    pub result: Option<WorkerResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchedulerState {
    pub policy: SchedulerPolicy,
    pub graph_revision: u64,
    pub last_observed_ms: u64,
    pub attempts: BTreeMap<String, AttemptState>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DispatchPacket {
    pub schema: u32,
    pub packet: TaskPacket,
    pub lease: LeaseToken,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttemptResult {
    pub schema: u32,
    pub lease: LeaseToken,
    pub result: WorkerResult,
    pub worker_stopped: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchedulerBlocker {
    pub code: String,
    pub detail: String,
    pub attempt_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchedulerStatus {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub story_revision: u64,
    pub graph_revision: u64,
    pub ledger_revision: u64,
    pub current: bool,
    pub policy: SchedulerPolicy,
    pub last_observed_ms: u64,
    pub observed_at_ms: u64,
    pub active_attempts: usize,
    pub dispatchable: Vec<String>,
    pub blockers: BTreeMap<String, Vec<SchedulerBlocker>>,
    pub attempts: BTreeMap<String, AttemptState>,
}

/// Only journal publication/replay applies these events. Event time is durable
/// input; replay never consults the host clock or relaunches a worker.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulerEvent {
    Configure {
        policy: SchedulerPolicy,
        at_ms: u64,
    },
    Dispatch {
        task: String,
        task_revision: u64,
        owner: String,
        at_ms: u64,
    },
    Heartbeat {
        lease: LeaseToken,
        at_ms: u64,
    },
    Complete {
        result: AttemptResult,
        at_ms: u64,
    },
    Cancel {
        task: String,
        task_revision: u64,
        reason: String,
        at_ms: u64,
    },
    Recover {
        at_ms: u64,
    },
    Stopped {
        lease: LeaseToken,
        reason: String,
        at_ms: u64,
    },
    Retry {
        task: String,
        task_revision: u64,
        reason: String,
        at_ms: u64,
    },
}
impl SchedulerEvent {
    fn at_ms(&self) -> u64 {
        match self {
            Self::Configure { at_ms, .. }
            | Self::Dispatch { at_ms, .. }
            | Self::Heartbeat { at_ms, .. }
            | Self::Complete { at_ms, .. }
            | Self::Cancel { at_ms, .. }
            | Self::Recover { at_ms }
            | Self::Stopped { at_ms, .. }
            | Self::Retry { at_ms, .. } => *at_ms,
        }
    }
}

pub(crate) fn configured(view: &TaskView) -> TaskResult<&SchedulerState> {
    view.scheduler.as_ref().ok_or_else(|| {
        error(
            "scheduler_unconfigured",
            "configure the scheduler before managing attempts",
        )
    })
}

pub(crate) fn check_time(state: &SchedulerState, now: u64) -> TaskResult<()> {
    require(
        now >= state.last_observed_ms,
        "scheduler_clock_regression",
        "clock precedes the last durable scheduler observation",
    )
}

fn expiry(now: u64, policy: &SchedulerPolicy) -> TaskResult<u64> {
    now.checked_add(policy.lease_ms).ok_or_else(|| {
        error(
            "scheduler_clock_overflow",
            "lease expiry exceeds clock range",
        )
    })
}

fn text(value: &str, max: usize, code: &'static str) -> TaskResult<()> {
    require(
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        code,
        "value must be nonblank, bounded and contain no control characters",
    )
}

fn revision(view: &TaskView) -> TaskResult<u64> {
    view.revision
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| error("task_uninitialized", "initialize the graph first"))
}

fn token(view: &TaskView, task: &str, owner: &str) -> TaskResult<LeaseToken> {
    let state = configured(view)?;
    let input = serde_json::to_vec(&(
        &view.story,
        &view.base,
        state.graph_revision,
        view.story_revision,
        task,
        revision(view)?,
        owner,
    ))?;
    let hash = |domain: &[u8]| {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update(&input);
        format!("{:x}", digest.finalize())
    };
    Ok(LeaseToken {
        attempt_id: hash(b"storyflow-attempt-v1\0"),
        lease_id: hash(b"storyflow-lease-v1\0"),
        owner: owner.to_owned(),
    })
}

fn task_at<'a>(view: &'a TaskView, id: &str, rev: u64) -> TaskResult<&'a TaskState> {
    let task = view
        .tasks
        .get(id)
        .ok_or_else(|| error("unknown_task", "task does not exist"))?;
    require(
        task.revision == rev,
        "stale_task_revision",
        "task revision changed",
    )?;
    Ok(task)
}

fn exact_attempt<'a>(
    state: &'a SchedulerState,
    token: &LeaseToken,
) -> TaskResult<&'a AttemptState> {
    let attempt = state
        .attempts
        .get(&token.attempt_id)
        .ok_or_else(|| error("stale_attempt", "attempt is not recorded in this run"))?;
    require(
        attempt.lease == *token,
        "stale_lease",
        "attempt, lease and owner must match the recorded reservation",
    )?;
    Ok(attempt)
}

fn live_attempt<'a>(
    view: &'a TaskView,
    token: &LeaseToken,
    now: u64,
) -> TaskResult<&'a AttemptState> {
    let scheduler = configured(view)?;
    let attempt = exact_attempt(scheduler, token)?;
    require(
        attempt.graph_revision == scheduler.graph_revision
            && attempt.story_revision == view.story_revision
            && attempt.phase == AttemptPhase::Leased
            && !attempt.worker_stopped,
        "stale_attempt",
        "attempt is no longer active in the current graph",
    )?;
    require(
        now < attempt.expires_at_ms,
        "lease_expired",
        "lease expired; recover and confirm worker stop before retrying",
    )?;
    let task = task_at(view, &attempt.task, attempt.task_revision)?;
    require(
        task.status == TaskStatus::Running,
        "stale_attempt",
        "task is no longer running for this reservation",
    )?;
    Ok(attempt)
}

fn blocker(code: &str, detail: &str, attempt: Option<&AttemptState>) -> SchedulerBlocker {
    SchedulerBlocker {
        code: code.into(),
        detail: detail.into(),
        attempt_id: attempt.map(|a| a.lease.attempt_id.clone()),
    }
}

fn overlapping(a: &[String], b: &[String]) -> bool {
    a.iter().any(|a| {
        let a = fold(a);
        b.iter().any(|b| {
            let b = fold(b);
            within(&a, &b) || within(&b, &a)
        })
    })
}

fn task_blockers(
    view: &TaskView,
    id: &str,
    now: u64,
    current: bool,
) -> TaskResult<Vec<SchedulerBlocker>> {
    let state = configured(view)?;
    let task = &view.tasks[id];
    let mut blockers = Vec::new();
    if !current {
        blockers.push(blocker(
            "stale_story_context",
            "replace the graph for the current unblocked, unfinished story revision",
            None,
        ));
    }
    if task.status != TaskStatus::Ready {
        let (code, detail) = match task.status {
            TaskStatus::Pending => (
                "dependencies_pending",
                "successful prerequisite results are required",
            ),
            TaskStatus::Running => (
                "task_running",
                "wait for result or explicitly stop/recover this worker",
            ),
            TaskStatus::Succeeded => ("task_succeeded", "task already succeeded"),
            TaskStatus::Failed | TaskStatus::Blocked => (
                "retry_required",
                "explicit retry is required after all workers stop",
            ),
            TaskStatus::Cancelled => ("task_cancelled", "cancellation is terminal for this graph"),
            TaskStatus::Ready => unreachable!(),
        };
        blockers.push(blocker(code, detail, None));
    }
    let attempts = state
        .attempts
        .values()
        .filter(|a| a.graph_revision == state.graph_revision && a.task == id)
        .count();
    if attempts >= usize::from(state.policy.max_attempts) {
        blockers.push(blocker(
            "attempt_budget_exhausted",
            "attempt budget is exhausted",
            None,
        ));
    }
    if state
        .attempts
        .values()
        .filter(|a| !a.worker_stopped)
        .count()
        >= usize::from(state.policy.max_concurrency)
    {
        blockers.push(blocker(
            "concurrency_limit",
            "confirm an occupied worker has stopped before reserving another slot",
            None,
        ));
    }
    for attempt in state.attempts.values().filter(|a| !a.worker_stopped) {
        if attempt.task == id {
            blockers.push(blocker(
                "worker_stop_required",
                "the prior worker has not been confirmed stopped",
                Some(attempt),
            ));
        }
        if overlapping(&task.spec.write_scope, &attempt.write_scope) {
            blockers.push(blocker(
                "write_scope_occupied",
                "an unstopped worker owns an overlapping write scope",
                Some(attempt),
            ));
        }
        if attempt.graph_revision == state.graph_revision
            && attempt.task == id
            && attempt.phase == AttemptPhase::Leased
            && now >= attempt.expires_at_ms
        {
            blockers.push(blocker(
                "lease_expired",
                "recover the expired lease and confirm worker stop",
                Some(attempt),
            ));
        }
    }
    Ok(blockers)
}

pub(crate) fn status(view: &TaskView, now: u64, current: bool) -> TaskResult<SchedulerStatus> {
    let state = configured(view)?;
    check_time(state, now)?;
    let mut dispatchable = Vec::new();
    let mut blockers = BTreeMap::new();
    for id in view.tasks.keys() {
        let reasons = task_blockers(view, id, now, current)?;
        if reasons.is_empty() {
            dispatchable.push(id.clone());
        } else {
            blockers.insert(id.clone(), reasons);
        }
    }
    Ok(SchedulerStatus {
        schema: SCHEMA,
        story: view.story.clone(),
        base: view.base.clone(),
        story_revision: view.story_revision,
        graph_revision: state.graph_revision,
        ledger_revision: view
            .revision
            .ok_or_else(|| error("task_uninitialized", "initialize graph first"))?,
        current,
        policy: state.policy.clone(),
        last_observed_ms: state.last_observed_ms,
        observed_at_ms: now,
        active_attempts: state
            .attempts
            .values()
            .filter(|a| !a.worker_stopped)
            .count(),
        dispatchable,
        blockers,
        attempts: state.attempts.clone(),
    })
}

pub(crate) fn retire_graph(view: &mut TaskView, graph_revision: u64) {
    if let Some(state) = &mut view.scheduler {
        for attempt in state.attempts.values_mut().filter(|a| !a.worker_stopped) {
            attempt.phase = AttemptPhase::Retired;
        }
        state.graph_revision = graph_revision;
    }
}

pub(crate) fn apply(
    view: &mut TaskView,
    graph_revision: u64,
    event: &SchedulerEvent,
) -> TaskResult<()> {
    if let SchedulerEvent::Configure { policy, at_ms } = event {
        require(
            view.scheduler.is_none(),
            "scheduler_already_configured",
            "scheduler policy is immutable within a run",
        )?;
        require(
            view.revision.is_some(),
            "task_uninitialized",
            "initialize graph first",
        )?;
        require(
            !view.tasks.values().any(|t| t.status == TaskStatus::Running),
            "legacy_task_running",
            "finish legacy running tasks before enabling scheduler",
        )?;
        policy.validate()?;
        view.scheduler = Some(SchedulerState {
            policy: policy.clone(),
            graph_revision,
            last_observed_ms: *at_ms,
            attempts: BTreeMap::new(),
        });
        return Ok(());
    }
    let now = event.at_ms();
    check_time(configured(view)?, now)?;
    match event {
        SchedulerEvent::Configure { .. } => unreachable!(),
        SchedulerEvent::Dispatch {
            task,
            task_revision,
            owner,
            ..
        } => {
            text(owner, 128, "invalid_lease_owner")?;
            task_at(view, task, *task_revision)?;
            let blockers = task_blockers(view, task, now, true)?;
            if let Some(first) = blockers.first() {
                return Err(error(
                    "task_not_dispatchable",
                    format!("{}: {}", first.code, first.detail),
                ));
            }
            let expires_at_ms = expiry(now, &configured(view)?.policy)?;
            let lease = token(view, task, owner)?;
            let state = view.tasks.get_mut(task).expect("validated task");
            state.revision += 1;
            state.status = TaskStatus::Running;
            let attempt = AttemptState {
                lease: lease.clone(),
                graph_revision,
                story_revision: view.story_revision,
                task: task.clone(),
                task_revision: state.revision,
                write_scope: state.spec.write_scope.clone(),
                phase: AttemptPhase::Leased,
                dispatched_at_ms: now,
                heartbeat_at_ms: now,
                expires_at_ms,
                worker_stopped: false,
                result: None,
            };
            let scheduler = view.scheduler.as_mut().expect("configured");
            require(
                !scheduler.attempts.contains_key(&lease.attempt_id),
                "duplicate_attempt",
                "attempt identity has already been used",
            )?;
            scheduler.attempts.insert(lease.attempt_id, attempt);
        }
        SchedulerEvent::Heartbeat { lease, .. } => {
            live_attempt(view, lease, now)?;
            let expires_at_ms = expiry(now, &configured(view)?.policy)?;
            let attempt = view
                .scheduler
                .as_mut()
                .expect("configured")
                .attempts
                .get_mut(&lease.attempt_id)
                .expect("validated lease");
            attempt.heartbeat_at_ms = now;
            attempt.expires_at_ms = expires_at_ms;
        }
        SchedulerEvent::Complete { result, .. } => {
            require(
                result.schema == SCHEMA,
                "task_schema",
                "unsupported attempt result schema",
            )?;
            require(
                result.worker_stopped,
                "worker_stop_required",
                "completion requires explicit host confirmation that the worker stopped",
            )?;
            let attempt = live_attempt(view, &result.lease, now)?;
            result.result.validate()?;
            require(
                result.result.story == view.story && result.result.task == attempt.task,
                "task_identity",
                "result differs from the recorded attempt story/task",
            )?;
            require(
                result.result.task_revision == attempt.task_revision,
                "stale_task_revision",
                "result task revision differs from the running attempt",
            )?;
            let phase = match result.result.status {
                ResultStatus::Succeeded => AttemptPhase::Succeeded,
                ResultStatus::Failed => AttemptPhase::Failed,
                ResultStatus::Blocked => AttemptPhase::Blocked,
            };
            let state = view
                .tasks
                .get_mut(&result.result.task)
                .expect("validated task");
            state.status = match result.result.status {
                ResultStatus::Succeeded => TaskStatus::Succeeded,
                ResultStatus::Failed => TaskStatus::Failed,
                ResultStatus::Blocked => TaskStatus::Blocked,
            };
            state.revision += 1;
            state.result = Some(result.result.clone());
            let attempt = view
                .scheduler
                .as_mut()
                .expect("configured")
                .attempts
                .get_mut(&result.lease.attempt_id)
                .expect("validated lease");
            attempt.phase = phase;
            attempt.worker_stopped = true;
            attempt.result = Some(result.result.clone());
        }
        SchedulerEvent::Cancel {
            task,
            task_revision,
            reason,
            ..
        } => {
            text(reason, MAX_REPORT_BYTES, "invalid_scheduler_reason")?;
            let state = task_at(view, task, *task_revision)?;
            require(
                !matches!(state.status, TaskStatus::Succeeded | TaskStatus::Cancelled),
                "invalid_task_transition",
                "succeeded/cancelled tasks cannot be cancelled",
            )?;
            let generation = configured(view)?.graph_revision;
            for attempt in view
                .scheduler
                .as_mut()
                .expect("configured")
                .attempts
                .values_mut()
            {
                if attempt.graph_revision == generation
                    && attempt.task == *task
                    && !attempt.worker_stopped
                {
                    attempt.phase = AttemptPhase::Cancelled;
                }
            }
            let state = view.tasks.get_mut(task).expect("validated task");
            state.status = TaskStatus::Cancelled;
            state.revision += 1;
            state.result = None;
        }
        SchedulerEvent::Recover { .. } => {
            let scheduler = view.scheduler.as_mut().expect("configured");
            for attempt in scheduler.attempts.values_mut() {
                if attempt.phase == AttemptPhase::Leased
                    && !attempt.worker_stopped
                    && now >= attempt.expires_at_ms
                {
                    attempt.phase = AttemptPhase::Expired;
                    if attempt.graph_revision == scheduler.graph_revision
                        && let Some(task) = view.tasks.get_mut(&attempt.task)
                        && task.status == TaskStatus::Running
                        && task.revision == attempt.task_revision
                    {
                        task.status = TaskStatus::Blocked;
                        task.revision += 1;
                    }
                }
            }
        }
        SchedulerEvent::Stopped { lease, reason, .. } => {
            text(reason, MAX_REPORT_BYTES, "invalid_scheduler_reason")?;
            let scheduler = configured(view)?;
            let attempt = exact_attempt(scheduler, lease)?;
            require(
                !attempt.worker_stopped,
                "worker_already_stopped",
                "worker stop was already recorded",
            )?;
            let task_id = attempt.task.clone();
            let task_revision = attempt.task_revision;
            let is_current = attempt.graph_revision == scheduler.graph_revision;
            let attempt = view
                .scheduler
                .as_mut()
                .expect("configured")
                .attempts
                .get_mut(&lease.attempt_id)
                .expect("validated lease");
            if attempt.phase == AttemptPhase::Leased {
                attempt.phase = AttemptPhase::Blocked;
            }
            attempt.worker_stopped = true;
            if is_current
                && let Some(task) = view.tasks.get_mut(&task_id)
                && task.status == TaskStatus::Running
                && task.revision == task_revision
            {
                task.status = TaskStatus::Blocked;
                task.revision += 1;
            }
        }
        SchedulerEvent::Retry {
            task,
            task_revision,
            reason,
            ..
        } => {
            text(reason, MAX_REPORT_BYTES, "invalid_scheduler_reason")?;
            let state = task_at(view, task, *task_revision)?;
            require(
                matches!(state.status, TaskStatus::Failed | TaskStatus::Blocked),
                "invalid_task_transition",
                "retry requires a failed or blocked task",
            )?;
            let scheduler = configured(view)?;
            let attempts: Vec<_> = scheduler
                .attempts
                .values()
                .filter(|a| a.graph_revision == scheduler.graph_revision && a.task == *task)
                .collect();
            require(
                attempts.iter().all(|a| a.worker_stopped),
                "worker_stop_required",
                "confirm all earlier workers stopped before retrying",
            )?;
            require(
                attempts.len() < usize::from(scheduler.policy.max_attempts),
                "attempt_budget_exhausted",
                "attempt budget is exhausted",
            )?;
            let state = view.tasks.get_mut(task).expect("validated task");
            state.status = TaskStatus::Pending;
            state.revision += 1;
            state.result = None;
        }
    }
    view.scheduler
        .as_mut()
        .expect("configured")
        .last_observed_ms = now;
    Ok(())
}

pub(crate) fn dispatch_packet(view: &TaskView, task: &str) -> TaskResult<DispatchPacket> {
    let scheduler = configured(view)?;
    let state = view
        .tasks
        .get(task)
        .ok_or_else(|| error("unknown_task", "task does not exist"))?;
    let attempt = scheduler
        .attempts
        .values()
        .find(|a| {
            a.graph_revision == scheduler.graph_revision
                && a.task == task
                && a.task_revision == state.revision
                && a.phase == AttemptPhase::Leased
        })
        .ok_or_else(|| error("stale_attempt", "no current dispatch for task"))?;
    Ok(DispatchPacket {
        schema: SCHEMA,
        packet: view.packet(task)?,
        lease: attempt.lease.clone(),
        expires_at_ms: attempt.expires_at_ms,
    })
}
