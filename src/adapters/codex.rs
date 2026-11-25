//! Codex host adapter using capability probing and supervised subprocess execution.
use super::{
    ADAPTER_SCHEMA, AdapterError, AdapterOutcome, AdapterRequest, AdapterResult, CancelToken,
    ExecutionPolicy, HostAdapter, HostCapabilities, MAX_SESSION_ID_BYTES, SessionCorrelation,
    sanitize_reason,
};
use crate::{
    config::BUILTIN_PROTECTED,
    security::process::{
        CommandSpec, ControlledResult, HostCommandSpec, ProcessError, git_executable,
        host_executable, run as run_process, run_host_command,
    },
    tasks::{Artifact, ResultStatus, SCHEMA, WorkerResult},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, hash_map::RandomState},
    fs::{self, DirBuilder, OpenOptions},
    hash::{BuildHasher, Hasher},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use unicode_normalization::UnicodeNormalization;

const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 5;
const MAX_PROMPT_BYTES: usize = 65_536;
const MAX_BACKUP_BYTES: usize = 32 * 1024 * 1024;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const FORBIDDEN_FLAGS: &[&str] = &[
    "--yolo",
    "--dangerously-bypass-approvals-and-sandbox",
    "--full-auto",
    "--ask-for-approval",
    "-a",
    "--sandbox",
    "-s",
    "danger-full-access",
    "--cd",
    "-C",
    "--add-dir",
    "--config",
    "-c",
    "--profile",
    "-p",
    "--enable",
    "--disable",
    "--json",
    "--output-schema",
    "--output-last-message",
    "-o",
    "--auto-merge",
    "--push",
];

const FORBIDDEN_SHORT_PREFIXES: &[&str] = &["-a", "-s", "-c", "-C", "-o", "-p"];

