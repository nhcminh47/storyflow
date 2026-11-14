use crate::{
    config,
    snapshot::RunSnapshot,
    tasks::{RunContext, TaskSpec},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

pub const MAX_FILE_BYTES: usize = 1_048_576;
const MAX_AUDIT_BYTES: u64 = 4_194_304;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxBackend {
    /// Reviewed project commands execute with the caller's OS identity.
    Trusted,
    /// Only mediated regular-file and pinned-blob operations; no worker process.
    ManagedOnly,
}
impl SandboxBackend {
    pub fn authorize_command(self) -> SecurityResult<()> {
        match self {
            Self::Trusted => Ok(()),
            Self::ManagedOnly => Err(error(
                "sandbox_command_unsupported",
                "managed-only isolation cannot execute commands",
            )),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SecurityError {
    pub code: &'static str,
    pub message: &'static str,
}
impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for SecurityError {}
pub type SecurityResult<T> = Result<T, SecurityError>;
pub fn error(code: &'static str, message: &'static str) -> SecurityError {
    SecurityError { code, message }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Read { path: String },
    Write { path: String, content: String },
    GitRead { path: String },
    Git { args: Vec<String> },
    Shell { program: String, args: Vec<String> },
}
impl Operation {
    fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::GitRead { .. } => "git_read",
            Self::Git { .. } => "git",
            Self::Shell { .. } => "shell",
        }
    }
}

/// Optional extra host leaf floors; execute always validates the pinned path.
/// A Gitlink/symlink is never a file scope.
#[derive(Clone, Debug)]
pub struct TreeBoundary {
    pub path: String,
    pub regular: bool,
}

/// Construct only from a verified parent journal and current task packet while
/// retaining both authoritative locks. Caller-created packets are not authority.
/// The companion executable is the authoritative admission path for workers.
pub struct RuntimeIdentity {
    pub attempt_id: String,
    pub graph_revision: u64,
    pub expires_at_ms: u64,
}

pub struct ManagedRuntime {
    root: PathBuf,
    snapshot: RunSnapshot,
    digest: String,
    context: RunContext,
    task: TaskSpec,
    boundaries: Vec<TreeBoundary>,
    identity: RuntimeIdentity,
}
impl ManagedRuntime {
    pub fn new(
        root: &Path,
        snapshot: &RunSnapshot,
        digest: &str,
        context: RunContext,
        task: TaskSpec,
        boundaries: Vec<TreeBoundary>,
        identity: RuntimeIdentity,
    ) -> SecurityResult<Self> {
        if !cfg!(any(target_os = "linux", windows)) {
            return Err(error(
                "unsupported_platform",
                "managed-only runtime supports Linux and Windows",
            ));
        }
        snapshot
            .verify(&context.story, &context.base, digest)
            .map_err(|_| {
                error(
                    "snapshot_integrity",
                    "pinned configuration verification failed",
                )
            })?;
        if !snapshot.managed_only {
            return Err(error(
                "managed_runtime_required",
                "initialize a new run with --managed-runtime",
            ));
        }
        if context.blocked || task.story != context.story || task.stage != context.stage {
            return Err(error("task_identity", "task is not current and active"));
        }
        context
            .validate()
            .map_err(|_| error("task_identity", "invalid task context"))?;
        for scope in task.read_scope.iter().chain(&task.write_scope) {
            valid_path(scope)?;
        }
        if task
            .write_scope
            .iter()
            .any(|p| !context.scope.iter().any(|s| under(p, s)))
        {
            return Err(error("path_denied", "task exceeds pinned story scope"));
        }
        if !root.is_absolute() {
            return Err(error("path_denied", "host root must be absolute"));
        }
        let root = std::fs::canonicalize(root)
            .map_err(|_| error("path_denied", "host root is unavailable"))?;
        Ok(Self {
            root,
            snapshot: snapshot.clone(),
            digest: digest.into(),
            context,
            task,
            boundaries,
            identity,
        })
    }
    fn capability(&self, name: &str) -> SecurityResult<()> {
        if !self.snapshot.effective.capabilities.granted.contains(name) {
            return Err(error(
                "capability_denied",
                "pinned configuration denies this capability",
            ));
        }
        Ok(())
    }
    fn path(&self, path: &str, write: bool) -> SecurityResult<()> {
        valid_path(path)?;
        let scopes = if write {
            &self.task.write_scope
        } else {
            &self.task.read_scope
        };
        if !scopes.iter().any(|s| under(path, s))
            || (write && !self.context.scope.iter().any(|s| under(path, s)))
        {
            return Err(error(
                "path_denied",
                "path is outside the allowed task scope",
            ));
        }
        let key = fold(path);
        if path.split('/').any(|p| fold(p) == ".git")
            || self
                .snapshot
                .effective
                .policy
                .protected
                .iter()
                .any(|p| under(&key, &fold(&p.value)))
            || config::BUILTIN_PROTECTED
                .iter()
                .any(|p| under(&key, &fold(p)))
        {
            return Err(error("path_denied", "path is protected"));
        }
        if self.boundaries.iter().any(|b| {
            let boundary = fold(&b.path);
            (key == boundary && (path != b.path || !b.regular))
                || (key != boundary && under(&key, &boundary))
        }) {
            return Err(error(
                "path_denied",
                "path crosses a pinned file or repository boundary",
            ));
        }
        Ok(())
    }
    pub fn authorize(&self, operation: &Operation) -> SecurityResult<()> {
        self.authorize_with_clock(operation, &crate::tasks::SystemClock)
    }
    fn authorize_with_clock(
        &self,
        operation: &Operation,
        clock: &dyn crate::tasks::Clock,
    ) -> SecurityResult<()> {
        if clock.now_ms() >= self.identity.expires_at_ms {
            return Err(error(
                "lease_denied",
                "managed lease expired before execution",
            ));
        }
        self.capability(match operation {
            Operation::Read { .. } => "read",
            Operation::Write { .. } => "write",
            Operation::GitRead { .. } | Operation::Git { .. } => "git",
            Operation::Shell { .. } => "shell",
        })?;
        match operation {
            Operation::Read { path } => self.path(path, false),
            Operation::Write { path, content } => {
                self.path(path, true)?;
                if content.len() > MAX_FILE_BYTES {
                    return Err(error(
                        "request_too_large",
                        "managed file exceeds byte limit",
                    ));
                }
                Ok(())
            }
            Operation::GitRead { path } => {
                self.capability("read")?;
                self.path(path, false)
            }
            Operation::Git { .. } | Operation::Shell { .. } => Err(error(
                "unsupported_managed_operation",
                "managed-only isolation refuses arbitrary Git and shell commands",
            )),
        }
    }
    /// Writes a bounded intent before the effect. No path, content, argument,
    /// environment value or owner token is copied into audit evidence.
    pub fn execute(
        &self,
        operation: &Operation,
        audit: &mut File,
    ) -> SecurityResult<serde_json::Value> {
        self.execute_with_clock(operation, audit, &crate::tasks::SystemClock)
    }
    fn execute_with_clock(
        &self,
        operation: &Operation,
        audit: &mut File,
        clock: &dyn crate::tasks::Clock,
    ) -> SecurityResult<serde_json::Value> {
        if audit
            .metadata()
            .map_err(|_| error("security_audit_io", "audit is unavailable"))?
            .len()
            > MAX_AUDIT_BYTES - 2048
        {
            return Err(error(
                "security_audit_full",
                "security audit capacity exhausted",
            ));
        }
        let authorization = self.authorize_with_clock(operation, clock);
        self.record(
            audit,
            operation,
            if authorization.is_ok() {
                "authorized"
            } else {
                "denied"
            },
            authorization.as_ref().err().map(|e| e.code),
        )?;
        authorization?;
        // Durable audit I/O can block; do not use a lease that expired while
        // publishing its intent to authorize the actual filesystem/Git effect.
        let result = if clock.now_ms() >= self.identity.expires_at_ms {
            Err(error(
                "lease_denied",
                "managed lease expired before execution",
            ))
        } else {
            self.execute_authorized(operation, clock)
        };
        self.record(
            audit,
            operation,
            if result.is_ok() {
                "completed"
            } else {
                "failed"
            },
            result.as_ref().err().map(|e| e.code),
        )?;
        result
    }
    // All callers pass capability/scope authorization and durable intent first.
    // Baseline checks are mandatory here, independent of caller-provided floors.
    fn execute_authorized(
        &self,
        operation: &Operation,
        clock: &dyn crate::tasks::Clock,
    ) -> SecurityResult<serde_json::Value> {
        let path = match operation {
            Operation::Read { path }
            | Operation::Write { path, .. }
            | Operation::GitRead { path } => path,
            Operation::Git { .. } | Operation::Shell { .. } => {
                unreachable!("unsupported operations cannot be authorized")
            }
        };
        let baseline = config::GitBaseline {
            root: self.root.clone(),
            commit: self.snapshot.base.clone(),
        };
        let origin = config::Origin::new(config::Scope::Repo, "pinned", "managed_path");
        let object = baseline.lookup_managed_blob(path, &origin).map_err(|e| {
            if e.code == "managed_namespace_limit" {
                error(
                    "namespace_limit",
                    "pinned namespace metadata limit exceeded",
                )
            } else {
                error("path_denied", "pinned namespace lookup refused")
            }
        })?;
        if clock.now_ms() >= self.identity.expires_at_ms {
            return Err(error(
                "lease_denied",
                "managed lease expired before execution",
            ));
        }
        match operation {
            Operation::Read { path } => super::files::read(&self.root, path, MAX_FILE_BYTES)
                .map_err(|_| error("path_denied", "confined file read refused"))
                .and_then(content),
            Operation::Write { path, content } => {
                super::files::write(&self.root, path, content.as_bytes())
                    .map_err(|_| error("path_denied", "confined file write refused"))
                    .map(|()| serde_json::json!({"written":content.len()}))
            }
            Operation::GitRead { .. } => baseline
                .read_blob_bounded(
                    object
                        .as_deref()
                        .ok_or_else(|| error("path_denied", "pinned file is absent"))?,
                    &origin,
                    MAX_FILE_BYTES,
                )
                .map_err(|_| error("path_denied", "pinned blob read refused"))
                .and_then(content),
            Operation::Git { .. } | Operation::Shell { .. } => {
                unreachable!("unsupported operations cannot be authorized")
            }
        }
    }
    fn record(
        &self,
        audit: &mut File,
        operation: &Operation,
        phase: &str,
        code: Option<&str>,
    ) -> SecurityResult<()> {
        let record = serde_json::json!({"schema":1,"snapshot":self.digest,"story":self.context.story,"task":self.task.id,"attempt":self.identity.attempt_id,"graph_revision":self.identity.graph_revision,"operation":operation.kind(),"phase":phase,"code":code});
        append_audit(audit, &record)
    }
}
fn content(bytes: Vec<u8>) -> SecurityResult<serde_json::Value> {
    let text =
        String::from_utf8(bytes).map_err(|_| error("invalid_utf8", "managed file is not UTF-8"))?;
    Ok(serde_json::json!({"content":text}))
}
fn valid_path(path: &str) -> SecurityResult<()> {
    if path.nfc().collect::<String>() != path {
        return Err(error(
            "path_denied",
            "canonical Unicode path spelling required",
        ));
    }
    config::validate_relative_path(
        path,
        &config::Origin::new(config::Scope::Repo, "request", "path"),
    )
    .map_err(|_| error("path_denied", "unsafe repository-relative path"))
}
fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('/'))
}
fn fold(path: &str) -> String {
    path.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfc()
        .collect()
}

