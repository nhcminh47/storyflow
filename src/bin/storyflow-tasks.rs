//! Companion entry point: reuse the authoritative story verifier and keep its
//! run lock alive across all task operations. Unused story mutation helpers are
//! deliberately private to these shared-source modules.
#[allow(dead_code)]
#[path = "../journal.rs"]
mod journal;
#[allow(dead_code)]
#[path = "../model.rs"]
mod model;
#[allow(dead_code)]
#[path = "../repo.rs"]
mod repo;

use std::{env, process::ExitCode};
use storyflow::tasks::*;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "storyflow-tasks init ID GRAPH.json\n\
storyflow-tasks replace ID REV GRAPH.json\n\
storyflow-tasks view ID [--markdown]\n\
storyflow-tasks packet ID TASK\n\
storyflow-tasks start ID REV TASK TASK_REV\n\
storyflow-tasks complete ID REV RESULT.json\n\
storyflow-tasks configure ID REV POLICY.json\n\
storyflow-tasks status ID\n\
storyflow-tasks dispatch ID REV TASK TASK_REV OWNER\n\
storyflow-tasks heartbeat ID REV LEASE.json\n\
storyflow-tasks finish ID REV ATTEMPT_RESULT.json\n\
storyflow-tasks cancel ID REV TASK TASK_REV REASON\n\
storyflow-tasks recover ID REV\n\
storyflow-tasks stopped ID REV LEASE.json REASON\n\
storyflow-tasks retry ID REV TASK TASK_REV REASON\n\n\
Uses an initialized story's pinned base, scope, stage and current revision.\n\
Configure opts into durable leases; legacy start/complete are then refused.\n\
Dispatch reserves a lease; the host launches workers and confirms they stopped.\n\
Expiry/cancellation/graph replacement do not prove a worker has stopped.\n\
Replace explicitly publishes a new graph after the story revision changes.\n\
Never launches a worker, integrates commits or approves a story.\n\
JSON output except --markdown; success 0, rejected/operational error 2.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("storyflow-tasks: {error}");
            ExitCode::from(2)
        }
    }
}
fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    let [command, id, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    // Validate grammar before touching any run data.
    if !matches!(
        (command.as_str(), rest),
        ("init", [_])
            | ("replace", [_, _])
            | ("view", [])
            | ("view", [..])
            | ("packet", [_])
            | ("start", [_, _, _])
            | ("complete", [_, _])
            | ("configure", [_, _])
            | ("status", [])
            | ("dispatch", [_, _, _, _])
            | ("heartbeat", [_, _])
            | ("finish", [_, _])
            | ("cancel", [_, _, _, _])
            | ("recover", [_])
            | ("stopped", [_, _, _])
            | ("retry", [_, _, _, _])
    ) || (command == "view" && !(rest.is_empty() || rest == ["--markdown"]))
    {
        return Err(USAGE.into());
    }
    // Revisions are grammar, so malformed numbers cannot create task storage.
    if matches!(
        command.as_str(),
        "replace"
            | "start"
            | "complete"
            | "configure"
            | "dispatch"
            | "heartbeat"
            | "finish"
            | "cancel"
            | "recover"
            | "stopped"
            | "retry"
    ) {
        rest[0].parse::<u64>()?;
    }
    if matches!(command.as_str(), "start" | "dispatch" | "cancel" | "retry") {
        rest[2].parse::<u64>()?;
    }
    let root = repo::root()?;
    let story = journal::Journal::open(&root, id, false)?;
    let state = story.state()?;
    let stage = match state.stage {
        model::Stage::Plan => TaskStage::Plan,
        model::Stage::Implement => TaskStage::Implement,
        model::Stage::Gate => TaskStage::Gate,
        model::Stage::Review => TaskStage::Review,
        model::Stage::Done => TaskStage::Done,
    };
    let mut protected: Vec<String> = if let Some(snapshot) = &story.snapshot {
        snapshot
            .effective
            .policy
            .protected
            .iter()
            .map(|v| v.value.clone())
            .collect()
    } else {
        repo::policy(&root, &state.base)?.protected
    };
    protected.extend(repo::RESERVED.iter().map(|p| (*p).to_owned()));
    let context = RunContext {
        story: id.clone(),
        base: state.base.clone(),
        story_revision: story.revision()? as u64,
        stage,
        blocked: state.blocked || stage == TaskStage::Done,
        scope: repo::story(&root, &state.base, id)?.scope,
        protected,
    };
    let mut tasks = TaskJournal::open(&root, context)?;
    let clock = SystemClock;
    let output = match (command.as_str(), rest) {
        ("init", [path]) => {
            tasks.initialize(read_json(path)?)?;
            serde_json::to_value(tasks.view())?
        }
        ("replace", [revision, path]) => {
            tasks.replace(revision.parse()?, read_json(path)?)?;
            serde_json::to_value(tasks.view())?
        }
        ("view", []) => serde_json::to_value(tasks.view())?,
        ("view", [_]) => {
            print!("{}", tasks.view().markdown());
            return Ok(());
        }
        ("packet", [task]) => serde_json::to_value(tasks.packet(task)?)?,
        ("start", [revision, task, task_revision]) => {
            serde_json::to_value(tasks.start(revision.parse()?, task, task_revision.parse()?)?)?
        }
        ("complete", [revision, path]) => {
            tasks.complete(revision.parse()?, read_json(path)?)?;
            serde_json::to_value(tasks.view())?
        }
        ("configure", [revision, path]) => {
            tasks.configure_scheduler(revision.parse()?, read_json(path)?, &clock)?;
            serde_json::to_value(tasks.scheduler_status(&clock)?)?
        }
        ("status", []) => serde_json::to_value(tasks.scheduler_status(&clock)?)?,
        ("dispatch", [revision, task, task_revision, owner]) => {
            serde_json::to_value(tasks.dispatch(
                revision.parse()?,
                task,
                task_revision.parse()?,
                owner,
                &clock,
            )?)?
        }
        ("heartbeat", [revision, path]) => {
            tasks.heartbeat(revision.parse()?, &read_json(path)?, &clock)?;
            serde_json::to_value(tasks.scheduler_status(&clock)?)?
        }
        ("finish", [revision, path]) => {
            tasks.complete_attempt(revision.parse()?, read_json(path)?, &clock)?;
            serde_json::to_value(tasks.scheduler_status(&clock)?)?
        }
        ("cancel", [revision, task, task_revision, reason]) => {
            tasks.cancel_task(
                revision.parse()?,
                task,
                task_revision.parse()?,
                reason,
                &clock,
            )?;
            serde_json::to_value(tasks.scheduler_status(&clock)?)?
        }
        ("recover", [revision]) => {
            tasks.recover_expired(revision.parse()?, &clock)?;
            serde_json::to_value(tasks.scheduler_status(&clock)?)?
        }
        ("stopped", [revision, path, reason]) => {
            tasks.confirm_stopped(revision.parse()?, &read_json(path)?, reason, &clock)?;
            serde_json::to_value(tasks.scheduler_status(&clock)?)?
        }
        ("retry", [revision, task, task_revision, reason]) => {
            serde_json::to_value(tasks.retry_task(
                revision.parse()?,
                task,
                task_revision.parse()?,
                reason,
                &clock,
            )?)?
        }
        _ => return Err(USAGE.into()),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    // `story` (including run.lock) is retained until after task publication/output.
    drop(tasks);
    drop(story);
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    Ok(serde_json::from_slice(&read_document(
        std::path::Path::new(path),
    )?)?)
}
