//! Codex host adapter using capability probing and supervised subprocess execution.
use super::{
    ADAPTER_SCHEMA, AdapterError, AdapterOutcome, AdapterRequest, AdapterResult, CancelToken,
    ExecutionPolicy, HostAdapter, HostCapabilities, MAX_SESSION_ID_BYTES, SessionCorrelation,
    sanitize_reason,
};
use crate::{
    config::BUILTIN_PROTECTED,
    security::process::{
        ControlledResult, HostCommandSpec, ProcessError, host_executable, run_host_command,
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

        parse_exec_help(&exe_str, version, &help_text)
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

        let before_snapshot =
            match snapshot_workspace(&request.workdir, true, self.config.max_backup_bytes) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    return AdapterOutcome::HostCrashed {
                        correlation: fallback_correlation,
                        worker_stopped: true,
                        exit_code: None,
                        detail: sanitize_reason(&format!(
                            "failed to snapshot codex workspace before execution: {err}"
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

        let mut args = vec!["exec".to_owned()];
        if caps.supports_working_dir {
            args.push("--cd".to_owned());
            args.push(request.workdir.to_string_lossy().into_owned());
        }
        args.push("--sandbox".to_owned());
        args.push(match request.policy {
            ExecutionPolicy::ReadOnly => "read-only".to_owned(),
            ExecutionPolicy::WorkspaceWrite => "workspace-write".to_owned(),
        });
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
            .run(&spec, &request.workdir, &|| cancel.is_cancelled())
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

        let stdout_text = String::from_utf8_lossy(&controlled.process.stdout);
        let stderr_text = String::from_utf8_lossy(&controlled.process.stderr);
        let session_id = extract_session_id(&stdout_text)
            .unwrap_or_else(|| fallback_correlation.session_id.clone());
        let correlation = SessionCorrelation::from_lease(&request.dispatch.lease, session_id);

        let scope_check = verify_and_restore_post_run(
            &request.workdir,
            &before_snapshot,
            request.policy,
            &request.dispatch.packet.task.spec.write_scope,
        );

        if controlled.cancelled {
            let reason = match scope_check {
                Ok(()) => "cancelled by host request".to_owned(),
                Err(msg) => sanitize_reason(&format!("cancelled by host request ({msg})")),
            };
            return AdapterOutcome::Cancelled {
                correlation,
                worker_stopped: true,
                reason,
            };
        }

        if controlled.process.timed_out {
            let reason = match scope_check {
                Ok(()) => "codex execution timed out".to_owned(),
                Err(msg) => sanitize_reason(&format!("codex execution timed out ({msg})")),
            };
            return AdapterOutcome::TimedOut {
                correlation,
                worker_stopped: true,
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
                Ok(()) => sanitize_reason(&base),
                Err(msg) => sanitize_reason(&format!("{base}; {msg}")),
            };
            return AdapterOutcome::HostCrashed {
                correlation,
                worker_stopped: true,
                exit_code,
                detail,
            };
        }

        if let Err(detail) = scope_check {
            return AdapterOutcome::InvalidOutput {
                correlation,
                worker_stopped: true,
                detail: sanitize_reason(&detail),
            };
        }

        let payload = if caps.supports_output_file {
            match read_last_message_file(&output_path) {
                Ok(Some(text)) => Some(text),
                Ok(None) => extract_message_from_stdout(&stdout_text, caps.supports_json_output),
                Err(detail) => {
                    return AdapterOutcome::InvalidOutput {
                        correlation,
                        worker_stopped: true,
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
                worker_stopped: true,
                detail: "codex produced no structured result payload".to_owned(),
            };
        };

        let envelope: CodexResultEnvelope = match serde_json::from_str(raw_json.trim()) {
            Ok(parsed) => parsed,
            Err(err) => {
                return AdapterOutcome::InvalidOutput {
                    correlation,
                    worker_stopped: true,
                    detail: sanitize_reason(&format!("invalid codex JSON result: {err}")),
                };
            }
        };

        AdapterOutcome::Completed {
            correlation,
            result: WorkerResult {
                schema: SCHEMA,
                story: envelope.story,
                task: envelope.task,
                task_revision: envelope.task_revision,
                status: envelope.status,
                report: envelope.report,
                commit: envelope.commit,
                artifacts: envelope.artifacts,
            },
            worker_stopped: true,
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

fn parse_exec_help(exe: &str, version: Option<String>, help_text: &str) -> HostCapabilities {
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
        supports_cancel: true,
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
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
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
            snapshot.entries.insert(
                rel_str,
                SnapshotEntry::Dir {
                    mode: entry_mode(&meta),
                },
            );
            collect_snapshot(root, &path, capture_backups, max_backup_bytes, snapshot)?;
        } else if file_type.is_file() {
            let mode = entry_mode(&meta);
            let mut file = fs::File::open(&path)?;
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
) -> Result<(), String> {
    let after = snapshot_workspace(root, false, 0)
        .map_err(|err| format!("failed to verify codex workspace changes: {err}"))?;
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
        let folded_path = fold_path(path);
        let in_write_scope = write_scope
            .iter()
            .any(|scope| within_path(&folded_path, &fold_path(scope)));
        let created_parent_for_scope = prev.is_none()
            && matches!(curr, Some(SnapshotEntry::Dir { .. }))
            && write_scope
                .iter()
                .any(|scope| within_path(&fold_path(scope), &folded_path));
        if !in_write_scope && !created_parent_for_scope {
            violations.push(path.clone());
        }
    }
    if violations.is_empty() {
        return Ok(());
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

fn restore_violations(
    root: &Path,
    violations: &[String],
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
) -> Vec<String> {
    let mut errors = Vec::new();

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

    // 2. Recreate deleted/replaced directories shallowest-first and restore dir permissions.
    for path in violations {
        if let Some(SnapshotEntry::Dir { mode }) = before.entries.get(path) {
            let full = root.join(path);
            if let Err(err) = fs::create_dir_all(&full).and_then(|()| restore_mode(&full, *mode)) {
                errors.push(format!("{path}: {err}"));
            }
        }
    }

    // 3. Restore modified/deleted files and symlinks.
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
            Some(SnapshotEntry::LockedFile { .. } | SnapshotEntry::Dir { .. }) | None => {}
        }
    }

    errors
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
        let _ = fs::remove_dir_all(&self.0);
    }
}