pub trait CodexCommandRunner: Send + Sync {
    fn resolve_executable(&self, configured: Option<&Path>) -> Result<PathBuf, String>;
    fn run(
        &self,
        spec: &HostCommandSpec,
        cwd: &Path,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<ControlledResult, ProcessError>;

    /// Returns `true` only if the runner's OS containment guarantees that no
    /// detached descendant process can survive after termination is reported.
    fn has_unbroken_containment(&self) -> bool {
        false
    }

    /// Returns `true` only if the runner strictly restricts filesystem reads
    /// to `staging_dir` (refusing full-filesystem reads outside `read_scope`).
    fn has_read_root_confinement(&self) -> bool {
        false
    }

    /// Returns `true` if the host's ambient Codex config defines legacy
    /// `sandbox_mode` or `sandbox_workspace_write` keys that override `default_permissions`.
    fn ambient_config_sets_legacy_sandbox(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct ProcessCommandRunner;

impl CodexCommandRunner for ProcessCommandRunner {
    fn resolve_executable(&self, configured: Option<&Path>) -> Result<PathBuf, String> {
        let path = match configured {
            Some(path) => {
                if is_windows_batch(path) {
                    return Err("implicit batch wrapper refused for codex executable".to_owned());
                }
                if !path.is_file() {
                    return Err(format!(
                        "configured codex executable not found: {}",
                        path.display()
                    ));
                }
                fs::canonicalize(path).map_err(|e| {
                    format!(
                        "failed to resolve configured codex executable {}: {e}",
                        path.display()
                    )
                })?
            }
            None => {
                host_executable("codex").map_err(|e| format!("codex not found on PATH: {e}"))?
            }
        };
        if is_windows_batch(&path) {
            return Err("implicit batch wrapper refused for codex executable".to_owned());
        }
        Ok(path)
    }

    fn run(
        &self,
        spec: &HostCommandSpec,
        cwd: &Path,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<ControlledResult, ProcessError> {
        run_host_command(spec, cwd, should_stop)
    }

    fn has_unbroken_containment(&self) -> bool {
        // Windows Job Objects track all descendants regardless of session/group changes;
        // Unix process-group cleanup can be escaped via setsid().
        cfg!(windows)
    }

    fn has_read_root_confinement(&self) -> bool {
        // Legacy `codex exec --sandbox read-only|workspace-write` grants full-filesystem
        // read access and cannot isolate omitted files in the workspace.
        false
    }

    fn ambient_config_sets_legacy_sandbox(&self) -> bool {
        ambient_codex_config_paths()
            .into_iter()
            .any(|path| match fs::read_to_string(&path) {
                Ok(text) => config_toml_sets_legacy_sandbox(&text),
                Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true,
            })
    }
}

#[derive(Clone, Debug)]
pub struct CodexAdapterConfig {
    pub executable: Option<PathBuf>,
    pub probe_timeout: Duration,
    pub passthrough_env: Vec<String>,
    pub extra_args: Vec<String>,
    pub max_backup_bytes: usize,
}

impl Default for CodexAdapterConfig {
    fn default() -> Self {
        Self {
            executable: None,
            probe_timeout: Duration::from_secs(DEFAULT_PROBE_TIMEOUT_SECS),
            passthrough_env: [
                "HOME",
                "USERPROFILE",
                "CODEX_HOME",
                "XDG_CONFIG_HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "OPENAI_API_KEY",
                "OPENAI_BASE_URL",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            extra_args: Vec::new(),
            max_backup_bytes: MAX_BACKUP_BYTES,
        }
    }
}

impl CodexAdapterConfig {
    pub fn validate(&self) -> AdapterResult<()> {
        if self.probe_timeout.is_zero() || self.probe_timeout > Duration::from_secs(60) {
            return Err(AdapterError::new(
                "invalid_config",
                "probe_timeout must be within 1ms..=60s",
            ));
        }
        for key in &self.passthrough_env {
            if key.is_empty() || key.contains(['=', '\0']) {
                return Err(AdapterError::new(
                    "invalid_config",
                    "invalid passthrough environment variable name",
                ));
            }
        }
        for arg in &self.extra_args {
            if arg.is_empty() || arg.chars().any(char::is_control) {
                return Err(AdapterError::new(
                    "invalid_config",
                    "extra_args cannot be empty or contain control characters",
                ));
            }
            let flag_name = arg.split('=').next().unwrap_or(arg.as_str());
            let lower = arg.to_ascii_lowercase();
            let short_prefix_hit = !arg.starts_with("--")
                && FORBIDDEN_SHORT_PREFIXES.iter().any(|p| arg.starts_with(p));
            if FORBIDDEN_FLAGS
                .iter()
                .any(|f| flag_name.eq_ignore_ascii_case(f) || arg.eq_ignore_ascii_case(f))
                || short_prefix_hit
                || lower.contains("writable_roots")
                || lower.contains("sandbox_mode")
            {
                return Err(AdapterError::new(
                    "forbidden_host_flag",
                    format!("refusing high-risk or reserved codex flag: {flag_name}"),
                ));
            }
        }
        Ok(())
    }
}

pub struct CodexAdapter {
    config: CodexAdapterConfig,
    runner: Arc<dyn CodexCommandRunner>,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self::with_config(CodexAdapterConfig::default())
    }

    pub fn with_config(config: CodexAdapterConfig) -> Self {
        Self {
            config,
            runner: Arc::new(ProcessCommandRunner),
        }
    }

    pub fn with_runner(config: CodexAdapterConfig, runner: Arc<dyn CodexCommandRunner>) -> Self {
        Self { config, runner }
    }
}

impl HostAdapter for CodexAdapter {
    fn provider(&self) -> &str {
        "codex"
    }

    fn probe(&self) -> HostCapabilities {
        if let Err(err) = self.config.validate() {
            return HostCapabilities::unavailable("codex", err.to_string());
        }
        let exe = match self
            .runner
            .resolve_executable(self.config.executable.as_deref())
        {
            Ok(path) => path,
            Err(reason) => return HostCapabilities::unavailable("codex", reason),
        };
        let cwd = std::env::temp_dir();
        let exe_str = exe.to_string_lossy().into_owned();

        let version_spec = HostCommandSpec {
            program: exe_str.clone(),
            args: vec!["--version".to_owned()],
            timeout: self.config.probe_timeout,
            stdin: None,
            passthrough_env: self.config.passthrough_env.clone(),
        };
        let version = match self.runner.run(&version_spec, &cwd, &|| false) {
            Ok(res)
                if !res.cancelled
                    && !res.process.timed_out
                    && res.process.status.is_some_and(|s| s.success()) =>
            {
                let text = String::from_utf8_lossy(&res.process.stdout)
                    .trim()
                    .to_owned();
                if text.is_empty() { None } else { Some(text) }
            }
            Ok(_) => None,
            Err(err) => {
                return HostCapabilities::unavailable(
                    "codex",
                    format!("failed to probe codex --version: {err}"),
                );
            }
        };

        let help_spec = HostCommandSpec {
            program: exe_str.clone(),
            args: vec!["exec".to_owned(), "--help".to_owned()],
            timeout: self.config.probe_timeout,
            stdin: None,
            passthrough_env: self.config.passthrough_env.clone(),
        };
        let help_text = match self.runner.run(&help_spec, &cwd, &|| false) {
            Ok(res)
                if !res.cancelled
                    && !res.process.timed_out
                    && res.process.status.is_some_and(|s| s.success()) =>
            {
                let mut combined = String::from_utf8_lossy(&res.process.stdout).into_owned();
                if !res.process.stderr.is_empty() {
                    combined.push('\n');
                    combined.push_str(&String::from_utf8_lossy(&res.process.stderr));
                }
                combined
            }
            Ok(res) => {
                let code = res.process.status.and_then(|s| s.code());
                return HostCapabilities::unavailable(
                    "codex",
                    format!("codex exec --help failed (exit {code:?})"),
                );
            }
            Err(err) => {
                return HostCapabilities::unavailable(
                    "codex",
                    format!("failed to probe codex exec --help: {err}"),
                );
            }
        };

        parse_exec_help(
            &exe_str,
            version,
            &help_text,
            self.runner.has_unbroken_containment(),
        )
    }

    fn execute(&self, request: &AdapterRequest, cancel: &CancelToken) -> AdapterOutcome {
        if let Err(err) = self.config.validate() {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail: err.to_string(),
            };
        }
        let fallback_correlation = SessionCorrelation::from_lease(
            &request.dispatch.lease,
            format!("codex:{}", request.dispatch.lease.attempt_id),
        );
        if let Err(err) = request.validate() {
            return AdapterOutcome::InvalidOutput {
                correlation: fallback_correlation,
                worker_stopped: true,
                detail: err.to_string(),
            };
        }

        let caps = self.probe();
        if !caps.available || !caps.supports_sandbox_flag {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail: caps.unavailable_reason.unwrap_or_else(|| {
                    "codex host cannot enforce required '--sandbox' policy".to_owned()
                }),
            };
        }

        if request.policy == ExecutionPolicy::WorkspaceWrite
            && let Err(detail) = ensure_isolated_workspace(
                &request.workdir,
                &request.dispatch.packet.task.spec.write_scope,
            )
        {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail,
            };
        }

        if cancel.is_cancelled() {
            return AdapterOutcome::Cancelled {
                correlation: fallback_correlation,
                worker_stopped: true,
                reason: "cancelled before launching codex".to_owned(),
            };
        }

        let use_config_confinement =
            !self.runner.has_read_root_confinement() && supports_config_read_confinement(&caps);

        if use_config_confinement && self.runner.ambient_config_sets_legacy_sandbox() {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail: "ambient codex config.toml sets legacy sandbox_mode or sandbox_workspace_write, which deactivates scoped default_permissions confinement".to_owned(),
            };
        }

        if !self.runner.has_read_root_confinement() && !use_config_confinement {
            match find_unscoped_workdir_entries(
                &request.workdir,
                &request.dispatch.packet.task.spec.read_scope,
            ) {
                Ok(unscoped) if !unscoped.is_empty() => {
                    return AdapterOutcome::HostUnavailable {
                        provider: "codex".to_owned(),
                        detail: sanitize_reason(&format!(
                            "codex --sandbox does not restrict filesystem reads and cannot isolate unscoped or protected paths in workdir: {}",
                            unscoped.join(", ")
                        )),
                    };
                }
                Ok(_) => {}
                Err(err) => {
                    return AdapterOutcome::HostCrashed {
                        correlation: fallback_correlation,
                        worker_stopped: true,
                        exit_code: None,
                        detail: sanitize_reason(&format!(
                            "failed to verify codex read_scope isolation in workdir: {err}"
                        )),
                    };
                }
            }
        }

        let prompt = match build_prompt(request) {
            Ok(prompt) => prompt,
            Err(err) => {
                return AdapterOutcome::InvalidOutput {
                    correlation: fallback_correlation,
                    worker_stopped: true,
                    detail: err.to_string(),
                };
            }
        };

        let exe = match caps.executable.as_deref() {
            Some(path) => path.to_owned(),
            None => {
                return AdapterOutcome::HostUnavailable {
                    provider: "codex".to_owned(),
                    detail: "probed codex executable path missing".to_owned(),
                };
            }
        };

        let staging_dir = match TempArtifactsDir::new() {
            Ok(dir) => dir,
            Err(err) => {
                return AdapterOutcome::HostCrashed {
                    correlation: fallback_correlation,
                    worker_stopped: true,
                    exit_code: None,
                    detail: format!("failed to create codex staging directory: {err}"),
                };
            }
        };
        if let Err(err) = stage_read_scope(
            &request.workdir,
            staging_dir.path(),
            &request.dispatch.packet.task.spec.read_scope,
            self.config.max_backup_bytes,
        ) {
            return match err {
                StageError::BudgetExceeded => AdapterOutcome::HostUnavailable {
                    provider: "codex".to_owned(),
                    detail: format!(
                        "read_scope exceeds {} byte rollback backup limit for staged execution; narrow task read_scope",
                        self.config.max_backup_bytes
                    ),
                },
                StageError::SymlinkOrSpecial(detail) => AdapterOutcome::HostUnavailable {
                    provider: "codex".to_owned(),
                    detail: sanitize_reason(&detail),
                },
                StageError::Io(detail) => AdapterOutcome::HostCrashed {
                    correlation: fallback_correlation,
                    worker_stopped: true,
                    exit_code: None,
                    detail: sanitize_reason(&format!(
                        "failed to stage codex read_scope workspace: {detail}"
                    )),
                },
            };
        }

        let before_snapshot =
            match snapshot_workspace(staging_dir.path(), true, self.config.max_backup_bytes) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    return AdapterOutcome::HostCrashed {
                        correlation: fallback_correlation,
                        worker_stopped: true,
                        exit_code: None,
                        detail: sanitize_reason(&format!(
                            "failed to snapshot codex staging workspace before execution: {err}"
                        )),
                    };
                }
            };
        if request.policy == ExecutionPolicy::WorkspaceWrite && before_snapshot.incomplete_backup {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail: format!(
                    "workspace exceeds {} byte rollback backup limit for WorkspaceWrite; use a clean isolated worktree",
                    self.config.max_backup_bytes
                ),
            };
        }

        let temp_dir = match TempArtifactsDir::new() {
            Ok(dir) => dir,
            Err(err) => {
                return AdapterOutcome::HostCrashed {
                    correlation: fallback_correlation,
                    worker_stopped: true,
                    exit_code: None,
                    detail: format!("failed to create codex temp directory: {err}"),
                };
            }
        };

        let schema_path = temp_dir.path().join("result-schema.json");
        let output_path = temp_dir.path().join("last-message.json");
        let staged_codex_cfg = staging_dir.path().join(".codex").join("config.toml");
        if use_config_confinement
            && match fs::read_to_string(&staged_codex_cfg) {
                Ok(text) => config_toml_sets_legacy_sandbox(&text),
                Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true,
            }
        {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail: "staged .codex/config.toml sets legacy sandbox or colliding profile keys"
                    .to_owned(),
            };
        }
        if caps.supports_output_schema
            && let Err(err) = write_exclusive_file(&schema_path, output_schema_json().as_bytes())
        {
            return AdapterOutcome::HostCrashed {
                correlation: fallback_correlation,
                worker_stopped: true,
                exit_code: None,
                detail: format!("failed to write codex output schema: {err}"),
            };
        }

        let profile_name = scoped_profile_name(&request.dispatch.lease.attempt_id);
        let mut args = vec!["exec".to_owned()];
        if caps.supports_working_dir {
            args.push("--cd".to_owned());
            args.push(staging_dir.path().to_string_lossy().into_owned());
        }
        if use_config_confinement {
            append_config_read_confinement(
                &mut args,
                &profile_name,
                staging_dir.path(),
                &request.workdir,
                request.policy,
            );
        } else {
            args.push("--sandbox".to_owned());
            args.push(match request.policy {
                ExecutionPolicy::ReadOnly => "read-only".to_owned(),
                ExecutionPolicy::WorkspaceWrite => "workspace-write".to_owned(),
            });
        }
        if caps.supports_json_output {
            args.push("--json".to_owned());
        }
        if caps.supports_output_schema {
            args.push("--output-schema".to_owned());
            args.push(schema_path.to_string_lossy().into_owned());
        }
        if caps.supports_output_file {
            args.push("--output-last-message".to_owned());
            args.push(output_path.to_string_lossy().into_owned());
        }
        if caps.detected_flags.iter().any(|f| f == "--ephemeral") {
            args.push("--ephemeral".to_owned());
        }
        args.extend(self.config.extra_args.iter().cloned());

        let stdin = if caps.supports_stdin_prompt {
            args.push("-".to_owned());
            Some(prompt.into_bytes())
        } else {
            args.push(prompt);
            None
        };

        let spec = HostCommandSpec {
            program: exe,
            args,
            timeout: Duration::from_millis(request.timeout_ms),
            stdin,
            passthrough_env: self.config.passthrough_env.clone(),
        };

        let controlled = match self
            .runner
            .run(&spec, staging_dir.path(), &|| cancel.is_cancelled())
        {
            Ok(result) => result,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return AdapterOutcome::HostUnavailable {
                    provider: "codex".to_owned(),
                    detail: format!("codex executable unavailable: {err}"),
                };
            }
            Err(err) => {
                return AdapterOutcome::HostCrashed {
                    correlation: fallback_correlation,
                    worker_stopped: false,
                    exit_code: None,
                    detail: sanitize_reason(&format!(
                        "codex supervision failed (worker exit unconfirmed): {err}"
                    )),
                };
            }
        };

        let worker_stopped = self.runner.has_unbroken_containment();
        let stdout_text = String::from_utf8_lossy(&controlled.process.stdout);
        let stderr_text = String::from_utf8_lossy(&controlled.process.stderr);
        let session_id = extract_session_id(&stdout_text)
            .unwrap_or_else(|| fallback_correlation.session_id.clone());
        let correlation = SessionCorrelation::from_lease(&request.dispatch.lease, session_id);

        let scope_check = verify_and_restore_post_run(
            staging_dir.path(),
            &before_snapshot,
            request.policy,
            &request.dispatch.packet.task.spec.write_scope,
        );

        if controlled.cancelled {
            let reason = match scope_check {
                Ok(_) => "cancelled by host request".to_owned(),
                Err(msg) => sanitize_reason(&format!("cancelled by host request ({msg})")),
            };
            return AdapterOutcome::Cancelled {
                correlation,
                worker_stopped,
                reason,
            };
        }

        if controlled.process.timed_out {
            let reason = match scope_check {
                Ok(_) => "codex execution timed out".to_owned(),
                Err(msg) => sanitize_reason(&format!("codex execution timed out ({msg})")),
            };
            return AdapterOutcome::TimedOut {
                correlation,
                worker_stopped,
                reason,
            };
        }

        if !controlled.process.status.is_some_and(|s| s.success()) {
            let exit_code = controlled.process.status.and_then(|s| s.code());
            let detail_source = if !stderr_text.trim().is_empty() {
                stderr_text.trim()
            } else {
                stdout_text.trim()
            };
            let base = format!("codex exited unsuccessfully: {detail_source}");
            let detail = match scope_check {
                Ok(_) => sanitize_reason(&base),
                Err(msg) => sanitize_reason(&format!("{base}; {msg}")),
            };
            return AdapterOutcome::HostCrashed {
                correlation,
                worker_stopped,
                exit_code,
                detail,
            };
        }

        let after_snapshot = match scope_check {
            Ok(snapshot) => snapshot,
            Err(detail) => {
                return AdapterOutcome::InvalidOutput {
                    correlation,
                    worker_stopped,
                    detail: sanitize_reason(&detail),
                };
            }
        };

        let payload = if caps.supports_output_file {
            match read_last_message_file(&output_path) {
                Ok(Some(text)) => Some(text),
                Ok(None) => extract_message_from_stdout(&stdout_text, caps.supports_json_output),
                Err(detail) => {
                    return AdapterOutcome::InvalidOutput {
                        correlation,
                        worker_stopped,
                        detail,
                    };
                }
            }
        } else {
            extract_message_from_stdout(&stdout_text, caps.supports_json_output)
        };

        let Some(raw_json) = payload else {
            return AdapterOutcome::InvalidOutput {
                correlation,
                worker_stopped,
                detail: "codex produced no structured result payload".to_owned(),
            };
        };

        let envelope: CodexResultEnvelope = match serde_json::from_str(raw_json.trim()) {
            Ok(parsed) => parsed,
            Err(err) => {
                return AdapterOutcome::InvalidOutput {
                    correlation,
                    worker_stopped,
                    detail: sanitize_reason(&format!("invalid codex JSON result: {err}")),
                };
            }
        };

        let mut result = WorkerResult {
            schema: SCHEMA,
            story: envelope.story,
            task: envelope.task,
            task_revision: envelope.task_revision,
            status: envelope.status,
            report: envelope.report,
            commit: envelope.commit,
            artifacts: envelope.artifacts,
        };
        if let Err(err) = super::runner::validate_worker_result(&result, &request.dispatch) {
            return AdapterOutcome::InvalidOutput {
                correlation,
                worker_stopped,
                detail: sanitize_reason(&format!("invalid codex worker result: {err}")),
            };
        }

        if worker_stopped
            && request.policy == ExecutionPolicy::WorkspaceWrite
            && result.status == ResultStatus::Succeeded
        {
            let has_git = workdir_is_git_worktree(&request.workdir);
            if has_git
                && let Err(detail) =
                    verify_git_worktree_base(&request.workdir, &request.dispatch.packet)
            {
                return AdapterOutcome::InvalidOutput {
                    correlation,
                    worker_stopped,
                    detail: sanitize_reason(&detail),
                };
            }

            let promoted = match apply_staged_writes(
                staging_dir.path(),
                &request.workdir,
                &before_snapshot,
                &after_snapshot,
                &request.dispatch.packet.task.spec.write_scope,
            ) {
                Ok(promoted) => promoted,
                Err(detail) => {
                    return AdapterOutcome::InvalidOutput {
                        correlation,
                        worker_stopped,
                        detail: sanitize_reason(&detail),
                    };
                }
            };

            if has_git {
                if promoted.paths.is_empty() {
                    let _ = rollback_workdir_promotion(&request.workdir, &promoted.backups);
                    return AdapterOutcome::InvalidOutput {
                        correlation,
                        worker_stopped,
                        detail: "codex reported succeeded for WorkspaceWrite task in Git worktree without producing write_scope changes to commit".to_owned(),
                    };
                }
                match commit_promoted_writes(
                    &request.workdir,
                    &promoted.paths,
                    &request.dispatch.packet.story,
                    &request.dispatch.packet.task.spec.id,
                    request.dispatch.packet.task.revision,
                ) {
                    Ok(head_after) => {
                        result.commit = Some(head_after);
                        if let Err(err) =
                            super::runner::validate_worker_result(&result, &request.dispatch)
                        {
                            return AdapterOutcome::InvalidOutput {
                                correlation,
                                worker_stopped,
                                detail: sanitize_reason(&format!(
                                    "invalid codex worker result after commit: {err}"
                                )),
                            };
                        }
                    }
                    Err(commit_err) => {
                        let _ = unstage_paths(&request.workdir, &promoted.paths);
                        let rollback_errs =
                            rollback_workdir_promotion(&request.workdir, &promoted.backups);
                        let detail = if rollback_errs.is_empty() {
                            commit_err
                        } else {
                            format!(
                                "{commit_err} (rollback failed: {})",
                                rollback_errs.join("; ")
                            )
                        };
                        return AdapterOutcome::InvalidOutput {
                            correlation,
                            worker_stopped,
                            detail: sanitize_reason(&detail),
                        };
                    }
                }
            }
        }

        AdapterOutcome::Completed {
            correlation,
            result,
            worker_stopped,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CodexResultEnvelope {
    story: String,
    task: String,
    task_revision: u64,
    status: ResultStatus,
    report: String,
    commit: Option<String>,
    artifacts: Vec<Artifact>,
}

fn parse_exec_help(
    exe: &str,
    version: Option<String>,
    help_text: &str,
    supports_cancel: bool,
) -> HostCapabilities {
    let mut detected = BTreeSet::new();
    for token in help_text.split(|c: char| {
        c.is_whitespace() || matches!(c, ',' | '[' | ']' | '(' | ')' | '=' | '<' | '>' | ':')
    }) {
        if token.starts_with("--") && token.len() > 2 {
            detected.insert(token.to_owned());
        }
    }

    let supports_json_output = detected.contains("--json");
    let supports_output_schema = detected.contains("--output-schema");
    let supports_output_file = detected.contains("--output-last-message");
    let supports_working_dir = detected.contains("--cd");
    let supports_sandbox_flag = detected.contains("--sandbox");
    let supports_stdin_prompt = help_text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("stdin")
            || (line.contains("PROMPT") && line.split_whitespace().any(|tok| tok == "-"))
    });

    HostCapabilities {
        schema: ADAPTER_SCHEMA,
        provider: "codex".to_owned(),
        available: supports_sandbox_flag,
        version,
        executable: Some(exe.to_owned()),
        supports_json_output,
        supports_output_schema,
        supports_output_file,
        supports_working_dir,
        supports_sandbox_flag,
        supports_cancel,
        supports_session_id: supports_json_output,
        supports_stdin_prompt,
        detected_flags: detected.into_iter().collect(),
        unavailable_reason: if supports_sandbox_flag {
            None
        } else {
            Some(
                "installed codex host does not advertise '--sandbox' required to enforce execution policy"
                    .to_owned(),
            )
        },
    }
}

