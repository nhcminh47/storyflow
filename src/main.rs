mod gate;
mod journal;
mod model;
mod repo;

use journal::Journal;
use model::{CheckResult, Event, Stage, State};
use serde_json::{Value, json};
use std::{env, fs::File, io::Read, path::Path, process::ExitCode};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "+storyflow init ID [BASE]
storyflow next ID
storyflow status ID
storyflow submit ID REV plan|block|resume REPORT.md
storyflow submit ID REV candidate|approve|changes REPORT.md COMMIT
storyflow gate ID REV
storyflow audit ID";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("storyflow: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        println!("{USAGE}");
        return Ok(0);
    }
    let [command, id, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let root = repo::root()?;
    let mut journal = Journal::open(&root, id, command == "init" && rest.len() <= 1)?;
    let output = match (command.as_str(), rest) {
        ("init", [] | [_]) => {
            if journal.state.is_some() {
                return Err("run already exists; use next to continue it".into());
            }
            let base = repo::resolve(&root, rest.first().map(String::as_str).unwrap_or("HEAD"))?;
            repo::policy(&root, &base)?;
            repo::story(&root, &base, id)?;
            repo::spec(&root, &base, id)?;
            for role in storyflow::config::LEGACY_ROLES {
                contract(&root, &base, role)?;
            }
            repo::pin(&root, id, 0, &base)?;
            journal.append(Event::Init {
                story: id.into(),
                base,
            })?;
            summary(&journal, id)?
        }
        ("next", []) => context(&root, &journal, id)?,
        ("status", []) => summary(&journal, id)?,
        ("audit", []) => {
            journal.state()?;
            json!({ "status": summary(&journal, id)?, "events": journal.records })
        }
        ("submit", [revision, action, report, commit @ ..]) => {
            journal.expect_revision(revision)?;
            let state = journal.state()?;
            let report = read_report(report)?;
            let event = match (action.as_str(), commit) {
                ("plan", []) => Event::Plan { report },
                ("block", []) => Event::Block { report },
                ("resume", []) => Event::Resume { report },
                ("candidate", [commit]) => {
                    require_stage(state, Stage::Implement)?;
                    let commit = repo::resolve(&root, commit)?;
                    repo::assert_descendant(&root, &state.base, &commit)?;
                    repo::pin(&root, id, journal.records.len(), &commit)?;
                    Event::Candidate { commit, report }
                }
                ("approve" | "changes", [commit]) => {
                    require_stage(state, Stage::Review)?;
                    let commit = repo::resolve(&root, commit)?;
                    Event::Review {
                        commit,
                        approved: action == "approve",
                        report,
                    }
                }
                _ => return Err(USAGE.into()),
            };
            journal.append(event)?;
            summary(&journal, id)?
        }
        ("gate", [revision]) => {
            journal.expect_revision(revision)?;
            let state = journal.state()?;
            require_stage(state, Stage::Gate)?;
            let commit = state.candidate.clone().ok_or("no candidate")?;
            let policy = repo::policy(&root, &state.base)?;
            let story = repo::story(&root, &state.base, id)?;
            let checks = gate::run(&root, &journal.dir, &policy, &story, &state.base, &commit)?;
            let passed = checks.iter().all(CheckResult::passed);
            journal.append(Event::Gate {
                commit,
                checks: checks.clone(),
            })?;
            print_json(&json!({ "status": summary(&journal, id)?, "checks": checks }))?;
            return Ok(if passed { 0 } else { 1 });
        }
        _ => return Err(USAGE.into()),
    };
    print_json(&output)?;
    Ok(0)
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn require_stage(state: &State, stage: Stage) -> Result<()> {
    if state.blocked || state.stage != stage {
        return Err(format!("requires unblocked stage {stage:?}; run next").into());
    }
    Ok(())
}

fn contract(root: &Path, base: &str, role: &str) -> Result<String> {
    repo::read(root, base, &format!("agents/{role}.md"), 4096)
}

fn summary(journal: &Journal, id: &str) -> Result<Value> {
    let state = journal.state()?;
    Ok(json!({
        "story": id,
        "revision": journal.revision()?,
        "rejected_candidates": state.rejected_candidates,
        "stage": state.stage,
        "blocked": state.blocked,
        "base": state.base,
        "candidate": state.candidate,
    }))
}

fn context(root: &Path, journal: &Journal, id: &str) -> Result<Value> {
    let state = journal.state()?;
    let status = summary(journal, id)?;
    if state.blocked {
        return Ok(json!({ "status": status, "action": "resume", "blocker": state.blocker }));
    }
    let role = match state.stage {
        Stage::Plan => "planner",
        Stage::Implement => "implementer",
        Stage::Gate => {
            return Ok(json!({
                "status": status, "action": "gate", "resume_note": state.resume_note,
            }));
        }
        Stage::Review => "reviewer",
        Stage::Done => {
            return Ok(json!({
                "status": status, "action": "report_completion", "review": state.feedback,
            }));
        }
    };
    let mut packet = json!({
        "status": status, "action": "delegate", "role": role,
        "contract": contract(root, &state.base, role)?,
        "story": repo::spec(root, &state.base, id)?,
        "scope": repo::story(root, &state.base, id)?.scope,
        "resume_note": state.resume_note,
    });
    if state.stage != Stage::Plan {
        packet["plan"] = json!(state.plan);
    }
    if state.stage == Stage::Implement {
        packet["review_feedback"] = json!(state.feedback);
        packet["gate_failures"] = json!(
            state
                .checks
                .iter()
                .filter(|check| !check.passed())
                .collect::<Vec<_>>()
        );
    }
    if state.stage == Stage::Review {
        packet["implementation"] = json!(state.implementation);
        packet["passed_checks"] = json!(
            state
                .checks
                .iter()
                .map(|check| &check.id)
                .collect::<Vec<_>>()
        );
    }
    Ok(packet)
}

fn read_report(path: &str) -> Result<String> {
    const MAX: u64 = 4096;
    let mut bytes = Vec::new();
    File::open(path)?.take(MAX + 1).read_to_end(&mut bytes)?;
    let report = String::from_utf8(bytes)?;
    if report.trim().is_empty() || report.len() as u64 > MAX {
        return Err("report must contain 1–4096 UTF-8 bytes".into());
    }
    Ok(report)
}
