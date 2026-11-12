use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Plan,
    Implement,
    Gate,
    Review,
    Done,
}

pub use storyflow::config::Check;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub protected: Vec<String>,
    pub checks: Vec<Check>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Story {
    pub scope: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail,
    Timeout,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub id: String,
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub log: Option<String>,
}

impl CheckResult {
    pub fn passed(&self) -> bool {
        matches!(self.outcome, Outcome::Pass)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Init {
        story: String,
        base: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_sha256: Option<String>,
    },
    Plan {
        report: String,
    },
    Candidate {
        commit: String,
        report: String,
    },
    Gate {
        commit: String,
        checks: Vec<CheckResult>,
    },
    Review {
        commit: String,
        approved: bool,
        report: String,
    },
    Block {
        report: String,
    },
    Resume {
        report: String,
    },
}

#[derive(Clone)]
pub struct State {
    pub rejected_candidates: usize,
    pub base: String,
    pub config_sha256: Option<String>,
    pub stage: Stage,
    pub blocked: bool,
    pub plan: Option<String>,
    pub candidate: Option<String>,
    pub implementation: Option<String>,
    pub checks: Vec<CheckResult>,
    pub feedback: Option<String>,
    pub blocker: Option<String>,
    pub resume_note: Option<String>,
}

impl State {
    pub fn new(event: &Event, story: &str) -> Result<Self> {
        let Event::Init {
            story: id,
            base,
            config_sha256,
        } = event
        else {
            return Err("journal must begin with init".into());
        };
        if id != story || !crate::repo::is_commit_id(base) {
            return Err("invalid init event".into());
        }
        if config_sha256
            .as_ref()
            .is_some_and(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("invalid init config digest".into());
        }
        Ok(Self {
            rejected_candidates: 0,
            base: base.clone(),
            config_sha256: config_sha256.clone(),
            stage: Stage::Plan,
            blocked: false,
            plan: None,
            candidate: None,
            implementation: None,
            checks: Vec::new(),
            feedback: None,
            blocker: None,
            resume_note: None,
        })
    }

    pub fn apply(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::Block { report } if !self.blocked && self.stage != Stage::Done => {
                self.blocked = true;
                self.blocker = Some(report.clone());
            }
            Event::Resume { report } if self.blocked => {
                self.blocked = false;
                self.blocker = None;
                self.resume_note = Some(report.clone());
            }
            _ if self.blocked => return Err("run is blocked; resume it first".into()),
            Event::Plan { report } if self.stage == Stage::Plan => {
                self.plan = Some(report.clone());
                self.stage = Stage::Implement;
                self.resume_note = None;
            }
            Event::Candidate { commit, report }
                if self.stage == Stage::Implement && crate::repo::is_commit_id(commit) =>
            {
                self.candidate = Some(commit.clone());
                self.implementation = Some(report.clone());
                self.checks.clear();
                self.stage = Stage::Gate;
                self.resume_note = None;
            }
            Event::Gate { commit, checks }
                if self.stage == Stage::Gate
                    && self.candidate.as_ref() == Some(commit)
                    && !checks.is_empty() =>
            {
                self.checks.clone_from(checks);
                if let Some(check) = checks
                    .iter()
                    .find(|check| matches!(check.outcome, Outcome::Error | Outcome::Timeout))
                {
                    self.blocked = true;
                    self.blocker = Some(format!(
                        "{} needs operator attention: {}",
                        check.id, check.detail
                    ));
                } else if checks.iter().all(CheckResult::passed) {
                    self.stage = Stage::Review;
                } else {
                    self.rejected_candidates += 1;
                    self.stage = Stage::Implement;
                }
                self.resume_note = None;
            }
            Event::Review {
                commit,
                approved,
                report,
            } if self.stage == Stage::Review && self.candidate.as_ref() == Some(commit) => {
                self.feedback = Some(report.clone());
                self.stage = if *approved {
                    Stage::Done
                } else {
                    self.rejected_candidates += 1;
                    Stage::Implement
                };
                self.resume_note = None;
            }
            _ => return Err(format!("event is not valid in {:?}", self.stage).into()),
        }
        Ok(())
    }
}