fn extract_session_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let candidate = value
            .get("thread_id")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("session_id").and_then(|v| v.as_str()))
            .or_else(|| {
                value
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(|v| v.as_str())
            });
        if let Some(id) = candidate
            && !id.trim().is_empty()
            && id.len() <= MAX_SESSION_ID_BYTES
            && !id.chars().any(char::is_control)
        {
            return Some(id.to_owned());
        }
    }
    None
}

fn extract_message_from_stdout(stdout: &str, json_output: bool) -> Option<String> {
    if json_output {
        for line in stdout.lines().rev() {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if value.get("type").and_then(|v| v.as_str()) == Some("item.completed")
                && let Some(item) = value.get("item")
                && item.get("type").and_then(|v| v.as_str()) == Some("agent_message")
                && let Some(text) = item.get("text").and_then(|v| v.as_str())
                && !text.trim().is_empty()
            {
                return Some(text.to_owned());
            }
            if let Some(text) = value
                .get("last_message")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("message").and_then(|v| v.as_str()))
                && !text.trim().is_empty()
            {
                return Some(text.to_owned());
            }
        }
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn build_prompt(request: &AdapterRequest) -> AdapterResult<String> {
    let packet = &request.dispatch.packet;
    let task = &packet.task;
    let constraints = format!(
        "\nConstraints:\n\
         - Modify only files inside write_scope.\n\
         - Never modify .storyflow/ or protected paths.\n\
         - Never run git push, auto-merge, or claim story approval.\n\
         - Return ONLY a JSON object with fields: story (\"{}\"), task (\"{}\"), task_revision ({}), status (\"succeeded\"|\"failed\"|\"blocked\"), report (<=4096 bytes), commit (null or 40/64 hex), artifacts (array of {{path, sha256}}).\n",
        packet.story, task.spec.id, task.revision
    );

    let mut required_body = format!(
        "Storyflow task execution request\n\
         Story: {}\n\
         Base commit: {}\n\
         Task: {} (revision {})\n\
         Attempt: {}\n\
         Read scope: {}\n\
         Write scope: {}\n\
         Acceptance criteria:\n",
        packet.story,
        packet.base,
        task.spec.id,
        task.revision,
        request.dispatch.lease.attempt_id,
        if task.spec.read_scope.is_empty() {
            "(none)".to_owned()
        } else {
            task.spec.read_scope.join(", ")
        },
        if task.spec.write_scope.is_empty() {
            "(none)".to_owned()
        } else {
            task.spec.write_scope.join(", ")
        },
    );
    for ac in &task.spec.acceptance_criteria {
        required_body.push_str("- ");
        required_body.push_str(ac);
        required_body.push('\n');
    }

    if required_body.len() + constraints.len() > MAX_PROMPT_BYTES {
        return Err(AdapterError::new(
            "prompt_too_large",
            "required task prompt, acceptance criteria, and output contract exceed byte budget",
        ));
    }

    let mut prereqs = String::new();
    if !packet.prerequisites.is_empty() {
        prereqs.push_str("Completed prerequisites:\n");
        for (dep_id, dep_res) in &packet.prerequisites {
            prereqs.push_str(&format!(
                "- {dep_id}: {:?} ({})\n",
                dep_res.status,
                dep_res.commit.as_deref().unwrap_or("no-commit")
            ));
        }
        let prereq_budget = MAX_PROMPT_BYTES - required_body.len() - constraints.len();
        if prereqs.len() > prereq_budget {
            const MARKER: &str = "\n...[prerequisites truncated]...\n";
            if prereq_budget >= MARKER.len() {
                let mut end = prereq_budget - MARKER.len();
                while !prereqs.is_char_boundary(end) {
                    end -= 1;
                }
                prereqs.truncate(end);
                prereqs.push_str(MARKER);
            } else {
                prereqs.clear();
            }
        }
    }

    let mut prompt = String::with_capacity(required_body.len() + prereqs.len() + constraints.len());
    prompt.push_str(&required_body);
    prompt.push_str(&prereqs);
    prompt.push_str(&constraints);
    Ok(prompt)
}

fn output_schema_json() -> &'static str {
    r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "required": ["story", "task", "task_revision", "status", "report", "commit", "artifacts"],
  "properties": {
    "story": { "type": "string", "minLength": 1 },
    "task": { "type": "string", "minLength": 1 },
    "task_revision": { "type": "integer", "minimum": 0 },
    "status": { "type": "string", "enum": ["succeeded", "failed", "blocked"] },
    "report": { "type": "string", "minLength": 1, "maxLength": 4096 },
    "commit": { "type": ["string", "null"] },
    "artifacts": {
      "type": "array",
      "maxItems": 64,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "sha256"],
        "properties": {
          "path": { "type": "string", "minLength": 1 },
          "sha256": { "type": "string", "pattern": "^[0-9a-f]{64}$" }
        }
      }
    }
  }
}"#
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotEntry {
    Dir {
        mode: u32,
    },
    File {
        len: u64,
        sha256: [u8; 32],
        mode: u32,
    },
    LockedFile {
        len: u64,
        mode: u32,
    },
    Symlink(String),
    Unreadable,
}

