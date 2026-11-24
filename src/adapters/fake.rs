//! Deterministic fake `HostAdapter` for offline unit/integration tests and CI.
use super::{
    ADAPTER_SCHEMA, AdapterOutcome, AdapterRequest, CancelToken, HostAdapter, HostCapabilities,
    SessionCorrelation,
};
use crate::tasks::{Artifact, ResultStatus, SCHEMA, WorkerResult};
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
};

pub type FakeHandler = Arc<dyn Fn(&AdapterRequest, &CancelToken) -> AdapterOutcome + Send + Sync>;

#[derive(Clone)]
pub enum FakeBehavior {
    Succeed {
        report: String,
        commit: Option<String>,
        artifacts: Vec<Artifact>,
    },
    Fail {
        report: String,
        commit: Option<String>,
        artifacts: Vec<Artifact>,
    },
    Blocked {
        report: String,
    },
    InvalidOutput {
        detail: String,
    },
    MismatchedCorrelation,
    InvalidSessionId {
        session_id: String,
    },
    StaleRevision,
    WrongTask {
        task: String,
    },
    WrongStory {
        story: String,
    },
    UnstoppedCompletion,
    TimedOut {
        reason: String,
        worker_stopped: bool,
    },
    Cancelled {
        reason: String,
        worker_stopped: bool,
    },
    HostCrashed {
        exit_code: Option<i32>,
        detail: String,
        worker_stopped: bool,
    },
    HostUnavailable {
        detail: String,
    },
    Custom(FakeHandler),
}

impl fmt::Debug for FakeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeed { report, .. } => f.debug_tuple("Succeed").field(report).finish(),
            Self::Fail { report, .. } => f.debug_tuple("Fail").field(report).finish(),
            Self::Blocked { report } => f.debug_tuple("Blocked").field(report).finish(),
            Self::InvalidOutput { detail } => f.debug_tuple("InvalidOutput").field(detail).finish(),
            Self::MismatchedCorrelation => f.write_str("MismatchedCorrelation"),
            Self::InvalidSessionId { session_id } => {
                f.debug_tuple("InvalidSessionId").field(session_id).finish()
            }
            Self::StaleRevision => f.write_str("StaleRevision"),
            Self::WrongTask { task } => f.debug_tuple("WrongTask").field(task).finish(),
            Self::WrongStory { story } => f.debug_tuple("WrongStory").field(story).finish(),
            Self::UnstoppedCompletion => f.write_str("UnstoppedCompletion"),
            Self::TimedOut {
                reason,
                worker_stopped,
            } => f
                .debug_struct("TimedOut")
                .field("reason", reason)
                .field("worker_stopped", worker_stopped)
                .finish(),
            Self::Cancelled {
                reason,
                worker_stopped,
            } => f
                .debug_struct("Cancelled")
                .field("reason", reason)
                .field("worker_stopped", worker_stopped)
                .finish(),
            Self::HostCrashed {
                exit_code,
                detail,
                worker_stopped,
            } => f
                .debug_struct("HostCrashed")
                .field("exit_code", exit_code)
                .field("detail", detail)
                .field("worker_stopped", worker_stopped)
                .finish(),
            Self::HostUnavailable { detail } => {
                f.debug_tuple("HostUnavailable").field(detail).finish()
            }
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

pub struct FakeAdapter {
    capabilities: HostCapabilities,
    queue: Mutex<VecDeque<FakeBehavior>>,
    default: FakeBehavior,
    requests: Mutex<Vec<AdapterRequest>>,
    session_counter: Mutex<u64>,
}

