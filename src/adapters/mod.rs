//! Provider-neutral host adapter protocol and concrete host implementations.
//!
//! Adapters translate Rust scheduler `DispatchPacket`s into host executions and
//! return typed `AdapterOutcome`s. Adapters never mutate story state or bypass
//! Rust policy/quality gates; `runner` reconciles outcomes against `TaskJournal`.
pub mod codex;
pub mod fake;
pub mod runner;

use crate::tasks::{DispatchPacket, LeaseToken, MAX_REPORT_BYTES, SCHEMA, WorkerResult};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const ADAPTER_SCHEMA: u32 = 1;
pub const MAX_SESSION_ID_BYTES: usize = 128;
pub const MAX_DETAIL_BYTES: usize = MAX_REPORT_BYTES;
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;

pub type AdapterResult<T> = Result<T, AdapterError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterError {
    pub code: &'static str,
    pub detail: String,
}

impl AdapterError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for AdapterError {}

/// Execution sandbox mode requested by the orchestrator.
/// Intentionally excludes unapproved full-access, auto-merge, or auto-push modes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPolicy {
    ReadOnly,
    WorkspaceWrite,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilities {
    pub schema: u32,
    pub provider: String,
    pub available: bool,
    pub version: Option<String>,
    pub executable: Option<String>,
    pub supports_json_output: bool,
    pub supports_output_schema: bool,
    pub supports_output_file: bool,
    pub supports_working_dir: bool,
    pub supports_sandbox_flag: bool,
    pub supports_cancel: bool,
    pub supports_session_id: bool,
    pub supports_stdin_prompt: bool,
    #[serde(default)]
    pub detected_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

impl HostCapabilities {
    pub fn unavailable(provider: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            schema: ADAPTER_SCHEMA,
            provider: provider.into(),
            available: false,
            version: None,
            executable: None,
            supports_json_output: false,
            supports_output_schema: false,
            supports_output_file: false,
            supports_working_dir: false,
            supports_sandbox_flag: false,
            supports_cancel: false,
            supports_session_id: false,
            supports_stdin_prompt: false,
            detected_flags: Vec::new(),
            unavailable_reason: Some(sanitize_reason(&reason.into())),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdapterRequest {
    pub schema: u32,
    pub dispatch: DispatchPacket,
    pub workdir: PathBuf,
    pub timeout_ms: u64,
    pub policy: ExecutionPolicy,
}

impl AdapterRequest {
    pub fn validate(&self) -> AdapterResult<()> {
        if self.schema != ADAPTER_SCHEMA || self.dispatch.schema != SCHEMA {
            return Err(AdapterError::new(
                "adapter_schema",
                "unsupported adapter or dispatch packet schema",
            ));
        }
        if self.timeout_ms == 0 || self.timeout_ms > MAX_TIMEOUT_MS {
            return Err(AdapterError::new(
                "invalid_timeout",
                "timeout_ms must be within 1..=3600000",
            ));
        }
        if !self.workdir.is_dir() {
            return Err(AdapterError::new(
                "invalid_workdir",
                "adapter workdir does not exist or is not a directory",
            ));
        }
        valid_bounded_text(&self.dispatch.lease.attempt_id, 128, "invalid_lease")?;
        valid_bounded_text(&self.dispatch.lease.lease_id, 128, "invalid_lease")?;
        valid_bounded_text(&self.dispatch.lease.owner, 128, "invalid_lease")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionCorrelation {
    pub attempt_id: String,
    pub lease_id: String,
    pub owner: String,
    pub session_id: String,
}

impl SessionCorrelation {
    pub fn from_lease(lease: &LeaseToken, session_id: impl Into<String>) -> Self {
        Self {
            attempt_id: lease.attempt_id.clone(),
            lease_id: lease.lease_id.clone(),
            owner: lease.owner.clone(),
            session_id: session_id.into(),
        }
    }

    pub fn validate(&self) -> AdapterResult<()> {
        valid_bounded_text(&self.attempt_id, 128, "invalid_correlation")?;
        valid_bounded_text(&self.lease_id, 128, "invalid_correlation")?;
        valid_bounded_text(&self.owner, 128, "invalid_correlation")?;
        valid_bounded_text(&self.session_id, MAX_SESSION_ID_BYTES, "invalid_session_id")
    }

    pub fn matches_lease(&self, lease: &LeaseToken) -> bool {
        self.attempt_id == lease.attempt_id
            && self.lease_id == lease.lease_id
            && self.owner == lease.owner
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterOutcome {
    Completed {
        correlation: SessionCorrelation,
        result: WorkerResult,
        worker_stopped: bool,
    },
    Cancelled {
        correlation: SessionCorrelation,
        worker_stopped: bool,
        reason: String,
    },
    TimedOut {
        correlation: SessionCorrelation,
        worker_stopped: bool,
        reason: String,
    },
    HostCrashed {
        correlation: SessionCorrelation,
        worker_stopped: bool,
        exit_code: Option<i32>,
        detail: String,
    },
    HostUnavailable {
        provider: String,
        detail: String,
    },
    InvalidOutput {
        correlation: SessionCorrelation,
        worker_stopped: bool,
        detail: String,
    },
}

impl AdapterOutcome {
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Completed { correlation, .. }
            | Self::Cancelled { correlation, .. }
            | Self::TimedOut { correlation, .. }
            | Self::HostCrashed { correlation, .. }
            | Self::InvalidOutput { correlation, .. } => Some(&correlation.session_id),
            Self::HostUnavailable { .. } => None,
        }
    }

    pub fn worker_stopped(&self) -> bool {
        match self {
            Self::Completed { worker_stopped, .. }
            | Self::Cancelled { worker_stopped, .. }
            | Self::TimedOut { worker_stopped, .. }
            | Self::HostCrashed { worker_stopped, .. }
            | Self::InvalidOutput { worker_stopped, .. } => *worker_stopped,
            Self::HostUnavailable { .. } => true,
        }
    }
}

/// Cooperative cancellation signal shared between the host orchestrator and an adapter.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub trait HostAdapter {
    fn provider(&self) -> &str;
    fn probe(&self) -> HostCapabilities;
    fn execute(&self, request: &AdapterRequest, cancel: &CancelToken) -> AdapterOutcome;
}

/// Sanitize a diagnostic reason/detail so it satisfies `TaskJournal` text rules
/// (non-blank, no control characters, bounded to `MAX_DETAIL_BYTES`).
pub fn sanitize_reason(input: &str) -> String {
    let mut cleaned = String::with_capacity(input.len().min(MAX_DETAIL_BYTES));
    for ch in input.chars() {
        let mapped = if ch.is_control() { ' ' } else { ch };
        if cleaned.len() + mapped.len_utf8() > MAX_DETAIL_BYTES {
            break;
        }
        cleaned.push(mapped);
    }
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "host adapter reported unspecified reason".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn valid_bounded_text(value: &str, max_bytes: usize, code: &'static str) -> AdapterResult<()> {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(AdapterError::new(
            code,
            "value must be non-blank, bounded, and free of control characters",
        ));
    }
    Ok(())
}

pub use codex::{CodexAdapter, CodexAdapterConfig};
pub use fake::{FakeAdapter, FakeBehavior};
pub use runner::{ReconcileReport, ReconcileStatus, dispatch_and_run, reconcile_outcome};