enum StageError {
    BudgetExceeded,
    SymlinkOrSpecial(String),
    Io(String),
}

fn stage_read_scope(
    workdir: &Path,
    staging_root: &Path,
    read_scope: &[String],
    max_bytes: usize,
) -> Result<(), StageError> {
    if read_scope.is_empty() {
        return Ok(());
    }
    let mut copied_bytes = 0usize;
    let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
    stage_dir_entries(
        workdir,
        workdir,
        staging_root,
        read_scope,
        max_bytes,
        &mut copied_bytes,
        &mut dir_modes,
    )?;
    for (dir_path, mode) in dir_modes.into_iter().rev() {
        restore_mode(&dir_path, mode).map_err(|err| StageError::Io(err.to_string()))?;
    }
    Ok(())
}

fn stage_dir_entries(
    workdir: &Path,
    dir: &Path,
    staging_root: &Path,
    read_scope: &[String],
    max_bytes: usize,
    copied_bytes: &mut usize,
    dir_modes: &mut Vec<(PathBuf, u32)>,
) -> Result<(), StageError> {
    let mut children: Vec<_> = fs::read_dir(dir)
        .map_err(|err| StageError::Io(err.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| StageError::Io(err.to_string()))?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = child.path();
        let rel = path
            .strip_prefix(workdir)
            .map_err(|_| StageError::Io("workspace path escaped root".to_owned()))?;
        let rel_str = rel_path_utf8(rel).map_err(|err| StageError::Io(err.to_string()))?;
        if rel_str.is_empty() || is_protected_path(&rel_str) {
            continue;
        }
        let in_read = read_scope.iter().any(|scope| within_path(&rel_str, scope));
        let is_ancestor = read_scope.iter().any(|scope| within_path(scope, &rel_str));
        if !in_read && !is_ancestor {
            continue;
        }

        let meta = fs::symlink_metadata(&path).map_err(|err| StageError::Io(err.to_string()))?;
        let file_type = meta.file_type();
        let target = staging_root.join(&rel_str);
        if file_type.is_symlink() {
            return Err(StageError::SymlinkOrSpecial(format!(
                "refusing symlink in codex read_scope: {rel_str}"
            )));
        } else if file_type.is_dir() {
            fs::create_dir_all(&target).map_err(|err| StageError::Io(err.to_string()))?;
            dir_modes.push((target, entry_mode(&meta)));
            stage_dir_entries(
                workdir,
                &path,
                staging_root,
                read_scope,
                max_bytes,
                copied_bytes,
                dir_modes,
            )?;
        } else if file_type.is_file() {
            if !in_read {
                continue;
            }
            let next_bytes = usize::try_from(meta.len())
                .ok()
                .and_then(|len| copied_bytes.checked_add(len))
                .filter(|total| *total <= max_bytes);
            let Some(new_total) = next_bytes else {
                return Err(StageError::BudgetExceeded);
            };
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| StageError::Io(err.to_string()))?;
            }
            let bytes = fs::read(&path).map_err(|err| StageError::Io(err.to_string()))?;
            fs::write(&target, &bytes).map_err(|err| StageError::Io(err.to_string()))?;
            restore_mode(&target, entry_mode(&meta))
                .map_err(|err| StageError::Io(err.to_string()))?;
            *copied_bytes = new_total;
        } else {
            return Err(StageError::SymlinkOrSpecial(format!(
                "unsupported special file in codex read_scope: {rel_str}"
            )));
        }
    }
    Ok(())
}

fn find_unscoped_workdir_entries(workdir: &Path, read_scope: &[String]) -> io::Result<Vec<String>> {
    let mut unscoped = Vec::new();
    collect_unscoped_entries(workdir, workdir, read_scope, &mut unscoped)?;
    Ok(unscoped)
}

fn collect_unscoped_entries(
    workdir: &Path,
    dir: &Path,
    read_scope: &[String],
    unscoped: &mut Vec<String>,
) -> io::Result<()> {
    const MAX_SAMPLE: usize = 16;
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        if unscoped.len() >= MAX_SAMPLE {
            break;
        }
        let path = child.path();
        let rel = path
            .strip_prefix(workdir)
            .map_err(|_| io::Error::other("workspace path escaped root"))?;
        let rel_str = rel_path_utf8(rel)?;
        if rel_str.is_empty() {
            continue;
        }
        if is_protected_path(&rel_str) {
            unscoped.push(rel_str);
            continue;
        }
        let in_read = read_scope.iter().any(|scope| within_path(&rel_str, scope));
        let is_ancestor = read_scope.iter().any(|scope| within_path(scope, &rel_str));
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_dir() {
            if in_read || is_ancestor {
                collect_unscoped_entries(workdir, &path, read_scope, unscoped)?;
            } else {
                unscoped.push(rel_str);
            }
        } else if !in_read {
            unscoped.push(rel_str);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PromotedWrites {
    paths: Vec<String>,
    backups: BTreeMap<String, WorkdirBackupEntry>,
}

enum StagedWriteOp {
    RemoveFile {
        rel: String,
    },
    RemoveDir {
        rel: String,
    },
    EnsureDir {
        rel: String,
        mode: u32,
    },
    WriteFile {
        rel: String,
        bytes: Vec<u8>,
        mode: u32,
    },
}

#[derive(Debug)]
enum WorkdirBackupEntry {
    Missing,
    Dir { mode: u32 },
    File { bytes: Vec<u8>, mode: u32 },
}

fn apply_staged_writes(
    staging_root: &Path,
    workdir: &Path,
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    write_scope: &[String],
) -> Result<PromotedWrites, String> {
    apply_staged_writes_with(
        staging_root,
        workdir,
        before,
        after,
        write_scope,
        |_| Ok(()),
    )
}

fn apply_staged_writes_with(
    staging_root: &Path,
    workdir: &Path,
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    write_scope: &[String],
    on_write: impl FnMut(&str) -> Result<(), String>,
) -> Result<PromotedWrites, String> {
    let all_paths: BTreeSet<&String> = before.entries.keys().chain(after.entries.keys()).collect();
    let mut ops = Vec::new();
    let mut promoted_paths = Vec::new();

    for path in all_paths {
        let prev = before.entries.get(path);
        let curr = after.entries.get(path);
        if prev == curr {
            continue;
        }
        if is_protected_path(path) {
            return Err(format!(
                "refusing protected path promotion from staging: {path}"
            ));
        }
        let in_write_scope = write_scope.iter().any(|scope| within_path(path, scope));
        let created_parent_for_scope = prev.is_none()
            && matches!(curr, Some(SnapshotEntry::Dir { .. }))
            && write_scope.iter().any(|scope| within_path(scope, path));
        if !in_write_scope && !created_parent_for_scope {
            return Err(format!(
                "refusing out-of-scope promotion from staging: {path}"
            ));
        }
        ensure_no_symlink_components(workdir, path)?;

        let staged_path = staging_root.join(path);
        match (prev, curr) {
            (_, Some(SnapshotEntry::Symlink(_)))
            | (_, Some(SnapshotEntry::Unreadable))
            | (_, Some(SnapshotEntry::LockedFile { .. })) => {
                return Err(format!(
                    "refusing unsafe staged entry in write_scope: {path}"
                ));
            }
            (_, Some(SnapshotEntry::File { mode, .. })) => {
                let bytes = fs::read(&staged_path).map_err(|err| {
                    format!("failed to read staged output {path} before promotion: {err}")
                })?;
                promoted_paths.push(path.clone());
                ops.push(StagedWriteOp::WriteFile {
                    rel: path.clone(),
                    bytes,
                    mode: *mode,
                });
            }
            (_, Some(SnapshotEntry::Dir { mode })) => {
                ops.push(StagedWriteOp::EnsureDir {
                    rel: path.clone(),
                    mode: *mode,
                });
            }
            (Some(SnapshotEntry::File { .. }), None) => {
                promoted_paths.push(path.clone());
                ops.push(StagedWriteOp::RemoveFile { rel: path.clone() });
            }
            (Some(SnapshotEntry::Dir { .. }), None) => {
                ops.push(StagedWriteOp::RemoveDir { rel: path.clone() });
            }
            (
                None
                | Some(
                    SnapshotEntry::Symlink(_)
                    | SnapshotEntry::LockedFile { .. }
                    | SnapshotEntry::Unreadable,
                ),
                None,
            ) => {}
        }
    }

    if ops.is_empty() {
        return Ok(PromotedWrites {
            paths: Vec::new(),
            backups: BTreeMap::new(),
        });
    }

    let backups = capture_workdir_backups(workdir, &ops)?;
    if let Err(apply_err) = apply_write_ops(workdir, &ops, on_write) {
        let rollback_errors = rollback_workdir_promotion(workdir, &backups);
        if rollback_errors.is_empty() {
            return Err(format!("{apply_err} (workdir changes rolled back)"));
        }
        return Err(format!(
            "{apply_err} (rollback failed: {})",
            rollback_errors.join("; ")
        ));
    }
    Ok(PromotedWrites {
        paths: promoted_paths,
        backups,
    })
}

fn ambient_codex_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(codex_home).join("config.toml"));
        return paths;
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(home).join(".codex").join("config.toml"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        let candidate = PathBuf::from(profile).join(".codex").join("config.toml");
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
    paths
}

fn config_toml_sets_legacy_sandbox(text: &str) -> bool {
    parse_toml_checks(text).unwrap_or(true)
}

fn parse_toml_checks(text: &str) -> Result<bool, ()> {
    let mut current_table: Vec<String> = Vec::new();
    for raw_line in text.lines() {
        let line_no_comment = strip_toml_comment(raw_line)?;
        let code = line_no_comment.trim();
        if code.is_empty() {
            continue;
        }
        if code.starts_with('[') {
            let (inner, rest) = if let Some(after_dbl) = code.strip_prefix("[[") {
                let end = after_dbl.rfind("]]").ok_or(())?;
                (&after_dbl[..end], after_dbl[end + 2..].trim())
            } else {
                let after_sgl = code.strip_prefix('[').ok_or(())?;
                let end = after_sgl.rfind(']').ok_or(())?;
                (&after_sgl[..end], after_sgl[end + 1..].trim())
            };
            if !rest.is_empty() {
                return Err(());
            }
            let table_path = parse_toml_key_path(inner)?;
            if is_restricted_toml_path(&table_path) {
                return Ok(true);
            }
            current_table = table_path;
            continue;
        }
        let Some((lhs, rhs)) = split_toml_assignment(code) else {
            return Err(());
        };
        let key_segments = parse_toml_key_path(lhs)?;
        let rhs_trimmed = rhs.trim();
        if rhs_trimmed.is_empty()
            || rhs_trimmed.contains("\"\"\"")
            || rhs_trimmed.contains("'''")
            || !toml_value_brackets_balanced(rhs_trimmed)?
        {
            return Err(());
        }
        let mut full_path = current_table.clone();
        full_path.extend(key_segments);
        if is_restricted_toml_path(&full_path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_restricted_toml_path(segments: &[String]) -> bool {
    let Some(first) = segments.first().map(String::as_str) else {
        return false;
    };
    first == "sandbox_mode"
        || first == "sandbox_workspace_write"
        || (first == "permissions"
            && segments
                .get(1)
                .is_some_and(|p| p.starts_with("storyflow_scoped")))
}

fn strip_toml_comment(line: &str) -> Result<&str, ()> {
    if line.contains("\"\"\"") || line.contains("'''") {
        return Err(());
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
        } else if b == b'\'' {
            in_single = true;
            i += 1;
        } else if b == b'"' {
            in_double = true;
            i += 1;
        } else if b == b'#' {
            return Ok(&line[..i]);
        } else {
            i += 1;
        }
    }
    if in_single || in_double {
        return Err(());
    }
    Ok(line)
}

fn split_toml_assignment(code: &str) -> Option<(&str, &str)> {
    let bytes = code.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if b == b'\'' {
            in_single = true;
        } else if b == b'"' {
            in_double = true;
        } else if b == b'=' {
            return Some((&code[..i], &code[i + 1..]));
        }
        i += 1;
    }
    None
}

fn toml_value_brackets_balanced(rhs: &str) -> Result<bool, ()> {
    let bytes = rhs.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                if i + 1 >= bytes.len() {
                    return Err(());
                }
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if b == b'\'' {
            in_single = true;
        } else if b == b'"' {
            in_double = true;
        } else if b == b'[' || b == b'{' {
            depth += 1;
        } else if b == b']' || b == b'}' {
            depth -= 1;
            if depth < 0 {
                return Err(());
            }
        }
        i += 1;
    }
    if in_single || in_double {
        return Err(());
    }
    Ok(depth == 0)
}

fn parse_toml_key_path(raw: &str) -> Result<Vec<String>, ()> {
    let mut segments = Vec::new();
    let mut rest = raw.trim();
    if rest.is_empty() {
        return Err(());
    }
    loop {
        rest = rest.trim_start();
        let (seg, after) = if let Some(s) = rest.strip_prefix('\'') {
            let end = s.find('\'').ok_or(())?;
            (s[..end].to_owned(), &s[end + 1..])
        } else if let Some(s) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = s.char_indices();
            let mut end_idx = None;
            while let Some((idx, ch)) = chars.next() {
                if ch == '"' {
                    end_idx = Some(idx);
                    break;
                }
                if ch == '\\' {
                    let (_, esc) = chars.next().ok_or(())?;
                    match esc {
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        _ => return Err(()),
                    }
                } else {
                    out.push(ch);
                }
            }
            let end = end_idx.ok_or(())?;
            (out, &s[end + 1..])
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(rest.len());
            if end == 0 {
                return Err(());
            }
            (rest[..end].to_owned(), &rest[end..])
        };
        segments.push(seg);
        let next = after.trim_start();
        if next.is_empty() {
            break;
        }
        rest = next.strip_prefix('.').ok_or(())?;
    }
    Ok(segments)
}

