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
    pub fn correlation(&self) -> Option<&SessionCorrelation> {
        match self {
            Self::Completed { correlation, .. }
            | Self::Cancelled { correlation, .. }
            | Self::TimedOut { correlation, .. }
            | Self::HostCrashed { correlation, .. }
            | Self::InvalidOutput { correlation, .. } => Some(correlation),
            Self::HostUnavailable { .. } => None,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.correlation().map(|c| c.session_id.as_str())
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

/// Redact configured secret values and common credential patterns (`Bearer ...`,
/// provider API keys, and `*_KEY`/`*_TOKEN`/`PASSWORD` assignments) from untrusted
/// host text before returning reports or persisting diagnostics.
pub fn redact_secrets(input: &str, secret_values: &[&str]) -> String {
    let mut secrets: Vec<&str> = secret_values
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect();
    secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    secrets.dedup();

    let after_literals = if secrets.is_empty() {
        input.to_owned()
    } else {
        let mut out = String::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            if let Some(matched) = secrets.iter().find(|s| input[i..].starts_with(**s)) {
                out.push_str("[REDACTED]");
                i += matched.len();
            } else {
                let ch = input[i..].chars().next().expect("non-empty tail");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    };

    redact_patterns(&after_literals)
}

fn is_cred_value_char(b: u8) -> bool {
    !b.is_ascii_whitespace() && !matches!(b, b'"' | b'\'' | b',' | b';' | b')' | b']' | b'}')
}

fn is_prefixed_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn redact_patterns(input: &str) -> String {
    const ASSIGN_KEYS: [&str; 6] = [
        "OPENAI_API_KEY=",
        "API_KEY=",
        "ACCESS_TOKEN=",
        "AUTH_TOKEN=",
        "SECRET_KEY=",
        "PASSWORD=",
    ];
    const TOKEN_PREFIXES: [&str; 10] = [
        "github_pat_",
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "xoxb-",
        "xoxp-",
        "AIza",
        "AKIA",
    ];

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        let prev_is_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let tail = &input[i..];

        if !prev_is_word
            && tail
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
        {
            let tok_start = i + 7;
            let mut end = tok_start;
            while end < bytes.len() && is_cred_value_char(bytes[end]) {
                end += 1;
            }
            if end - tok_start >= 8 && !input[tok_start..end].starts_with("[REDACTED]") {
                out.push_str("Bearer [REDACTED]");
                i = end;
                continue;
            }
        }

        if !prev_is_word
            && let Some(key) = ASSIGN_KEYS.iter().find(|k| {
                tail.get(..k.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(k))
            })
        {
            let val_start = i + key.len();
            let (quote_prefix, actual_start) = match bytes.get(val_start) {
                Some(b'"') => ("\"", val_start + 1),
                Some(b'\'') => ("'", val_start + 1),
                _ => ("", val_start),
            };
            let mut end = actual_start;
            while end < bytes.len() && is_cred_value_char(bytes[end]) {
                end += 1;
            }
            if end > actual_start && !input[actual_start..end].starts_with("[REDACTED]") {
                out.push_str(&input[i..val_start]);
                out.push_str(quote_prefix);
                out.push_str("[REDACTED]");
                i = end;
                continue;
            }
        }

        let prev_breaks_token = i == 0
            || !(bytes[i - 1].is_ascii_alphanumeric() || matches!(bytes[i - 1], b'_' | b'-'));
        if prev_breaks_token
            && let Some(_prefix) = TOKEN_PREFIXES.iter().find(|p| tail.starts_with(**p))
        {
            let mut end = i;
            while end < bytes.len() && is_prefixed_token_char(bytes[end]) {
                end += 1;
            }
            while end > i && bytes[end - 1] == b'.' {
                end -= 1;
            }
            if end - i >= 12 {
                out.push_str("[REDACTED]");
                i = end;
                continue;
            }
        }

        let ch = tail.chars().next().expect("non-empty tail");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

/// Redact configured secret values and credential patterns from a worker report.
/// Preserves oversized inputs so `WorkerResult::validate()` still rejects them;
/// if redaction expands a valid-sized report past `MAX_REPORT_BYTES`, truncates
/// cleanly on a UTF-8 char boundary.
pub(crate) fn redact_report(input: &str, secret_values: &[&str]) -> String {
    let redacted = redact_secrets(input, secret_values);
    if input.len() <= MAX_REPORT_BYTES && redacted.len() > MAX_REPORT_BYTES {
        let mut end = MAX_REPORT_BYTES;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        return redacted[..end].to_owned();
    }
    redacted
}

/// Sanitize a diagnostic reason/detail so it satisfies `TaskJournal` text rules
/// (non-blank, redacted, no control characters, bounded to `MAX_DETAIL_BYTES`).
pub fn sanitize_reason(input: &str) -> String {
    sanitize_reason_with_secrets(input, &[])
}

/// Like `sanitize_reason`, while also redacting configured literal secret values.
pub fn sanitize_reason_with_secrets(input: &str, secret_values: &[&str]) -> String {
    let redacted = redact_secrets(input, secret_values);
    let mut cleaned = String::with_capacity(redacted.len().min(MAX_DETAIL_BYTES));
    for ch in redacted.chars() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secrets_scrubs_configured_and_pattern_credentials() {
        let input = "using opaque-secret-xyz and sk-proj-1234567890abcdef \
                     with Bearer super_secret_token_99 and OPENAI_API_KEY=\"rawval123\" \
                     keeping src/adapters/codex.rs intact";
        let redacted = redact_secrets(input, &["opaque-secret", "opaque-secret-xyz"]);
        assert!(!redacted.contains("opaque-secret"), "{redacted}");
        assert!(!redacted.contains("sk-proj-1234567890abcdef"), "{redacted}");
        assert!(!redacted.contains("super_secret_token_99"), "{redacted}");
        assert!(!redacted.contains("rawval123"), "{redacted}");
        assert!(redacted.contains("src/adapters/codex.rs"), "{redacted}");
        assert!(redacted.contains("[REDACTED]"), "{redacted}");
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