impl FakeAdapter {
    pub fn available(provider: impl Into<String>) -> Self {
        let provider = provider.into();
        Self {
            capabilities: HostCapabilities {
                schema: ADAPTER_SCHEMA,
                provider,
                available: true,
                version: Some("fake-host 1.0.0".to_owned()),
                executable: Some("fake-host".to_owned()),
                supports_json_output: true,
                supports_output_schema: true,
                supports_output_file: true,
                supports_working_dir: true,
                supports_sandbox_flag: true,
                supports_cancel: true,
                supports_session_id: true,
                supports_stdin_prompt: true,
                detected_flags: vec!["--fake".to_owned()],
                unavailable_reason: None,
            },
            queue: Mutex::new(VecDeque::new()),
            default: FakeBehavior::Succeed {
                report: "fake task completed".to_owned(),
                commit: None,
                artifacts: Vec::new(),
            },
            requests: Mutex::new(Vec::new()),
            session_counter: Mutex::new(0),
        }
    }

    pub fn unavailable(provider: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            capabilities: HostCapabilities::unavailable(provider, reason),
            queue: Mutex::new(VecDeque::new()),
            default: FakeBehavior::HostUnavailable {
                detail: "host binary missing".to_owned(),
            },
            requests: Mutex::new(Vec::new()),
            session_counter: Mutex::new(0),
        }
    }

    pub fn with_default(mut self, behavior: FakeBehavior) -> Self {
        self.default = behavior;
        self
    }

    pub fn push_behavior(&self, behavior: FakeBehavior) {
        self.queue.lock().expect("queue lock").push_back(behavior);
    }

    pub fn requests(&self) -> Vec<AdapterRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    fn next_session_id(&self) -> String {
        let mut counter = self.session_counter.lock().expect("counter lock");
        *counter += 1;
        format!("fake-session-{}", *counter)
    }
}

impl HostAdapter for FakeAdapter {
    fn provider(&self) -> &str {
        &self.capabilities.provider
    }

    fn probe(&self) -> HostCapabilities {
        self.capabilities.clone()
    }

    fn execute(&self, request: &AdapterRequest, cancel: &CancelToken) -> AdapterOutcome {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());