fn scoped_profile_name(attempt_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(attempt_id.as_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(TEMP_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = hasher.finalize();
    let mut suffix = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut suffix, "{byte:02x}");
    }
    format!("storyflow_scoped_{suffix}")
}

fn supports_config_read_confinement(caps: &HostCapabilities) -> bool {
    caps.detected_flags.iter().any(|f| f == "--config")
}

fn append_config_read_confinement(
    args: &mut Vec<String>,
    profile_name: &str,
    staging_dir: &Path,
    workdir: &Path,
    policy: ExecutionPolicy,
) {
    let mut push_cfg = |kv: String| {
        args.push("--config".to_owned());
        args.push(kv);
    };
    push_cfg(format!("default_permissions=\"{profile_name}\""));
    push_cfg(format!("permissions.{profile_name}.network.enabled=false"));
    push_cfg(format!(
        "permissions.{profile_name}.filesystem.\":root\"=\"deny\""
    ));
    push_cfg(format!(
        "permissions.{profile_name}.filesystem.\":minimal\"=\"read\""
    ));

    let staging_policy = normalize_policy_path(staging_dir);
    let workdir_policy = normalize_policy_path(workdir);
    let staging_mode = match policy {
        ExecutionPolicy::ReadOnly => "read",
        ExecutionPolicy::WorkspaceWrite => "write",
    };
    if let Ok(quoted_staging) = serde_json::to_string(&staging_policy) {
        push_cfg(format!(
            "permissions.{profile_name}.filesystem.{quoted_staging}=\"{staging_mode}\""
        ));
    }
    if let Ok(quoted_workdir) = serde_json::to_string(&workdir_policy) {
        push_cfg(format!(
            "permissions.{profile_name}.filesystem.{quoted_workdir}=\"deny\""
        ));
    }
    if workdir_is_git_worktree(workdir)
        && let Ok(common_dir_raw) = run_git_checked(
            workdir,
            &["rev-parse".to_owned(), "--git-common-dir".to_owned()],
        )
    {
        let raw_path = Path::new(&common_dir_raw);
        let abs_common = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            workdir.join(raw_path)
        };
        let common_policy = normalize_policy_path(&abs_common);
        if common_policy != workdir_policy
            && let Ok(quoted_common) = serde_json::to_string(&common_policy)
        {
            push_cfg(format!(
                "permissions.{profile_name}.filesystem.{quoted_common}=\"deny\""
            ));
        }
    }
}

fn verify_git_worktree_base(
    workdir: &Path,
    packet: &crate::tasks::TaskPacket,
) -> Result<String, String> {
    let dirty = run_git_checked(workdir, &["status".to_owned(), "--porcelain".to_owned()])?;
    if !dirty.trim().is_empty() {
        return Err(
            "git worktree has uncommitted changes before promoting staged writes".to_owned(),
        );
    }
    let head = run_git_checked(
        workdir,
        &[
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "HEAD".to_owned(),
        ],
    )?;
    let story_base = run_git_checked(
        workdir,
        &[
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            format!("{}^{{commit}}", packet.base),
        ],
    )?;
    if packet.prerequisites.is_empty() {
        if head != story_base {
            return Err(format!(
                "git worktree HEAD ({head}) does not match expected task base ({story_base})"
            ));
        }
        return Ok(head);
    }
    run_git_checked(
        workdir,
        &[
            "merge-base".to_owned(),
            "--is-ancestor".to_owned(),
            story_base,
            "HEAD".to_owned(),
        ],
    )
    .map_err(|_| "git worktree HEAD does not descend from pinned story base commit".to_owned())?;
    for (dep_id, dep_res) in &packet.prerequisites {
        if let Some(dep_commit) = &dep_res.commit {
            let resolved_dep = run_git_checked(
                workdir,
                &[
                    "rev-parse".to_owned(),
                    "--verify".to_owned(),
                    format!("{dep_commit}^{{commit}}"),
                ],
            )?;
            run_git_checked(
                workdir,
                &[
                    "merge-base".to_owned(),
                    "--is-ancestor".to_owned(),
                    resolved_dep,
                    "HEAD".to_owned(),
                ],
            )
            .map_err(|_| {
                format!(
                    "git worktree HEAD is stale relative to prerequisite {dep_id} commit {dep_commit}"
                )
            })?;
        }
    }
    Ok(head)
}

fn normalize_policy_path(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let raw = resolved.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_owned()
    }
    #[cfg(not(windows))]
    {
        raw
    }
}

fn workdir_is_git_worktree(workdir: &Path) -> bool {
    if fs::symlink_metadata(workdir.join(".git")).is_err() {
        return false;
    }
    let Ok(toplevel) = run_git_checked(
        workdir,
        &["rev-parse".to_owned(), "--show-toplevel".to_owned()],
    ) else {
        return false;
    };
    let Ok(canon_work) = fs::canonicalize(workdir) else {
        return false;
    };
    let Ok(canon_top) = fs::canonicalize(Path::new(&toplevel)) else {
        return false;
    };
    canon_work == canon_top
}

fn run_git_checked(workdir: &Path, args: &[String]) -> Result<String, String> {
    let program = git_executable().to_string_lossy().into_owned();
    let res = run_process(
        &CommandSpec {
            program,
            args: args.to_vec(),
            timeout: Duration::from_secs(30),
        },
        workdir,
    )
    .map_err(|err| format!("git execution failed: {err}"))?;
    if res.timed_out {
        return Err("git command timed out while pinning worker commit".to_owned());
    }
    if !res.status.is_some_and(|s| s.success()) {
        let stderr = String::from_utf8_lossy(&res.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&res.stdout).trim().to_owned();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("git command failed: {msg}"));
    }
    Ok(String::from_utf8_lossy(&res.stdout).trim().to_owned())
}

