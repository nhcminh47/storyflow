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

const USAGE: &str = "storyflow-tasks init ID GRAPH.json\nstoryflow-tasks view ID [--markdown]\nstoryflow-tasks packet ID TASK\nstoryflow-tasks start ID REV TASK TASK_REV\nstoryflow-tasks complete ID REV RESULT.json\n\nTask bookkeeping only: never launches a worker or approves a story.\nUses an initialized story's pinned base, scope, stage and current revision.\nJSON output; success 0, rejected/operational error 2. No-task view is additive.";

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
            | ("view", [])
            | ("view", [..])
            | ("packet", [_])
            | ("start", [_, _, _])
            | ("complete", [_, _])
    ) || (command == "view" && !(rest.is_empty() || rest == ["--markdown"]))
    {
        return Err(USAGE.into());
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
    let output = match (command.as_str(), rest) {
        ("init", [path]) => {
            tasks.initialize(serde_json::from_slice(&read_document(
                std::path::Path::new(path),
            )?)?)?;
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
            tasks.complete(
                revision.parse()?,
                serde_json::from_slice(&read_document(std::path::Path::new(path))?)?,
            )?;
            serde_json::to_value(tasks.view())?
        }
        _ => return Err(USAGE.into()),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    // `story` (including run.lock) is retained until after task publication/output.
    drop(tasks);
    drop(story);
    Ok(())
}