/// The run lock must be held; worker input never selects the audit path.
pub fn open_audit(run_dir: &Path) -> SecurityResult<File> {
    let path = run_dir.join("security-events.jsonl");
    if let Ok(meta) = std::fs::symlink_metadata(&path)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err(error("security_audit_io", "audit is not a regular file"));
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|_| error("security_audit_io", "audit is unavailable"))
}

/// Sanitized failure evidence for a request admitted to a verified story. Never
/// copy a caller's task/owner/token/path into an admission failure record.
pub fn audit_rejection(
    audit: &mut File,
    snapshot_digest: &str,
    code: &'static str,
) -> SecurityResult<()> {
    append_audit(
        audit,
        &serde_json::json!({"schema":1,"snapshot":snapshot_digest,"phase":"request_rejected","code":code}),
    )
}
fn append_audit(audit: &mut File, record: &serde_json::Value) -> SecurityResult<()> {
    let mut bytes = serde_json::to_vec(record)
        .map_err(|_| error("security_audit_io", "audit encoding failed"))?;
    bytes.push(b'\n');
    let length = audit
        .metadata()
        .map_err(|_| error("security_audit_io", "audit unavailable"))?
        .len();
    if bytes.len() > 1024 || length > MAX_AUDIT_BYTES.saturating_sub(bytes.len() as u64) {
        return Err(error(
            "security_audit_full",
            "security audit capacity exhausted",
        ));
    }
    audit
        .write_all(&bytes)
        .and_then(|()| audit.sync_all())
        .map_err(|_| error("security_audit_io", "audit publication failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EffectiveCapabilities, EffectiveConfig, EffectivePolicy, Origin, Sourced};
    use crate::tasks::{Clock, TaskStage};
    use std::{cell::Cell, fs};
    struct AuditDeadline(Cell<u64>);
    impl Clock for AuditDeadline {
        fn now_ms(&self) -> u64 {
            let value = self.0.get();
            self.0.set(10);
            value
        }
    }
    #[test]
    fn lease_expiring_during_intent_publication_cannot_write() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/runtime-expiry-test")
            .join(std::process::id().to_string());
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/value"), "original").unwrap();
        let context = RunContext {
            story: "expiry".into(),
            base: "a".repeat(40),
            story_revision: 0,
            stage: TaskStage::Plan,
            blocked: false,
            scope: vec!["app".into()],
            protected: vec![],
        };
        let origin = Origin {
            scope: None,
            source: "host".into(),
            field: "test".into(),
        };
        let snapshot = RunSnapshot {
            schema: 1,
            story: context.story.clone(),
            base: context.base.clone(),
            managed_only: true,
            effective: EffectiveConfig {
                scopes: vec![],
                models: Default::default(),
                agents: Default::default(),
                policy: EffectivePolicy {
                    protected: vec![],
                    checks: vec![],
                    builtin_scope_gate: Sourced {
                        value: true,
                        origin,
                    },
                },
                capabilities: EffectiveCapabilities {
                    granted: ["write".into()].into(),
                    provenance: Default::default(),
                    constraints: vec![],
                },
            },
        };
        // Admission is covered by CLI tests; construct internal state here to
        // isolate the effect/intent ordering under the existing Clock contract.
        let runtime = ManagedRuntime {
            root: root.clone(),
            snapshot,
            digest: "b".repeat(64),
            context,
            boundaries: vec![],
            identity: RuntimeIdentity {
                attempt_id: "attempt".into(),
                graph_revision: 0,
                expires_at_ms: 10,
            },
            task: TaskSpec {
                id: "worker".into(),
                story: "expiry".into(),
                prerequisites: vec![],
                stage: TaskStage::Plan,
                read_scope: vec![],
                write_scope: vec!["app".into()],
                acceptance_criteria: vec!["AC1".into()],
                decision_owner: "host".into(),
            },
        };
        let mut audit = open_audit(&root).unwrap();
        let result = runtime.execute_with_clock(
            &Operation::Write {
                path: "app/value".into(),
                content: "changed".into(),
            },
            &mut audit,
            &AuditDeadline(Cell::new(0)),
        );
        assert_eq!(result.unwrap_err().code, "lease_denied");
        assert_eq!(
            fs::read_to_string(root.join("app/value")).unwrap(),
            "original"
        );
        drop(audit);
        let records = fs::read_to_string(root.join("security-events.jsonl")).unwrap();
        assert!(records.contains("authorized"));
        assert!(records.contains("failed"));
        assert!(records.contains("lease_denied"));
        fs::remove_dir_all(root).unwrap();
    }
}