fn commit_promoted_writes(
    workdir: &Path,
    promoted_paths: &[String],
    story: &str,
    task: &str,
    task_revision: u64,
) -> Result<String, String> {
    let head_before = run_git_checked(
        workdir,
        &[
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "HEAD".to_owned(),
        ],
    )?;
    let hooks_dir = TempArtifactsDir::new()
        .map_err(|err| format!("failed to allocate empty git hooks dir: {err}"))?;
    let hooks_arg = format!("core.hooksPath={}", hooks_dir.path().display());

    let mut add_args = vec!["add".to_owned(), "-A".to_owned(), "--".to_owned()];
    add_args.extend(promoted_paths.iter().cloned());
    run_git_checked(workdir, &add_args)?;

    let mut diff_args = vec![
        "diff".to_owned(),
        "--cached".to_owned(),
        "--name-only".to_owned(),
        "--".to_owned(),
    ];
    diff_args.extend(promoted_paths.iter().cloned());
    let staged_diff = run_git_checked(workdir, &diff_args)?;
    if staged_diff.trim().is_empty() {
        return Err(
            "codex promoted files produced no git diff to commit for WorkspaceWrite task"
                .to_owned(),
        );
    }

    let message = format!("storyflow({story}): task {task} r{task_revision}");
    let mut commit_args = vec![
        "-c".to_owned(),
        "user.name=storyflow".to_owned(),
        "-c".to_owned(),
        "user.email=storyflow@local".to_owned(),
        "-c".to_owned(),
        "commit.gpgsign=false".to_owned(),
        "-c".to_owned(),
        hooks_arg,
        "commit".to_owned(),
        "--no-verify".to_owned(),
        "-m".to_owned(),
        message,
        "--".to_owned(),
    ];
    commit_args.extend(promoted_paths.iter().cloned());
    run_git_checked(workdir, &commit_args)?;

    let head_after = run_git_checked(
        workdir,
        &[
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "HEAD".to_owned(),
        ],
    )?;
    if head_after == head_before {
        return Err("git commit did not advance HEAD in worker worktree".to_owned());
    }
    Ok(head_after)
}

fn unstage_paths(workdir: &Path, promoted_paths: &[String]) -> Result<(), String> {
    let mut reset_args = vec![
        "reset".to_owned(),
        "-q".to_owned(),
        "HEAD".to_owned(),
        "--".to_owned(),
    ];
    reset_args.extend(promoted_paths.iter().cloned());
    run_git_checked(workdir, &reset_args).map(|_| ())
}

fn capture_workdir_backups(
    workdir: &Path,
    ops: &[StagedWriteOp],
) -> Result<BTreeMap<String, WorkdirBackupEntry>, String> {
    let mut backups = BTreeMap::new();
    let mut record_path = |rel: &str| -> Result<(), String> {
        if backups.contains_key(rel) {
            return Ok(());
        }
        let target = workdir.join(rel);
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => Err(format!(
                "refusing staged promotion over symlink in workdir: {rel}"
            )),
            Ok(meta) if meta.file_type().is_dir() => {
                backups.insert(
                    rel.to_owned(),
                    WorkdirBackupEntry::Dir {
                        mode: entry_mode(&meta),
                    },
                );
                Ok(())
            }
            Ok(meta) if meta.file_type().is_file() => {
                let bytes = fs::read(&target).map_err(|err| {
                    format!("failed to back up {rel} in workdir before promotion: {err}")
                })?;
                backups.insert(
                    rel.to_owned(),
                    WorkdirBackupEntry::File {
                        bytes,
                        mode: entry_mode(&meta),
                    },
                );
                Ok(())
            }
            Ok(_) => Err(format!(
                "refusing staged promotion over special file in workdir: {rel}"
            )),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                backups.insert(rel.to_owned(), WorkdirBackupEntry::Missing);
                Ok(())
            }
            Err(err) => Err(format!(
                "failed to inspect {rel} in workdir before promotion: {err}"
            )),
        }
    };

    for op in ops {
        let rel = match op {
            StagedWriteOp::RemoveFile { rel }
            | StagedWriteOp::RemoveDir { rel }
            | StagedWriteOp::EnsureDir { rel, .. }
            | StagedWriteOp::WriteFile { rel, .. } => rel.as_str(),
        };
        let mut prefix = Vec::new();
        for comp in Path::new(rel).components() {
            let std::path::Component::Normal(part) = comp else {
                return Err(format!("invalid staged promotion path: {rel}"));
            };
            prefix.push(part.to_string_lossy().into_owned());
            let anc = prefix.join("/");
            record_path(&anc)?;
        }
    }
    Ok(backups)
}

fn apply_write_ops(
    workdir: &Path,
    ops: &[StagedWriteOp],
    mut on_write: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    // Apply removals deepest-first, then directory creation shallowest-first, then file writes.
    for op in ops.iter().rev() {
        if let StagedWriteOp::RemoveFile { rel } = op {
            let target = workdir.join(rel);
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_file() => {
                    remove_file_or_symlink(&target)
                        .map_err(|err| format!("failed to remove {rel}: {err}"))?;
                }
                Ok(_) => {
                    return Err(format!(
                        "refusing non-regular file removal in workdir: {rel}"
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("failed to inspect {rel} in workdir: {err}")),
            }
        }
    }
    for op in ops.iter().rev() {
        if let StagedWriteOp::RemoveDir { rel } = op {
            let target = workdir.join(rel);
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_dir() => {
                    make_dir_accessible(&target)
                        .and_then(|()| fs::remove_dir(&target))
                        .map_err(|err| format!("failed to remove directory {rel}: {err}"))?;
                }
                Ok(_) => {
                    return Err(format!("refusing non-directory removal in workdir: {rel}"));
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("failed to inspect {rel} in workdir: {err}")),
            }
        }
    }
    for op in ops {
        if let StagedWriteOp::EnsureDir { rel, .. } = op {
            let target = workdir.join(rel);
            if let Ok(meta) = fs::symlink_metadata(&target)
                && !meta.file_type().is_dir()
            {
                if meta.file_type().is_file() {
                    remove_file_or_symlink(&target)
                        .map_err(|err| format!("failed to replace file {rel} with dir: {err}"))?;
                } else {
                    return Err(format!("refusing to overwrite non-file entry {rel}"));
                }
            }
            fs::create_dir_all(&target)
                .map_err(|err| format!("failed to create directory {rel}: {err}"))?;
        }
    }
    for op in ops {
        if let StagedWriteOp::WriteFile { rel, bytes, mode } = op {
            on_write(rel)?;
            let target = workdir.join(rel);
            if let Ok(meta) = fs::symlink_metadata(&target)
                && meta.file_type().is_dir()
            {
                remove_dir_if_present(&target)
                    .map_err(|err| format!("failed to replace dir {rel} with file: {err}"))?;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create parent directory for {rel}: {err}"))?;
            }
            make_writable_if_exists(&target)
                .and_then(|()| fs::write(&target, bytes))
                .and_then(|()| restore_mode(&target, *mode))
                .map_err(|err| format!("failed to write staged file {rel}: {err}"))?;
        }
    }
    for op in ops.iter().rev() {
        if let StagedWriteOp::EnsureDir { rel, mode } = op {
            let target = workdir.join(rel);
            restore_mode(&target, *mode)
                .map_err(|err| format!("failed to set directory permissions for {rel}: {err}"))?;
        }
    }
    Ok(())
}

fn rollback_workdir_promotion(
    workdir: &Path,
    backups: &BTreeMap<String, WorkdirBackupEntry>,
) -> Vec<String> {
    let mut errors = Vec::new();
    // 1. Ensure existing parent dirs are writable before undoing changes.
    for (rel, entry) in backups {
        if matches!(entry, WorkdirBackupEntry::Dir { .. }) {
            let _ = make_dir_accessible(&workdir.join(rel));
        }
    }
    // 2. Remove anything created where WorkdirBackupEntry::Missing existed, deepest-first.
    for (rel, entry) in backups.iter().rev() {
        if matches!(entry, WorkdirBackupEntry::Missing) {
            let target = workdir.join(rel);
            let res = match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_dir() => remove_dir_if_present(&target),
                Ok(_) => remove_file_or_symlink(&target),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            };
            if let Err(err) = res {
                errors.push(format!("{rel}: {err}"));
            }
        }
    }
    // 3. Recreate directories shallowest-first with accessible modes.
    for (rel, entry) in backups {
        if matches!(entry, WorkdirBackupEntry::Dir { .. }) {
            let target = workdir.join(rel);
            if let Err(err) =
                fs::create_dir_all(&target).and_then(|()| make_dir_accessible(&target))
            {
                errors.push(format!("{rel}: {err}"));
            }
        }
    }
    // 4. Restore backed-up files and their permissions.
    for (rel, entry) in backups {
        if let WorkdirBackupEntry::File { bytes, mode } = entry {
            let target = workdir.join(rel);
            if let Ok(meta) = fs::symlink_metadata(&target)
                && meta.file_type().is_dir()
                && let Err(err) = remove_dir_if_present(&target)
            {
                errors.push(format!("{rel}: {err}"));
                continue;
            }
            if let Err(err) = make_writable_if_exists(&target)
                .and_then(|()| fs::write(&target, bytes))
                .and_then(|()| restore_mode(&target, *mode))
            {
                errors.push(format!("{rel}: {err}"));
            }
        }
    }
    // 5. Restore original directory modes deepest-first.
    for (rel, entry) in backups.iter().rev() {
        if let WorkdirBackupEntry::Dir { mode } = entry
            && let Err(err) = restore_mode(&workdir.join(rel), *mode)
        {
            errors.push(format!("{rel}: {err}"));
        }
    }
    errors
}

fn ensure_no_symlink_components(workdir: &Path, rel_path: &str) -> Result<(), String> {
    let mut cur = workdir.to_path_buf();
    for component in Path::new(rel_path).components() {
        let std::path::Component::Normal(part) = component else {
            return Err(format!(
                "invalid path component in staged write: {rel_path}"
            ));
        };
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "refusing to promote write through symlink in workdir: {rel_path}"
                ));
            }
            Ok(meta) if !meta.file_type().is_dir() => break,
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                break;
            }
            Err(err) => {
                return Err(format!(
                    "failed to inspect workdir path before promoting {rel_path}: {err}"
                ));
            }
        }
    }
    Ok(())
}

