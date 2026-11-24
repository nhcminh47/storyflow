//! Authoritative bridge between `TaskJournal` scheduler leases and `HostAdapter`.
//!
//! All state transitions go through `TaskJournal`; adapters never write story
//! journals, bypass quality gates, or self-certify completion.
use super::{
    ADAPTER_SCHEMA, AdapterOutcome, AdapterRequest, CancelToken, ExecutionPolicy, HostAdapter,
    SessionCorrelation, sanitize_reason,
};
use crate::tasks::{
    AttemptPhase, AttemptResult, Clock, DispatchPacket, ResultStatus, SCHEMA, TaskJournal,
    TaskResult, TaskStage, TaskStatus, WorkerResult,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileStatus {
    Applied,
    Blocked,
    BlockedUnstopped,
    Cancelled,
    RejectedDuplicateOrStale,
    HostUnavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReconcileReport {
    pub status: ReconcileStatus,
    pub code: String,
    pub detail: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub worker_stopped: bool,
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_revision: Option<u64>,
}

/// Probe host availability, reserve a scheduler lease, run the adapter, and
/// reconcile the typed outcome into `TaskJournal`.
///
/// Missing/unavailable hosts fail closed before reserving a dispatch lease so
/// attempt budget is not consumed before the host exists.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_and_run(
    journal: &mut TaskJournal,
    adapter: &dyn HostAdapter,
    task: &str,
    task_revision: u64,
    owner: &str,
    workdir: &Path,
    timeout: Duration,
    policy: ExecutionPolicy,
    cancel: &CancelToken,
    clock: &impl Clock,
) -> TaskResult<ReconcileReport> {
    let caps = adapter.probe();
    if !caps.available {
        return Ok(ReconcileReport {
            status: ReconcileStatus::HostUnavailable,
            code: "host_unavailable".to_owned(),
            detail: caps
                .unavailable_reason
                .unwrap_or_else(|| format!("host '{}' is unavailable", adapter.provider())),
            task: task.to_owned(),
            attempt_id: None,
            session_id: None,
            worker_stopped: true,
            recoverable: true,
            ledger_revision: journal.view().revision,
        });
    }

    let rev = journal
        .view()
        .revision
        .ok_or_else(|| crate::tasks::TaskError {
            code: "task_uninitialized",
            detail: "initialize the graph first".to_owned(),
        })?;
    let dispatch = journal.dispatch(rev, task, task_revision, owner, clock)?;
    let timeout_ms = timeout.as_millis().min(u64::MAX as u128) as u64;
    let request = AdapterRequest {
        schema: ADAPTER_SCHEMA,
        dispatch: dispatch.clone(),
        workdir: workdir.to_path_buf(),
        timeout_ms,
        policy,
    };
    if let Err(err) = request.validate() {
        let stop_rev = journal.view().revision.unwrap_or(rev);
        let detail = sanitize_reason(&err.to_string());
        journal.confirm_stopped(stop_rev, &dispatch.lease, &detail, clock)?;
        return Ok(ReconcileReport {
            status: ReconcileStatus::Blocked,
            code: err.code.to_owned(),
            detail,
            task: task.to_owned(),
            attempt_id: Some(dispatch.lease.attempt_id),
            session_id: None,
            worker_stopped: true,
            recoverable: true,
            ledger_revision: journal.view().revision,
        });
    }

    let outcome = adapter.execute(&request, cancel);
    reconcile_outcome(journal, &dispatch, outcome, clock)
}

/// Reconcile an `AdapterOutcome` against the authoritative `TaskJournal`.
pub fn reconcile_outcome(
    journal: &mut TaskJournal,
    dispatch: &DispatchPacket,
    outcome: AdapterOutcome,
    clock: &impl Clock,
) -> TaskResult<ReconcileReport> {
    let task_id = dispatch.packet.task.spec.id.clone();
    let attempt_id = dispatch.lease.attempt_id.clone();
    let session_id = outcome.session_id().map(str::to_owned);

    let Some(scheduler) = journal.view().scheduler.as_ref() else {
        return Ok(rejected_report(
            "scheduler_unconfigured",
            "scheduler is not configured",
            task_id,
            attempt_id,
            session_id,
            journal.view().revision,
        ));
    };
    let Some(attempt) = scheduler.attempts.get(&attempt_id).cloned() else {
        return Ok(rejected_report(
            "stale_attempt",
            "attempt is not recorded in this task journal",
            task_id,
            attempt_id,
            session_id,
            journal.view().revision,
        ));
    };
    if attempt.lease != dispatch.lease {
        return Ok(rejected_report(
            "stale_lease",
            "dispatch lease token does not match recorded attempt",
            task_id,
            attempt_id,
            session_id,
            journal.view().revision,
        ));
    }

    if attempt.worker_stopped {
        return Ok(rejected_report(
            "duplicate_or_settled_attempt",
            "attempt has already terminated and released its worker lease",
            task_id,
            attempt_id,
            session_id,
            journal.view().revision,
        ));
    }

    let rev = journal
        .view()
        .revision
        .ok_or_else(|| crate::tasks::TaskError {
            code: "task_uninitialized",
            detail: "initialize the graph first".to_owned(),
        })?;
    let worker_stopped = outcome.worker_stopped();

    match attempt.phase {
        AttemptPhase::Cancelled => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "cancelled task worker has not confirmed exit",
                    task_id,
                    attempt_id,
                    session_id,
                    Some(rev),
                ));
            }
            let reason = sanitize_reason(&outcome_summary(&outcome, "cancelled worker stopped"));
            journal.confirm_stopped(rev, &dispatch.lease, &reason, clock)?;
            return Ok(ReconcileReport {
                status: ReconcileStatus::Cancelled,
                code: "task_cancelled".to_owned(),
                detail: reason,
                task: task_id,
                attempt_id: Some(attempt_id),
                session_id,
                worker_stopped: true,
                recoverable: false,
                ledger_revision: journal.view().revision,
            });
        }
        AttemptPhase::Retired => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "retired graph worker has not confirmed exit",
                    task_id,
                    attempt_id,
                    session_id,
                    Some(rev),
                ));
            }
            let reason = sanitize_reason(&outcome_summary(&outcome, "retired worker stopped"));
            journal.confirm_stopped(rev, &dispatch.lease, &reason, clock)?;
            return Ok(ReconcileReport {
                status: ReconcileStatus::RejectedDuplicateOrStale,
                code: "attempt_retired".to_owned(),
                detail: reason,
                task: task_id,
                attempt_id: Some(attempt_id),
                session_id,
                worker_stopped: true,
                recoverable: false,
                ledger_revision: journal.view().revision,
            });
        }
        AttemptPhase::Expired => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "lease_expired_unstopped",
                    "expired lease still requires worker stop confirmation",
                    task_id,
                    attempt_id,
                    session_id,
                    Some(rev),
                ));
            }
            let reason = sanitize_reason(&outcome_summary(&outcome, "expired worker stopped"));
            journal.confirm_stopped(rev, &dispatch.lease, &reason, clock)?;
            return Ok(blocked_report(
                "lease_expired",
                reason,
                task_id,
                attempt_id,
                session_id,
                journal.view().revision,
            ));
        }
        AttemptPhase::Succeeded | AttemptPhase::Failed | AttemptPhase::Blocked => {
            return Ok(rejected_report(
                "duplicate_or_settled_attempt",
                "attempt already recorded a terminal phase",
                task_id,
                attempt_id,
                session_id,
                Some(rev),
            ));
        }
        AttemptPhase::Leased => {}
    }

    // Attempt is Leased. Check whether the story context drifted before completion.
    if !is_live_context(journal) {
        if !worker_stopped {
            return Ok(unstopped_report(
                "stale_story_context",
                "story context changed while worker is still unstopped",
                task_id,
                attempt_id,
                session_id,
                Some(rev),
            ));
        }
        let reason = sanitize_reason("story context changed; worker stop confirmed");
        journal.confirm_stopped(rev, &dispatch.lease, &reason, clock)?;
        return Ok(ReconcileReport {
            status: ReconcileStatus::RejectedDuplicateOrStale,
            code: "stale_story_context".to_owned(),
            detail: reason,
            task: task_id,
            attempt_id: Some(attempt_id),
            session_id,
            worker_stopped: true,
            recoverable: false,
            ledger_revision: journal.view().revision,
        });
    }

    // Check lease expiry using the latest durable attempt expiry in the journal.
    if clock.now_ms() >= attempt.expires_at_ms {
        journal.recover_expired(rev, clock)?;
        let latest_rev = journal.view().revision.unwrap_or(rev);
        if !worker_stopped {
            return Ok(unstopped_report(
                "lease_expired_unstopped",
                "lease expired; confirm worker stop before retrying",
                task_id,
                attempt_id,
                session_id,
                Some(latest_rev),
            ));
        }
        let reason = sanitize_reason(&outcome_summary(
            &outcome,
            "lease expired; worker stop confirmed",
        ));
        journal.confirm_stopped(latest_rev, &dispatch.lease, &reason, clock)?;
        return Ok(blocked_report(
            "lease_expired",
            reason,
            task_id,
            attempt_id,
            session_id,
            journal.view().revision,
        ));
    }

    match outcome {
        AdapterOutcome::Completed {
            correlation,
            result,
            worker_stopped,
        } => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "completion refused without explicit worker_stopped confirmation",
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    Some(rev),
                ));
            }
            if let Err(err) = validate_correlation(&correlation, &dispatch.lease) {
                let reason = sanitize_reason(&err);
                journal.confirm_stopped(rev, &dispatch.lease, &reason, clock)?;
                return Ok(blocked_report(
                    "correlation_mismatch",
                    reason,
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    journal.view().revision,
                ));
            }
            if let Err(err) = validate_worker_result(&result, dispatch) {
                let reason = sanitize_reason(&err);
                journal.confirm_stopped(rev, &dispatch.lease, &reason, clock)?;
                return Ok(blocked_report(
                    "invalid_worker_result",
                    reason,
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    journal.view().revision,
                ));
            }
            let status_code = match result.status {
                ResultStatus::Succeeded => "task_succeeded",
                ResultStatus::Failed => "task_failed",
                ResultStatus::Blocked => "task_blocked",
            };
            let recoverable = !matches!(result.status, ResultStatus::Succeeded);
            let detail = result.report.clone();
            journal.complete_attempt(
                rev,
                AttemptResult {
                    schema: SCHEMA,
                    lease: dispatch.lease.clone(),
                    result,
                    worker_stopped: true,
                },
                clock,
            )?;
            Ok(ReconcileReport {
                status: ReconcileStatus::Applied,
                code: status_code.to_owned(),
                detail,
                task: task_id,
                attempt_id: Some(attempt_id),
                session_id: Some(correlation.session_id),
                worker_stopped: true,
                recoverable,
                ledger_revision: journal.view().revision,
            })
        }
        AdapterOutcome::Cancelled {
            correlation,
            worker_stopped,
            reason,
        } => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "cancelled worker has not confirmed exit",
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    Some(rev),
                ));
            }
            let detail = sanitize_reason(&reason);
            journal.confirm_stopped(rev, &dispatch.lease, &detail, clock)?;
            let task_cancelled = journal
                .view()
                .tasks
                .get(&task_id)
                .is_some_and(|t| t.status == TaskStatus::Cancelled);
            Ok(ReconcileReport {
                status: if task_cancelled {
                    ReconcileStatus::Cancelled
                } else {
                    ReconcileStatus::Blocked
                },
                code: "worker_cancelled".to_owned(),
                detail,
                task: task_id,
                attempt_id: Some(attempt_id),
                session_id: Some(correlation.session_id),
                worker_stopped: true,
                recoverable: !task_cancelled,
                ledger_revision: journal.view().revision,
            })
        }
        AdapterOutcome::TimedOut {
            correlation,
            worker_stopped,
            reason,
        } => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "timed-out worker has not confirmed exit",
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    Some(rev),
                ));
            }
            let detail = sanitize_reason(&reason);
            journal.confirm_stopped(rev, &dispatch.lease, &detail, clock)?;
            Ok(blocked_report(
                "worker_timed_out",
                detail,
                task_id,
                attempt_id,
                Some(correlation.session_id),
                journal.view().revision,
            ))
        }
        AdapterOutcome::HostCrashed {
            correlation,
            worker_stopped,
            exit_code,
            detail,
        } => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "crashed host process tree was not confirmed stopped",
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    Some(rev),
                ));
            }
            let msg = match exit_code {
                Some(code) => sanitize_reason(&format!("host exited ({code}): {detail}")),
                None => sanitize_reason(&format!("host crashed: {detail}")),
            };
            journal.confirm_stopped(rev, &dispatch.lease, &msg, clock)?;
            Ok(blocked_report(
                "host_crashed",
                msg,
                task_id,
                attempt_id,
                Some(correlation.session_id),
                journal.view().revision,
            ))
        }
        AdapterOutcome::InvalidOutput {
            correlation,
            worker_stopped,
            detail,
        } => {
            if !worker_stopped {
                return Ok(unstopped_report(
                    "worker_stop_required",
                    "host produced invalid output and worker exit is unconfirmed",
                    task_id,
                    attempt_id,
                    Some(correlation.session_id),
                    Some(rev),
                ));
            }
            let msg = sanitize_reason(&format!("invalid host output: {detail}"));
            journal.confirm_stopped(rev, &dispatch.lease, &msg, clock)?;
            Ok(blocked_report(
                "invalid_output",
                msg,
                task_id,
                attempt_id,
                Some(correlation.session_id),
                journal.view().revision,
            ))
        }
        AdapterOutcome::HostUnavailable { provider, detail } => {
            let msg = sanitize_reason(&format!("host {provider} unavailable: {detail}"));
            journal.confirm_stopped(rev, &dispatch.lease, &msg, clock)?;
            Ok(ReconcileReport {
                status: ReconcileStatus::HostUnavailable,
                code: "host_unavailable".to_owned(),
                detail: msg,
                task: task_id,
                attempt_id: Some(attempt_id),
                session_id: None,
                worker_stopped: true,
                recoverable: true,
                ledger_revision: journal.view().revision,
            })
        }
    }
}

