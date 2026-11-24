//! Codex host adapter using capability probing and supervised subprocess execution.
use super::{
    ADAPTER_SCHEMA, AdapterError, AdapterOutcome, AdapterRequest, AdapterResult, CancelToken,
    ExecutionPolicy, HostAdapter, HostCapabilities, MAX_SESSION_ID_BYTES, SessionCorrelation,
    sanitize_reason,
};
use crate::{
    security::process::{
        ControlledResult, HostCommandSpec, ProcessError, host_executable, run_host_command,
    },
    tasks::{Artifact, ResultStatus, SCHEMA, WorkerResult},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 5;
const MAX_PROMPT_BYTES: usize = 32_768;

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
    "--json",
    "--output-schema",
    "--output-last-message",
    "-o",
    "--auto-merge",
    "--push",
];

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
                if !path.is_file() {
                    return Err(format!(
                        "configured codex executable not found: {}",
                        path.display()
                    ));
                }
                path.to_path_buf()
            }
            None => {
                host_executable("codex").map_err(|e| format!("codex not found on PATH: {e}"))?
            }
        };
        if cfg!(windows)
            && path.extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd")
            })
        {
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
            if FORBIDDEN_FLAGS
                .iter()
                .any(|f| flag_name.eq_ignore_ascii_case(f) || arg.eq_ignore_ascii_case(f))
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
        if !caps.available {
            return AdapterOutcome::HostUnavailable {
                provider: "codex".to_owned(),
                detail: caps
                    .unavailable_reason
                    .unwrap_or_else(|| "codex host is unavailable".to_owned()),
            };
        }

        if cancel.is_cancelled() {
            return AdapterOutcome::Cancelled {
                correlation: fallback_correlation,
                worker_stopped: true,
                reason: "cancelled before launching codex".to_owned(),
            };
        }

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
            && let Err(err) = fs::write(&schema_path, output_schema_json())
        {
            return AdapterOutcome::HostCrashed {
                correlation: fallback_correlation,
                worker_stopped: true,
                exit_code: None,
                detail: format!("failed to write codex output schema: {err}"),
            };
        }

        let prompt = build_prompt(request);
        let mut args = vec!["exec".to_owned()];
        if caps.supports_working_dir {
            args.push("--cd".to_owned());
            args.push(request.workdir.to_string_lossy().into_owned());
        }
        if caps.supports_sandbox_flag {
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

        if controlled.cancelled {
            return AdapterOutcome::Cancelled {
                correlation,
                worker_stopped: true,
                reason: "cancelled by host request".to_owned(),
            };
        }

        if controlled.process.timed_out {
            return AdapterOutcome::TimedOut {
                correlation,
                worker_stopped: true,
                reason: "codex execution timed out".to_owned(),
            };
        }

        if !controlled.process.status.is_some_and(|s| s.success()) {
            let exit_code = controlled.process.status.and_then(|s| s.code());
            let detail_source = if !stderr_text.trim().is_empty() {
                stderr_text.trim()
            } else {
                stdout_text.trim()
            };
            return AdapterOutcome::HostCrashed {
                correlation,
                worker_stopped: true,
                exit_code,
                detail: sanitize_reason(&format!("codex exited unsuccessfully: {detail_source}")),
            };
        }

        let payload = if caps.supports_output_file && output_path.is_file() {
            match fs::read_to_string(&output_path) {
                Ok(text) if !text.trim().is_empty() => Some(text),
                Ok(_) => extract_message_from_stdout(&stdout_text, caps.supports_json_output),
                Err(err) => {
                    return AdapterOutcome::InvalidOutput {
                        correlation,
                        worker_stopped: true,
                        detail: format!("failed to read codex last message file: {err}"),
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
        available: true,
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
        unavailable_reason: None,
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

fn build_prompt(request: &AdapterRequest) -> String {
    let packet = &request.dispatch.packet;
    let task = &packet.task;
    let mut prompt = format!(
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
        prompt.push_str("- ");
        prompt.push_str(ac);
        prompt.push('\n');
    }
    if !packet.prerequisites.is_empty() {
        prompt.push_str("Completed prerequisites:\n");
        for (dep_id, dep_res) in &packet.prerequisites {
            prompt.push_str(&format!(
                "- {dep_id}: {:?} ({})\n",
                dep_res.status,
                dep_res.commit.as_deref().unwrap_or("no-commit")
            ));
        }
    }
    prompt.push_str(&format!(
        "\nConstraints:\n\
         - Modify only files inside write_scope.\n\
         - Never modify .storyflow/ or protected paths.\n\
         - Never run git push, auto-merge, or claim story approval.\n\
         - Return ONLY a JSON object with fields: story (\"{}\"), task (\"{}\"), task_revision ({}), status (\"succeeded\"|\"failed\"|\"blocked\"), report (<=4096 bytes), commit (null or 40/64 hex), artifacts (array of {{path, sha256}}).\n",
        packet.story, task.spec.id, task.revision
    ));
    if prompt.len() > MAX_PROMPT_BYTES {
        let mut end = MAX_PROMPT_BYTES;
        while !prompt.is_char_boundary(end) {
            end -= 1;
        }
        prompt.truncate(end);
    }
    prompt
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

struct TempArtifactsDir(PathBuf);

impl TempArtifactsDir {
    fn new() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "storyflow-codex-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
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