fn entry_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        u32::from(meta.permissions().readonly())
    }
}

fn ensure_isolated_workspace(workdir: &Path, write_scope: &[String]) -> Result<(), String> {
    match fs::symlink_metadata(workdir.join(".storyflow")) {
        Ok(_) => {
            return Err(
                "workspace_write refused in directory containing .storyflow/; run Codex in an isolated worktree"
                    .to_owned(),
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "workspace_write refused: failed to inspect .storyflow isolation boundary: {err}"
            ));
        }
    }
    if write_scope.is_empty() {
        return Err(
            "workspace_write refused for task with empty write_scope; use ReadOnly policy"
                .to_owned(),
        );
    }
    Ok(())
}

struct WorkspaceSnapshot {
    entries: BTreeMap<String, SnapshotEntry>,
    backups: BTreeMap<String, Vec<u8>>,
    backup_bytes: usize,
    incomplete_backup: bool,
}

fn snapshot_workspace(
    root: &Path,
    capture_backups: bool,
    max_backup_bytes: usize,
) -> io::Result<WorkspaceSnapshot> {
    let mut snapshot = WorkspaceSnapshot {
        entries: BTreeMap::new(),
        backups: BTreeMap::new(),
        backup_bytes: 0,
        incomplete_backup: false,
    };
    collect_snapshot(root, root, capture_backups, max_backup_bytes, &mut snapshot)?;
    Ok(snapshot)
}

fn collect_snapshot(
    root: &Path,
    dir: &Path,
    capture_backups: bool,
    max_backup_bytes: usize,
    snapshot: &mut WorkspaceSnapshot,
) -> io::Result<()> {
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = child.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("workspace path escaped root"))?;
        let rel_str = rel_path_utf8(rel)?;
        if rel_str.is_empty() {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            let target = fs::read_link(&path)?;
            snapshot.entries.insert(
                rel_str,
                SnapshotEntry::Symlink(target.to_string_lossy().into_owned()),
            );
        } else if file_type.is_dir() {
            let dir_entry_idx = rel_str.clone();
            snapshot.entries.insert(
                rel_str,
                SnapshotEntry::Dir {
                    mode: entry_mode(&meta),
                },
            );
            match collect_snapshot(root, &path, capture_backups, max_backup_bytes, snapshot) {
                Ok(()) => {}
                Err(err) if !capture_backups && err.kind() == io::ErrorKind::PermissionDenied => {
                    snapshot
                        .entries
                        .insert(dir_entry_idx, SnapshotEntry::Unreadable);
                }
                Err(err) => return Err(err),
            }
        } else if file_type.is_file() {
            let mode = entry_mode(&meta);
            let mut file = match fs::File::open(&path) {
                Ok(file) => file,
                Err(err) if !capture_backups && err.kind() == io::ErrorKind::PermissionDenied => {
                    snapshot.entries.insert(rel_str, SnapshotEntry::Unreadable);
                    continue;
                }
                Err(err) => return Err(err),
            };
            let mut hasher = Sha256::new();
            let store_backup = capture_backups
                && usize::try_from(meta.len())
                    .ok()
                    .and_then(|len| snapshot.backup_bytes.checked_add(len))
                    .is_some_and(|total| total <= max_backup_bytes);
            if capture_backups && !store_backup {
                snapshot.incomplete_backup = true;
            }
            if store_backup {
                let mut bytes = Vec::with_capacity(meta.len() as usize);
                match file.read_to_end(&mut bytes) {
                    Ok(_) => {
                        hasher.update(&bytes);
                        let sha256: [u8; 32] = hasher.finalize().into();
                        snapshot.backup_bytes += bytes.len();
                        snapshot.backups.insert(rel_str.clone(), bytes);
                        snapshot.entries.insert(
                            rel_str,
                            SnapshotEntry::File {
                                len: meta.len(),
                                sha256,
                                mode,
                            },
                        );
                    }
                    Err(err)
                        if cfg!(windows)
                            && err.raw_os_error() == Some(33)
                            && rel_str.starts_with(".storyflow/")
                            && (rel_str.ends_with("/tasks.lock")
                                || rel_str.ends_with("/run.lock")) =>
                    {
                        snapshot.entries.insert(
                            rel_str,
                            SnapshotEntry::LockedFile {
                                len: meta.len(),
                                mode,
                            },
                        );
                    }
                    Err(err) => return Err(err),
                }
            } else {
                match io::copy(&mut file, &mut hasher) {
                    Ok(_) => {
                        let sha256: [u8; 32] = hasher.finalize().into();
                        snapshot.entries.insert(
                            rel_str,
                            SnapshotEntry::File {
                                len: meta.len(),
                                sha256,
                                mode,
                            },
                        );
                    }
                    Err(err)
                        if !capture_backups && err.kind() == io::ErrorKind::PermissionDenied =>
                    {
                        snapshot.entries.insert(rel_str, SnapshotEntry::Unreadable);
                    }
                    Err(err)
                        if cfg!(windows)
                            && err.raw_os_error() == Some(33)
                            && rel_str.starts_with(".storyflow/")
                            && (rel_str.ends_with("/tasks.lock")
                                || rel_str.ends_with("/run.lock")) =>
                    {
                        snapshot.entries.insert(
                            rel_str,
                            SnapshotEntry::LockedFile {
                                len: meta.len(),
                                mode,
                            },
                        );
                    }
                    Err(err) => return Err(err),
                }
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported special file in workspace: {rel_str}"),
            ));
        }
    }
    Ok(())
}

fn verify_and_restore_post_run(
    root: &Path,
    before: &WorkspaceSnapshot,
    policy: ExecutionPolicy,
    write_scope: &[String],
) -> Result<WorkspaceSnapshot, String> {
    let after = match snapshot_workspace(root, false, 0) {
        Ok(after) => after,
        Err(err) => {
            let rollback_errors = restore_all_before_entries(root, before);
            if rollback_errors.is_empty() {
                return Err(format!(
                    "failed to verify codex workspace changes (restored pre-run snapshot): {err}"
                ));
            }
            let err_sample = rollback_errors
                .into_iter()
                .take(8)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "failed to verify codex workspace changes (rollback failed: {err_sample}): {err}"
            ));
        }
    };
    let all_paths: BTreeSet<&String> = before.entries.keys().chain(after.entries.keys()).collect();
    let mut violations = Vec::new();
    for path in all_paths {
        let prev = before.entries.get(path);
        let curr = after.entries.get(path);
        if prev == curr {
            continue;
        }
        if policy == ExecutionPolicy::ReadOnly || is_protected_path(path) {
            violations.push(path.clone());
            continue;
        }
        let in_write_scope = write_scope.iter().any(|scope| within_path(path, scope));
        let created_parent_for_scope = prev.is_none()
            && matches!(curr, Some(SnapshotEntry::Dir { .. }))
            && write_scope.iter().any(|scope| within_path(scope, path));
        if !in_write_scope && !created_parent_for_scope {
            violations.push(path.clone());
        }
    }
    if violations.is_empty() {
        return Ok(after);
    }

    let rollback_errors = restore_violations(root, &violations, before, &after);
    let sample = violations
        .into_iter()
        .take(16)
        .collect::<Vec<_>>()
        .join(", ");
    if rollback_errors.is_empty() {
        Err(format!(
            "codex modified paths outside allowed scope ({policy:?}; restored): {sample}"
        ))
    } else {
        let err_sample = rollback_errors
            .into_iter()
            .take(8)
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!(
            "codex modified paths outside allowed scope ({policy:?}; rollback failed: {err_sample}): {sample}"
        ))
    }
}

fn restore_all_before_entries(root: &Path, before: &WorkspaceSnapshot) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(err) = make_dir_accessible(root) {
        errors.push(format!("root: {err}"));
    }
    let all_before: Vec<String> = before.entries.keys().cloned().collect();
    for path in &all_before {
        if let Some(SnapshotEntry::Dir { .. }) = before.entries.get(path) {
            let full = root.join(path);
            if let Err(err) = make_dir_accessible(&full) {
                errors.push(format!("{path}: {err}"));
            }
        }
    }
    errors.extend(restore_violations(
        root,
        &all_before,
        before,
        &WorkspaceSnapshot {
            entries: BTreeMap::new(),
            backups: BTreeMap::new(),
            backup_bytes: 0,
            incomplete_backup: false,
        },
    ));
    errors
}