fn is_live_context(journal: &TaskJournal) -> bool {
    let ctx = journal.context();
    let view = journal.view();
    !ctx.blocked
        && ctx.stage != TaskStage::Done
        && view.story_revision == ctx.story_revision
        && view.stage == ctx.stage
}

fn validate_correlation(
    correlation: &SessionCorrelation,
    lease: &crate::tasks::LeaseToken,
) -> Result<(), String> {
    correlation.validate().map_err(|e| e.to_string())?;
    if !correlation.matches_lease(lease) {
        return Err(
            "adapter correlation attempt_id/lease_id/owner does not match dispatch".to_owned(),
        );
    }
    Ok(())
}

fn validate_worker_result(result: &WorkerResult, dispatch: &DispatchPacket) -> Result<(), String> {
    result.validate().map_err(|e| e.to_string())?;
    if result.story != dispatch.packet.story || result.task != dispatch.packet.task.spec.id {
        return Err("worker result story/task does not match dispatched task".to_owned());
    }
    if result.task_revision != dispatch.packet.task.revision {
        return Err("stale worker result task_revision differs from dispatched attempt".to_owned());
    }
    Ok(())
}

fn outcome_summary(outcome: &AdapterOutcome, fallback: &str) -> String {
    match outcome {
        AdapterOutcome::Completed { .. } => fallback.to_owned(),
        AdapterOutcome::Cancelled { reason, .. } => reason.clone(),
        AdapterOutcome::TimedOut { reason, .. } => reason.clone(),
        AdapterOutcome::HostCrashed { detail, .. } => detail.clone(),
        AdapterOutcome::HostUnavailable { detail, .. } => detail.clone(),
        AdapterOutcome::InvalidOutput { detail, .. } => detail.clone(),
    }
}