        if !self.capabilities.available {
            return AdapterOutcome::HostUnavailable {
                provider: self.capabilities.provider.clone(),
                detail: self
                    .capabilities
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "fake host unavailable".to_owned()),
            };
        }

        let behavior = self
            .queue
            .lock()
            .expect("queue lock")
            .pop_front()
            .unwrap_or_else(|| self.default.clone());

        let session_id = self.next_session_id();
        let correlation = SessionCorrelation::from_lease(&request.dispatch.lease, session_id);

        if let Err(err) = request.validate() {
            return AdapterOutcome::InvalidOutput {
                correlation,
                worker_stopped: true,
                detail: err.to_string(),
            };
        }

        if cancel.is_cancelled()
            && !matches!(
                behavior,
                FakeBehavior::Custom(_)
                    | FakeBehavior::Cancelled { .. }
                    | FakeBehavior::UnstoppedCompletion
            )
        {
            return AdapterOutcome::Cancelled {
                correlation,
                worker_stopped: true,
                reason: "cancelled before execution".to_owned(),
            };
        }

        match behavior {
            FakeBehavior::Succeed {
                report,
                commit,
                artifacts,
            } => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story: request.dispatch.packet.story.clone(),
                    task: request.dispatch.packet.task.spec.id.clone(),
                    task_revision: request.dispatch.packet.task.revision,
                    status: ResultStatus::Succeeded,
                    report,
                    commit,
                    artifacts,
                },
                worker_stopped: true,
            },
            FakeBehavior::Fail {
                report,
                commit,
                artifacts,
            } => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story: request.dispatch.packet.story.clone(),
                    task: request.dispatch.packet.task.spec.id.clone(),
                    task_revision: request.dispatch.packet.task.revision,
                    status: ResultStatus::Failed,
                    report,
                    commit,
                    artifacts,
                },
                worker_stopped: true,
            },
            FakeBehavior::Blocked { report } => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story: request.dispatch.packet.story.clone(),
                    task: request.dispatch.packet.task.spec.id.clone(),
                    task_revision: request.dispatch.packet.task.revision,
                    status: ResultStatus::Blocked,
                    report,
                    commit: None,
                    artifacts: Vec::new(),
                },
                worker_stopped: true,
            },
            FakeBehavior::InvalidOutput { detail } => AdapterOutcome::InvalidOutput {
                correlation,
                worker_stopped: true,
                detail,
            },
            FakeBehavior::MismatchedCorrelation => {
                let mut bad = correlation;
                bad.lease_id = "forged-lease-id".to_owned();
                AdapterOutcome::Completed {
                    correlation: bad,
                    result: WorkerResult {
                        schema: SCHEMA,
                        story: request.dispatch.packet.story.clone(),
                        task: request.dispatch.packet.task.spec.id.clone(),
                        task_revision: request.dispatch.packet.task.revision,
                        status: ResultStatus::Succeeded,
                        report: "mismatched correlation".to_owned(),
                        commit: None,
                        artifacts: Vec::new(),
                    },
                    worker_stopped: true,
                }
            }
            FakeBehavior::InvalidSessionId { session_id } => {
                let mut bad = correlation;
                bad.session_id = session_id;
                AdapterOutcome::Completed {
                    correlation: bad,
                    result: WorkerResult {
                        schema: SCHEMA,
                        story: request.dispatch.packet.story.clone(),
                        task: request.dispatch.packet.task.spec.id.clone(),
                        task_revision: request.dispatch.packet.task.revision,
                        status: ResultStatus::Succeeded,
                        report: "invalid session id".to_owned(),
                        commit: None,
                        artifacts: Vec::new(),
                    },
                    worker_stopped: true,
                }
            }
            FakeBehavior::StaleRevision => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story: request.dispatch.packet.story.clone(),
                    task: request.dispatch.packet.task.spec.id.clone(),
                    task_revision: request.dispatch.packet.task.revision.saturating_sub(1),
                    status: ResultStatus::Succeeded,
                    report: "stale revision".to_owned(),
                    commit: None,
                    artifacts: Vec::new(),
                },
                worker_stopped: true,
            },
            FakeBehavior::WrongTask { task } => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story: request.dispatch.packet.story.clone(),
                    task,
                    task_revision: request.dispatch.packet.task.revision,
                    status: ResultStatus::Succeeded,
                    report: "wrong task".to_owned(),
                    commit: None,
                    artifacts: Vec::new(),
                },
                worker_stopped: true,
            },
            FakeBehavior::WrongStory { story } => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story,
                    task: request.dispatch.packet.task.spec.id.clone(),
                    task_revision: request.dispatch.packet.task.revision,
                    status: ResultStatus::Succeeded,
                    report: "wrong story".to_owned(),
                    commit: None,
                    artifacts: Vec::new(),
                },
                worker_stopped: true,
            },
            FakeBehavior::UnstoppedCompletion => AdapterOutcome::Completed {
                correlation,
                result: WorkerResult {
                    schema: SCHEMA,
                    story: request.dispatch.packet.story.clone(),
                    task: request.dispatch.packet.task.spec.id.clone(),
                    task_revision: request.dispatch.packet.task.revision,
                    status: ResultStatus::Succeeded,
                    report: "unstopped worker".to_owned(),
                    commit: None,
                    artifacts: Vec::new(),
                },
                worker_stopped: false,
            },
            FakeBehavior::TimedOut {
                reason,
                worker_stopped,
            } => AdapterOutcome::TimedOut {
                correlation,
                worker_stopped,
                reason,
            },
            FakeBehavior::Cancelled {
                reason,
                worker_stopped,
            } => AdapterOutcome::Cancelled {
                correlation,
                worker_stopped,
                reason,
            },
            FakeBehavior::HostCrashed {
                exit_code,
                detail,
                worker_stopped,
            } => AdapterOutcome::HostCrashed {
                correlation,
                worker_stopped,
                exit_code,
                detail,
            },
            FakeBehavior::HostUnavailable { detail } => AdapterOutcome::HostUnavailable {
                provider: self.capabilities.provider.clone(),
                detail,
            },
            FakeBehavior::Custom(handler) => handler(request, cancel),
        }
    }
}