fn restore_violations(
    root: &Path,
    violations: &[String],
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
) -> Vec<String> {
    let mut errors = Vec::new();

    // 0. Ensure root and traversed directories are writable/searchable before fixing entries.
    if let Err(err) = make_dir_accessible(root) {
        errors.push(format!("root: {err}"));
    }
    for path in violations {
        let full = root.join(path);
        if matches!(before.entries.get(path), Some(SnapshotEntry::Dir { .. }))
            || matches!(
                after.entries.get(path),
                Some(SnapshotEntry::Dir { .. } | SnapshotEntry::Unreadable)
            )
        {
            let _ = make_dir_accessible(&full);
        }
    }

    // 1. Remove added or type-changed non-directory entries first, then added directories deepest-first.
    for path in violations.iter().rev() {
        let prev = before.entries.get(path);
        let curr = after.entries.get(path);
        let full = root.join(path);
        match (prev, curr) {
            (None, Some(SnapshotEntry::File { .. } | SnapshotEntry::Symlink(_))) => {
                if let Err(err) = remove_file_or_symlink(&full) {
                    errors.push(format!("{path}: {err}"));
                }
            }
            (None, Some(SnapshotEntry::Unreadable)) => {
                let err = match fs::symlink_metadata(&full) {
                    Ok(meta) if meta.file_type().is_dir() => remove_dir_if_present(&full),
                    Ok(_) => remove_file_or_symlink(&full),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                };
                if let Err(err) = err {
                    errors.push(format!("{path}: {err}"));
                }
            }
            (
                Some(SnapshotEntry::Dir { .. }),
                Some(SnapshotEntry::File { .. } | SnapshotEntry::Symlink(_)),
            )
            | (
                Some(SnapshotEntry::File { .. } | SnapshotEntry::Symlink(_)),
                Some(SnapshotEntry::Dir { .. }),
            ) => {
                if let Err(err) = remove_entry_any(&full, curr) {
                    errors.push(format!("{path}: {err}"));
                }
            }
            (None, Some(SnapshotEntry::Dir { .. })) => {
                if let Err(err) = remove_dir_if_present(&full) {
                    errors.push(format!("{path}: {err}"));
                }
            }
            _ => {}
        }
    }

    // 2. Recreate deleted/replaced directories shallowest-first with accessible permissions first.
    for path in violations {
        if let Some(SnapshotEntry::Dir { .. }) = before.entries.get(path) {
            let full = root.join(path);
            if let Err(err) = fs::create_dir_all(&full).and_then(|()| make_dir_accessible(&full)) {
                errors.push(format!("{path}: {err}"));
            }
        }
    }

    // 3. Restore modified/deleted/unreadable files and symlinks.
    for path in violations {
        let full = root.join(path);
        match before.entries.get(path) {
            Some(SnapshotEntry::File { mode, .. }) => {
                let Some(bytes) = before.backups.get(path) else {
                    errors.push(format!("{path}: original file exceeded backup limit"));
                    continue;
                };
                if let Some(parent) = full.parent()
                    && let Err(err) = fs::create_dir_all(parent)
                {
                    errors.push(format!("{path}: {err}"));
                    continue;
                }
                if let Err(err) = make_writable_if_exists(&full)
                    .and_then(|()| fs::write(&full, bytes))
                    .and_then(|()| restore_mode(&full, *mode))
                {
                    errors.push(format!("{path}: {err}"));
                }
            }
            Some(SnapshotEntry::Symlink(target)) => {
                #[cfg(unix)]
                {
                    if let Err(err) = remove_file_or_symlink(&full)
                        .or_else(|e| {
                            if e.kind() == io::ErrorKind::NotFound {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        })
                        .and_then(|()| std::os::unix::fs::symlink(target, &full))
                    {
                        errors.push(format!("{path}: {err}"));
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = target;
                    errors.push(format!("{path}: cannot restore symlink on non-Unix host"));
                }
            }
            Some(
                SnapshotEntry::LockedFile { .. }
                | SnapshotEntry::Dir { .. }
                | SnapshotEntry::Unreadable,
            )
            | None => {}
        }
    }

    // 4. Restore original directory modes deepest-first once children have been restored.
    for path in violations.iter().rev() {
        if let Some(SnapshotEntry::Dir { mode }) = before.entries.get(path) {
            let full = root.join(path);
            if let Err(err) = restore_mode(&full, *mode) {
                errors.push(format!("{path}: {err}"));
            }
        }
    }

    errors
}

fn make_dir_accessible(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o700);
                fs::set_permissions(path, perms)
            }
            #[cfg(not(unix))]
            {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    fs::set_permissions(path, perms)?;
                }
                Ok(())
            }
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn make_writable_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o200);
                fs::set_permissions(path, perms)
            }
            #[cfg(not(unix))]
            {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    fs::set_permissions(path, perms)?;
                }
                Ok(())
            }
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_file_or_symlink(path: &Path) -> io::Result<()> {
    make_writable_if_exists(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o700);
                let _ = fs::set_permissions(path, perms);
            }
            fs::remove_dir_all(path)
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_entry_any(path: &Path, entry: Option<&SnapshotEntry>) -> io::Result<()> {
    match entry {
        Some(SnapshotEntry::Dir { .. }) => remove_dir_if_present(path),
        _ => remove_file_or_symlink(path),
    }
}

fn restore_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms)
    }
    #[cfg(not(unix))]
    {
        let meta = fs::symlink_metadata(path)?;
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(mode != 0);
        fs::set_permissions(path, perms)
    }
}

fn rel_path_utf8(rel: &Path) -> io::Result<String> {
    let mut parts = Vec::new();
    for component in rel.components() {
        let Some(seg) = component.as_os_str().to_str() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-UTF-8 path component in workspace",
            ));
        };
        parts.push(seg);
    }
    Ok(parts.join("/"))
}

fn fold_path(value: &str) -> String {
    value.nfc().flat_map(char::to_lowercase).collect()
}

fn within_path(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_protected_path(path: &str) -> bool {
    let folded = fold_path(path);
    path.split('/').any(|seg| fold_path(seg) == ".git")
        || BUILTIN_PROTECTED
            .iter()
            .any(|protected| within_path(&folded, &fold_path(protected)))
}

fn is_windows_batch(path: &Path) -> bool {
    cfg!(windows)
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
}

fn write_exclusive_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

fn read_last_message_file(path: &Path) -> Result<Option<String>, String> {
    const MAX_LAST_MESSAGE_BYTES: u64 = 65_536;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to inspect codex last message file: {err}")),
    };
    if !meta.file_type().is_file() {
        return Err("codex last message path is not a regular file".to_owned());
    }
    if meta.len() > MAX_LAST_MESSAGE_BYTES {
        return Err("codex last message file exceeds size limit".to_owned());
    }
    let file = fs::File::open(path)
        .map_err(|err| format!("failed to open codex last message file: {err}"))?;
    let mut buf = Vec::new();
    file.take(MAX_LAST_MESSAGE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| format!("failed to read codex last message file: {err}"))?;
    if buf.len() as u64 > MAX_LAST_MESSAGE_BYTES {
        return Err("codex last message file exceeds size limit".to_owned());
    }
    let text = String::from_utf8(buf)
        .map_err(|err| format!("codex last message file is not valid UTF-8: {err}"))?;
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

struct TempArtifactsDir(PathBuf);

impl TempArtifactsDir {
    fn new() -> io::Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let mut hasher = Sha256::new();
            hasher.update(std::process::id().to_le_bytes());
            hasher.update(TEMP_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            hasher.update(nanos.to_le_bytes());
            let mut rng_hasher = RandomState::new().build_hasher();
            rng_hasher.write_u64(nanos as u64);
            hasher.update(rng_hasher.finish().to_le_bytes());
            let digest = hasher.finalize();
            let mut suffix = String::with_capacity(32);
            for byte in &digest[..16] {
                use std::fmt::Write as _;
                let _ = write!(&mut suffix, "{byte:02x}");
            }
            let dir = base.join(format!("storyflow-codex-{suffix}"));
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "failed to allocate unique codex artifact directory",
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempArtifactsDir {
    fn drop(&mut self) {
        let _ = make_dir_accessible(&self.0);
        let _ = remove_dir_if_present(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_staged_writes_rolls_back_partial_promotion_on_failure() {
        let staging = TempArtifactsDir::new().unwrap();
        let workdir = TempArtifactsDir::new().unwrap();

        fs::create_dir_all(workdir.path().join("app")).unwrap();
        fs::write(workdir.path().join("app/first.txt"), b"original\n").unwrap();

        fs::create_dir_all(staging.path().join("app")).unwrap();
        fs::write(staging.path().join("app/first.txt"), b"original\n").unwrap();
        let before = snapshot_workspace(staging.path(), true, MAX_BACKUP_BYTES).unwrap();

        fs::write(staging.path().join("app/first.txt"), b"partial mutation\n").unwrap();
        fs::write(staging.path().join("app/new.txt"), b"newly created\n").unwrap();
        fs::write(staging.path().join("app/second.txt"), b"triggers failure\n").unwrap();
        let after = snapshot_workspace(staging.path(), false, 0).unwrap();

        let write_scope = vec![
            "app/first.txt".to_owned(),
            "app/new.txt".to_owned(),
            "app/second.txt".to_owned(),
        ];
        let mut saw_first_write = false;
        let err = apply_staged_writes_with(
            staging.path(),
            workdir.path(),
            &before,
            &after,
            &write_scope,
            |rel| {
                if rel == "app/first.txt" {
                    saw_first_write = true;
                }
                if rel == "app/second.txt" {
                    return Err("injected mid-batch failure".to_owned());
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(saw_first_write);
        assert!(err.contains("rolled back"), "{err}");
        assert_eq!(
            fs::read_to_string(workdir.path().join("app/first.txt")).unwrap(),
            "original\n",
            "modified file in failed batch must be rolled back"
        );
        assert!(
            !workdir.path().join("app/new.txt").exists(),
            "newly created file in failed batch must be removed on rollback"
        );
    }

    #[test]
    fn config_toml_detects_legacy_sandbox_keys() {
        assert!(!config_toml_sets_legacy_sandbox(
            "# sandbox_mode = \"workspace-write\"\ndefault_permissions = \":workspace\"\n[permissions.other]\n\":minimal\" = \"read\"\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "model = \"gpt-5\"\nsandbox_mode = \"workspace-write\"\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "'sandbox_mode' = 'workspace-write'\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "\"sandbox_mode\" = \"read-only\"\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "[sandbox_workspace_write]\nnetwork_access = false\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "sandbox_workspace_write.writable_roots = [\"/tmp\"]\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "[permissions.storyflow_scoped]\n\":root\" = \"read\"\n"
        ));
        assert!(config_toml_sets_legacy_sandbox(
            "[permissions.'storyflow_scoped_abc']\n\"/\" = \"read\"\n"
        ));
        assert!(config_toml_sets_legacy_sandbox("\"\"\"multiline\"\"\"\n"));
        assert!(config_toml_sets_legacy_sandbox(
            "unrecognized syntax here\n"
        ));
    }

    #[test]
    fn minimal_runner_defaults_reject_unverified_confinement_and_containment() {
        struct MinimalRunner;
        impl CodexCommandRunner for MinimalRunner {
            fn resolve_executable(&self, _configured: Option<&Path>) -> Result<PathBuf, String> {
                Ok(PathBuf::from("codex"))
            }

            fn run(
                &self,
                _spec: &HostCommandSpec,
                _cwd: &Path,
                _should_stop: &dyn Fn() -> bool,
            ) -> Result<ControlledResult, ProcessError> {
                unreachable!()
            }
        }

        let runner = MinimalRunner;
        assert!(!runner.has_unbroken_containment());
        assert!(!runner.has_read_root_confinement());
    }
}
