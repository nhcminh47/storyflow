//! Managed worker entry point. The host must route every worker operation here.
#[allow(dead_code)]
#[path = "../journal.rs"]
mod journal;
#[allow(dead_code)]
#[path = "../model.rs"]
mod model;
#[allow(dead_code)]
#[path = "../repo.rs"]
mod repo;

use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, process::ExitCode};
use storyflow::{security::*, tasks::*};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: u32,
    lease: LeaseToken,
    operation: Operation,
}
fn main() -> ExitCode {
    match run() {
        Ok(result) => {
            println!("{}", json!({"ok":true,"result":result}));
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("{}", json!({"ok":false,"error":error}));
            ExitCode::from(2)
        }
    }
}
fn run() -> SecurityResult<Value> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        return Ok(
            json!({"usage":"storyflow-runtime ID TASK < REQUEST.json", "backend":"managed-only", "operations":["read","write","git_read"], "boundary":"External host tools are not restricted. Arbitrary git and shell operations are refused."}),
        );
    }
    let [id, task_id] = args.as_slice() else {
        return Err(error(
            "invalid_request",
            "expected storyflow-runtime ID TASK with one JSON request on stdin",
        ));
    };
    // Read worker-controlled input before acquiring authoritative locks.
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(2_097_153)
        .read_to_end(&mut bytes)
        .map_err(|_| error("invalid_request", "cannot read request"))?;
    if bytes.len() > 2_097_152 {
        return Err(error("request_too_large", "request exceeds 2 MiB"));
    }
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|_| error("invalid_request", "invalid or unsupported request"))?;
    if request.schema != 1 {
        return Err(error("invalid_request", "unsupported request schema"));
    }
    let root = repo::root().map_err(|_| error("invalid_repository", "repository lookup failed"))?;
    let story = journal::Journal::open(&root, id, false).map_err(|_| {
        error(
            "snapshot_integrity",
            "story journal or pinned configuration verification failed",
        )
    })?;
    let state = story
        .state()
        .map_err(|_| error("snapshot_integrity", "story is not initialized"))?;
    let snapshot = story.snapshot.as_ref().ok_or_else(|| {
        error(
            "managed_runtime_required",
            "legacy runs cannot admit managed workers",
        )
    })?;
    if !snapshot.managed_only {
        return Err(error(
            "managed_runtime_required",
            "initialize a new run with --managed-runtime",
        ));
    }
    let mut audit = open_audit(&story.dir)?;
    let result = (|| {
        let context = RunContext {
            story: id.clone(),
            base: state.base.clone(),
            story_revision: story
                .revision()
                .map_err(|_| error("snapshot_integrity", "story revision unavailable"))?
                as u64,
            stage: match state.stage {
                model::Stage::Plan => TaskStage::Plan,
                model::Stage::Implement => TaskStage::Implement,
                model::Stage::Gate => TaskStage::Gate,
                model::Stage::Review => TaskStage::Review,
                model::Stage::Done => TaskStage::Done,
            },
            blocked: state.blocked || state.stage == model::Stage::Done,
            scope: repo::story(&root, &state.base, id)
                .map_err(|_| error("snapshot_integrity", "pinned story unavailable"))?
                .scope,
            protected: snapshot
                .effective
                .policy
                .protected
                .iter()
                .map(|p| p.value.clone())
                .chain(repo::RESERVED.iter().map(|p| (*p).to_owned()))
                .collect(),
        };
        let tasks = TaskJournal::open(&root, context.clone())
            .map_err(|_| error("task_integrity", "task journal verification failed"))?;
        let status = tasks
            .scheduler_status(&SystemClock)
            .map_err(|_| error("lease_denied", "current scheduler required"))?;
        let attempt = status
            .attempts
            .get(&request.lease.attempt_id)
            .ok_or_else(|| error("lease_denied", "unknown or stale lease"))?;
        if !status.current
            || attempt.lease != request.lease
            || attempt.task != *task_id
            || attempt.phase != AttemptPhase::Leased
            || attempt.worker_stopped
            || status.observed_at_ms >= attempt.expires_at_ms
            || attempt.graph_revision != status.graph_revision
            || attempt.story_revision != context.story_revision
        {
            return Err(error("lease_denied", "unknown or stale lease"));
        }
        let packet = tasks
            .packet(task_id)
            .map_err(|_| error("lease_denied", "task is not current"))?;
        if packet.task.status != TaskStatus::Running
            || packet.task.revision != attempt.task_revision
        {
            return Err(error(
                "lease_denied",
                "task revision does not match the active lease",
            ));
        }

        let runtime = ManagedRuntime::new(
            &root,
            snapshot,
            state
                .config_sha256
                .as_deref()
                .ok_or_else(|| error("snapshot_integrity", "missing snapshot binding"))?,
            context,
            packet.task.spec,
            vec![], // Optional extra host floors; execute enforces the pinned path itself.
            RuntimeIdentity {
                attempt_id: attempt.lease.attempt_id.clone(),
                graph_revision: attempt.graph_revision,
                expires_at_ms: attempt.expires_at_ms,
            },
        )?;
        let output = runtime.execute(&request.operation, &mut audit);
        drop(tasks);
        output
    })();
    if let Err(failure) = &result {
        audit_rejection(
            &mut audit,
            state.config_sha256.as_deref().unwrap_or(""),
            failure.code,
        )?;
    }
    result
}