fn rejected_report(
    code: &str,
    detail: &str,
    task: String,
    attempt_id: String,
    session_id: Option<String>,
    ledger_revision: Option<u64>,
) -> ReconcileReport {
    ReconcileReport {
        status: ReconcileStatus::RejectedDuplicateOrStale,
        code: code.to_owned(),
        detail: detail.to_owned(),
        task,
        attempt_id: Some(attempt_id),
        session_id,
        worker_stopped: false,
        recoverable: false,
        ledger_revision,
    }
}

fn unstopped_report(
    code: &str,
    detail: &str,
    task: String,
    attempt_id: String,
    session_id: Option<String>,
    ledger_revision: Option<u64>,
) -> ReconcileReport {
    ReconcileReport {
        status: ReconcileStatus::BlockedUnstopped,
        code: code.to_owned(),
        detail: detail.to_owned(),
        task,
        attempt_id: Some(attempt_id),
        session_id,
        worker_stopped: false,
        recoverable: false,
        ledger_revision,
    }
}

fn blocked_report(
    code: &str,
    detail: String,
    task: String,
    attempt_id: String,
    session_id: Option<String>,
    ledger_revision: Option<u64>,
) -> ReconcileReport {
    ReconcileReport {
        status: ReconcileStatus::Blocked,
        code: code.to_owned(),
        detail,
        task,
        attempt_id: Some(attempt_id),
        session_id,
        worker_stopped: true,
        recoverable: true,
        ledger_revision,
    }
}
