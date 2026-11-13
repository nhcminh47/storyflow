//! Regression tests run real worker requests through the Rust CLI boundary.
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const ID: &str = "security-001";
const TASK: &str = "worker";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    base: String,
    lease: Value,
}
impl Fixture {
    fn new(grants: &[&str]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "storyflow-security-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let mut f = Self {
            root,
            repo,
            base: String::new(),
            lease: Value::Null,
        };
        f.git(&["init", "-q"]);
        f.git(&["config", "user.name", "Runtime Tests"]);
        f.git(&["config", "user.email", "runtime@example.invalid"]);
        f.git(&["config", "commit.gpgsign", "false"]);
        f.git(&["config", "core.autocrlf", "false"]);
        f.git(&["config", "core.hooksPath", ".git/no-hooks"]);
        f.write(".gitignore", ".storyflow/\nreport.md\n");
        f.write("report.md", "AC1 security fixture\n");
        f.write("app/value.txt", "pinned value\n");
        f.write("app/value.txt-suffix", "sibling value\n");
        f.write("app/Case.txt", "case-sensitive value\n");
        f.write("app/é.txt", "canonical Unicode value\n");
        f.write("app/private/secret.txt", "protected value\n");
        f.write(
            "stories/security-001/story.md",
            "# Runtime security\nAC1: bounded authority\n",
        );
        f.document("stories/security-001/story.json", json!({"scope":["app"]}));
        for role in ["planner", "implementer", "reviewer"] {
            f.write(&format!("agents/{role}.md"), &format!("# {role}\n"));
        }
        f.document("storyflow.json", json!({
            "version":2,"scope":"repo", "agents":[
                {"id":"planner","source":"agents/planner.md","version":"1"},
                {"id":"implementer","source":"agents/implementer.md","version":"1"},
                {"id":"reviewer","source":"agents/reviewer.md","version":"1"}
            ], "tools":{"allow":grants}, "policy":{
                "protected":["app/private"], "checks":[{
                    "id":"probe", "program":std::env::current_exe().unwrap(),
                    "args":["--exact","runtime_gate_helper","--nocapture","--skip",
                        format!("sf-security-gate-marker={}", f.root.join("gate-ran").display())],
                    "timeout_seconds":15
                }]
            }
        }));
        f.base = f.commit();
        f
    }
    fn write(&self, relative: &str, content: &str) {
        let path = self.repo.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn document(&self, relative: &str, document: Value) {
        self.write(relative, &serde_json::to_string(&document).unwrap());
    }
    fn git_raw(&self, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args(args)
            .output()
            .unwrap()
    }
    fn git(&self, args: &[&str]) -> String {
        let out = self.git_raw(args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn commit(&self) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "-qm", "security fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn command(&self, binary: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(binary);
        cmd.current_dir(&self.repo)
            .args(args)
            .env_remove("STORYFLOW_HOST_CONTEXT")
            .env_remove("STORYFLOW_HOST_CONTEXT_ROOT");
        cmd
    }
    fn flow_raw(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_storyflow"), args)
            .output()
            .unwrap()
    }
    fn parse(out: Output, exit: i32) -> Value {
        assert_eq!(
            out.status.code(),
            Some(exit),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON ({error}); stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }
    fn flow(&self, args: &[&str]) -> Value {
        Self::parse(self.flow_raw(args), 0)
    }
    fn tasks(&self, args: &[&str]) -> Value {
        Self::parse(
            self.command(env!("CARGO_BIN_EXE_storyflow-tasks"), args)
                .output()
                .unwrap(),
            0,
        )
    }
    fn start(&mut self) {
        self.start_with_scopes(&["app"], &["app"], 60_000);
    }
    fn start_with_scopes(&mut self, read: &[&str], write: &[&str], lease_ms: u64) {
        assert_eq!(
            self.flow(&["init", ID, "HEAD", "--managed-runtime"])["revision"],
            0
        );
        self.document("graph.json", json!({
            "schema":1,"story":ID,"base":self.base,"story_revision":0,"stage":"plan",
            "tasks":[{"id":TASK,"story":ID,"stage":"plan","prerequisites":[],
                "read_scope":read,"write_scope":write,"acceptance_criteria":["AC1"],"decision_owner":"orchestrator"}]
        }));
        self.document(
            "scheduler.json",
            json!({"max_concurrency":1,"max_attempts":2,"lease_ms":lease_ms}),
        );
        self.tasks(&["init", ID, "graph.json"]);
        self.tasks(&["configure", ID, "0", "scheduler.json"]);
        let view = self.tasks(&["view", ID]);
        let revision = view["revision"].as_u64().unwrap().to_string();
        let task_revision = view["tasks"][TASK]["revision"]
            .as_u64()
            .unwrap()
            .to_string();
        self.lease = self.tasks(&[
            "dispatch",
            ID,
            &revision,
            TASK,
            &task_revision,
            "PRIVATE_OWNER_SENTINEL",
        ])["lease"]
            .clone();
    }
    fn request(&self, operation: Value) -> Value {
        json!({"schema":1,"lease":self.lease,"operation":operation})
    }
    fn runtime_raw(&self, request: &Value) -> Output {
        self.runtime_command(
            self.command(env!("CARGO_BIN_EXE_storyflow-runtime"), &[ID, TASK]),
            request,
        )
    }
    fn runtime_command(&self, mut cmd: Command, request: &Value) -> Output {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(request).unwrap())
            .unwrap();
        child.wait_with_output().unwrap()
    }
    fn allowed(&self, operation: Value) -> Value {
        let value = Self::parse(self.runtime_raw(&self.request(operation)), 0);
        assert_eq!(value["ok"], true);
        value
    }
    fn denied(&self, operation: Value) -> Value {
        assert_error(self.runtime_raw(&self.request(operation)))
    }
    fn snapshot(&self) -> PathBuf {
        self.repo
            .join(format!(".storyflow/{ID}/config-snapshot.json"))
    }
    fn audit(&self) -> String {
        fs::read_to_string(
            self.repo
                .join(format!(".storyflow/{ID}/security-events.jsonl")),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert_eq!(self.root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(
            self.root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("storyflow-security-")
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn assert_error(out: Output) -> Value {
    let value = Fixture::parse(out, 2);
    assert_eq!(value["ok"], false);
    assert!(value["error"]["code"].is_string());
    value
}
fn shell_probe(f: &Fixture) -> Value {
    json!({"op":"shell","program":std::env::current_exe().unwrap(),"args":["--exact","runtime_gate_helper","--nocapture","--skip",format!("sf-security-gate-marker={}", f.root.join("gate-ran").display())]})
}

#[test]
fn runtime_gate_helper() {
    for argument in std::env::args() {
        if let Some(path) = argument.strip_prefix("sf-security-gate-marker=") {
            fs::write(path, "gate executed").unwrap();
        }
    }
}

#[test]
fn authorized_files_and_git_reads_use_verified_scopes_and_pinned_baseline() {
    let mut f = Fixture::new(&["read", "write", "git"]);
    f.start();
    assert_eq!(
        f.allowed(json!({"op":"read","path":"app/value.txt"}))["result"]["content"],
        "pinned value\n"
    );
    f.allowed(json!({"op":"write","path":"app/value.txt","content":"managed edit\n"}));
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "managed edit\n"
    );
    assert_eq!(
        f.allowed(json!({"op":"read","path":"app/value.txt"}))["result"]["content"],
        "managed edit\n"
    );
    assert_eq!(
        f.allowed(json!({"op":"git_read","path":"app/value.txt"}))["result"]["content"],
        "pinned value\n"
    );
}

#[test]
fn missing_capabilities_deny_read_write_git_and_shell_without_side_effects() {
    let mut f = Fixture::new(&[]);
    f.start();
    for operation in [
        json!({"op":"read","path":"app/value.txt"}),
        json!({"op":"write","path":"app/value.txt","content":"UNAUTHORIZED_WRITE_SENTINEL"}),
        json!({"op":"git","args":["update-ref","refs/heads/runtime-injected",f.base]}),
        shell_probe(&f),
    ] {
        assert_eq!(f.denied(operation)["error"]["code"], "capability_denied");
    }
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
    assert!(
        !f.git_raw(&["show-ref", "--verify", "refs/heads/runtime-injected"])
            .status
            .success()
    );
    assert!(!f.root.join("gate-ran").exists());
    assert!(!f.audit().contains("UNAUTHORIZED_WRITE_SENTINEL"));
    assert!(!f.audit().contains("PRIVATE_OWNER_SENTINEL"));
}

#[test]
fn managed_backend_refuses_arbitrary_commands_even_with_granted_capabilities() {
    let mut f = Fixture::new(&["read", "write", "git", "shell"]);
    f.start();
    for operation in [
        json!({"op":"git","args":["update-ref","refs/heads/runtime-injected",f.base]}),
        shell_probe(&f),
    ] {
        assert_eq!(
            f.denied(operation)["error"]["code"],
            "unsupported_managed_operation"
        );
    }
    assert!(
        !f.git_raw(&["show-ref", "--verify", "refs/heads/runtime-injected"])
            .status
            .success()
    );
    assert!(!f.root.join("gate-ran").exists());
}

#[test]
fn ambient_config_and_request_fields_cannot_escalate_pinned_permissions() {
    let mut f = Fixture::new(&["read"]);
    f.start();
    let mut config: Value =
        serde_json::from_slice(&fs::read(f.repo.join("storyflow.json")).unwrap()).unwrap();
    config["tools"] = json!({"allow":["read","write","git","shell"]});
    f.document("storyflow.json", config);
    let operation =
        json!({"op":"write","path":"app/value.txt","content":"ESCALATION_SECRET_SENTINEL"});
    assert_eq!(
        f.denied(operation.clone())["error"]["code"],
        "capability_denied"
    );
    let mut request = f.request(operation);
    request["capabilities"] = json!(["read", "write", "git", "shell"]);
    assert_error(f.runtime_raw(&request));
    let mut cmd = f.command(env!("CARGO_BIN_EXE_storyflow-runtime"), &[ID, TASK]);
    cmd.env("STORYFLOW_HOST_CONTEXT", f.root.join("missing-host.json"))
        .env("STORYFLOW_HOST_CONTEXT_ROOT", &f.root);
    let value = Fixture::parse(
        f.runtime_command(cmd, &f.request(json!({"op":"read","path":"app/value.txt"}))),
        0,
    );
    assert_eq!(value["result"]["content"], "pinned value\n");
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
    assert!(!f.audit().contains("ESCALATION_SECRET_SENTINEL"));
}

#[test]
fn snapshot_tampering_refuses_a_write_before_touching_the_target() {
    let mut f = Fixture::new(&["read", "write"]);
    f.start();
    let mut snapshot: Value = serde_json::from_slice(&fs::read(f.snapshot()).unwrap()).unwrap();
    snapshot["effective"]["capabilities"]["granted"] = json!(["read", "write", "git", "shell"]);
    fs::write(f.snapshot(), serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let error = f.denied(json!({"op":"write","path":"app/value.txt","content":"tampered"}));
    assert_eq!(error["error"]["code"], "snapshot_integrity");
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
}

#[test]
fn traversal_aliases_and_protected_descendants_cannot_reach_files() {
    let mut f = Fixture::new(&["read", "write", "git"]);
    f.start();
    fs::write(f.root.join("outside.txt"), "outside sentinel").unwrap();
    for path in [
        "../outside.txt",
        "app/../../outside.txt",
        "app/./value.txt",
        "app//value.txt",
        "app\\value.txt",
        "/app/value.txt",
        "app/value.txt:stream",
        "app/CASE.txt",
        "app/e\u{301}.txt",
        "app/CON",
        "app/value.txt.",
        "app/value.txt ",
        "app/private/secret.txt",
        "app/PRIVATE/new.txt",
        ".git/config",
        "agents/planner.md",
    ] {
        f.denied(json!({"op":"write","path":path,"content":"must not write"}));
    }
    assert_eq!(
        fs::read_to_string(f.root.join("outside.txt")).unwrap(),
        "outside sentinel"
    );
    for (path, original) in [
        ("app/Case.txt", "case-sensitive value\n"),
        ("app/é.txt", "canonical Unicode value\n"),
        ("app/private/secret.txt", "protected value\n"),
    ] {
        assert_eq!(fs::read_to_string(f.repo.join(path)).unwrap(), original);
    }
    assert!(!f.repo.join("app/private/new.txt").exists());
}

#[test]
fn exact_file_scopes_do_not_authorize_siblings_with_the_same_prefix() {
    let mut f = Fixture::new(&["read", "write", "git"]);
    f.start_with_scopes(&["app/value.txt"], &["app/value.txt"], 60_000);
    for operation in [
        json!({"op":"read","path":"app/value.txt-suffix"}),
        json!({"op":"write","path":"app/value.txt-suffix","content":"must not write"}),
        json!({"op":"git_read","path":"app/value.txt-suffix"}),
    ] {
        f.denied(operation);
    }
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt-suffix")).unwrap(),
        "sibling value\n"
    );
}

#[test]
fn pinned_submodule_boundaries_cannot_be_crossed_by_unmarked_directories() {
    let mut f = Fixture::new(&["read", "write", "git"]);
    f.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},app/dependency", f.base),
    ]);
    f.git(&["commit", "-qm", "add pinned gitlink"]);
    f.base = f.git(&["rev-parse", "HEAD"]);
    f.start();
    f.write("app/dependency/value.txt", "submodule sentinel\n");
    for operation in [
        json!({"op":"read","path":"app/dependency/value.txt"}),
        json!({"op":"write","path":"app/dependency/value.txt","content":"must not write"}),
        json!({"op":"git_read","path":"app/dependency"}),
        json!({"op":"git_read","path":"app/dependency/value.txt"}),
    ] {
        f.denied(operation);
    }
    assert_eq!(
        fs::read_to_string(f.repo.join("app/dependency/value.txt")).unwrap(),
        "submodule sentinel\n"
    );
}

#[test]
fn forged_expired_and_cancelled_leases_cannot_authorize_operations() {
    let mut f = Fixture::new(&["read", "write"]);
    f.start();
    let mut request =
        f.request(json!({"op":"write","path":"app/value.txt","content":"must not write"}));
    for field in ["attempt_id", "lease_id", "owner"] {
        request["lease"] = f.lease.clone();
        request["lease"][field] = json!("forged");
        assert_error(f.runtime_raw(&request));
    }
    let view = f.tasks(&["view", ID]);
    let revision = view["revision"].as_u64().unwrap().to_string();
    let task_revision = view["tasks"][TASK]["revision"]
        .as_u64()
        .unwrap()
        .to_string();
    f.tasks(&[
        "cancel",
        ID,
        &revision,
        TASK,
        &task_revision,
        "cancel fixture",
    ]);
    f.denied(json!({"op":"write","path":"app/value.txt","content":"must not write"}));
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
    let mut expired = Fixture::new(&["read", "write"]);
    expired.start_with_scopes(&["app"], &["app"], 1);
    std::thread::sleep(Duration::from_millis(30));
    expired.denied(json!({"op":"write","path":"app/value.txt","content":"must not write"}));
    assert_eq!(
        fs::read_to_string(expired.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
}

#[test]
fn changing_story_revision_fences_an_old_runtime_lease() {
    let mut f = Fixture::new(&["read", "write"]);
    f.start();
    f.flow(&["submit", ID, "0", "plan", "report.md"]);
    f.denied(json!({"op":"write","path":"app/value.txt","content":"must not write"}));
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
}

#[test]
fn audit_excludes_request_paths_contents_arguments_and_owner_secrets() {
    let mut f = Fixture::new(&["read", "write", "git", "shell"]);
    f.start();
    f.allowed(json!({"op":"write","path":"app/PRIVATE_PATH_SENTINEL.txt","content":"PRIVATE_CONTENT_SENTINEL"}));
    f.denied(json!({"op":"shell","program":"PRIVATE_PROGRAM_SENTINEL","args":["PRIVATE_ARGUMENT_SENTINEL"]}));
    let audit = f.audit();
    assert!(audit.lines().count() >= 2);
    for line in audit.lines() {
        assert!(serde_json::from_str::<Value>(line).unwrap().is_object());
    }
    for secret in [
        "PRIVATE_PATH_SENTINEL",
        "PRIVATE_CONTENT_SENTINEL",
        "PRIVATE_PROGRAM_SENTINEL",
        "PRIVATE_ARGUMENT_SENTINEL",
        "PRIVATE_OWNER_SENTINEL",
    ] {
        assert!(!audit.contains(secret), "audit exposed {secret}");
    }
}

#[test]
fn managed_only_gate_sandbox_does_not_fall_back_to_command_execution() {
    let f = Fixture::new(&["read", "write", "git", "shell"]);
    f.flow(&["init", ID, "HEAD"]);
    f.flow(&["submit", ID, "0", "plan", "report.md"]);
    f.write("app/value.txt", "candidate value\n");
    let candidate = f.commit();
    f.flow(&["submit", ID, "1", "candidate", "report.md", &candidate]);
    let output = Fixture::parse(
        f.flow_raw(&["gate", ID, "2", "--sandbox", "managed-only"]),
        1,
    );
    assert_eq!(output["checks"][0]["id"], "scope");
    assert_eq!(output["checks"][0]["outcome"], "pass");
    assert!(
        output["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["outcome"] == "error")
    );
    assert_ne!(output["status"]["stage"], "review");
    assert!(!f.root.join("gate-ran").exists());
    assert!(f.flow(&["audit", ID])["events"].is_array());
}

#[test]
fn pinned_regular_files_cannot_be_reinterpreted_as_directory_scopes() {
    let mut f = Fixture::new(&["read", "write", "git"]);
    f.start();
    fs::remove_file(f.repo.join("app/value.txt")).unwrap();
    f.write("app/value.txt/descendant.txt", "shadowed leaf sentinel\n");
    for operation in [
        json!({"op":"read","path":"app/value.txt/descendant.txt"}),
        json!({"op":"write","path":"app/value.txt/descendant.txt","content":"must not write"}),
        json!({"op":"git_read","path":"app/value.txt/descendant.txt"}),
    ] {
        assert_eq!(f.denied(operation)["error"]["code"], "path_denied");
    }
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt/descendant.txt")).unwrap(),
        "shadowed leaf sentinel\n"
    );
}

#[test]
fn overlarge_and_unknown_field_writes_are_rejected_without_target_changes() {
    let mut f = Fixture::new(&["read", "write"]);
    f.start();
    let too_large = "x".repeat(1_048_577);
    assert_eq!(
        f.denied(json!({"op":"write","path":"app/value.txt","content":too_large}))["error"]["code"],
        "request_too_large"
    );
    let operation =
        json!({"op":"write","path":"app/value.txt","content":"must not write","grant":"write"});
    assert_eq!(f.denied(operation)["error"]["code"], "invalid_request");
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
}

#[test]
fn admission_denial_is_audited_without_untrusted_token_fields() {
    let mut f = Fixture::new(&["read", "write"]);
    f.start();
    let mut request = f.request(json!({"op":"write","path":"app/value.txt","content":"changed"}));
    request["lease"]["owner"] = json!("PRIVATE_FORGED_OWNER_SENTINEL");
    assert_eq!(
        assert_error(f.runtime_raw(&request))["error"]["code"],
        "lease_denied"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
    let audit = f.audit();
    assert!(audit.contains("request_rejected"));
    assert!(audit.contains("lease_denied"));
    assert!(!audit.contains("PRIVATE_FORGED_OWNER_SENTINEL"));
    assert!(!audit.contains("app/value.txt"));
}

#[test]
fn exhausted_audit_capacity_refuses_an_effect_before_writing() {
    let mut f = Fixture::new(&["write"]);
    f.start();
    let audit = f
        .repo
        .join(format!(".storyflow/{ID}/security-events.jsonl"));
    let file = fs::File::create(&audit).unwrap();
    file.set_len(4_194_304).unwrap();
    drop(file);
    assert_eq!(
        f.denied(json!({"op":"write","path":"app/value.txt","content":"changed"}))["error"]["code"],
        "security_audit_full"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
    assert_eq!(fs::metadata(&audit).unwrap().len(), 4_194_304);
}

#[test]
fn candidate_scope_is_checked_before_any_trusted_gate_command() {
    let f = Fixture::new(&["read", "write", "git", "shell"]);
    f.flow(&["init", ID]);
    f.flow(&["submit", ID, "0", "plan", "report.md"]);
    f.write("outside.txt", "outside scope\n");
    let candidate = f.commit();
    f.flow(&["submit", ID, "1", "candidate", "report.md", &candidate]);
    let gate = Fixture::parse(f.flow_raw(&["gate", ID, "2"]), 1);
    assert_eq!(gate["checks"][0]["id"], "scope");
    assert_eq!(gate["checks"][0]["outcome"], "fail");
    assert_eq!(gate["checks"].as_array().unwrap().len(), 1);
    assert_eq!(gate["status"]["candidate"], candidate);
    assert!(!f.root.join("gate-ran").exists());
}

#[test]
fn trusted_gate_command_receives_pinned_arguments_and_keeps_lifecycle_identity() {
    let f = Fixture::new(&["read", "write", "git", "shell"]);
    f.flow(&["init", ID]);
    f.flow(&["submit", ID, "0", "plan", "report.md"]);
    f.write("app/value.txt", "candidate\n");
    let candidate = f.commit();
    f.flow(&["submit", ID, "1", "candidate", "report.md", &candidate]);
    let gate = f.flow(&["gate", ID, "2"]);
    assert_eq!(gate["status"]["stage"], "review");
    assert_eq!(gate["status"]["candidate"], candidate);
    assert_eq!(
        fs::read_to_string(f.root.join("gate-ran")).unwrap(),
        "gate executed"
    );
    let done = f.flow(&["submit", ID, "3", "approve", "report.md", &candidate]);
    assert_eq!(done["stage"], "done");
    assert_eq!(done["revision"], 4);
}

#[test]
fn absent_tool_permissions_default_to_no_managed_grants() {
    let mut f = Fixture::new(&["read", "write", "git", "shell"]);
    let mut config: Value =
        serde_json::from_slice(&fs::read(f.repo.join("storyflow.json")).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("tools");
    f.document("storyflow.json", config);
    f.base = f.commit();
    f.start();
    let snapshot: Value = serde_json::from_slice(&fs::read(f.snapshot()).unwrap()).unwrap();
    assert_eq!(snapshot["effective"]["capabilities"]["granted"], json!([]));
    for operation in [
        json!({"op":"read","path":"app/value.txt"}),
        json!({"op":"write","path":"app/value.txt","content":"changed"}),
        json!({"op":"git","args":["reset","--hard"]}),
        shell_probe(&f),
    ] {
        assert_eq!(f.denied(operation)["error"]["code"], "capability_denied");
    }
    assert_eq!(
        fs::read_to_string(f.repo.join("app/value.txt")).unwrap(),
        "pinned value\n"
    );
    assert!(!f.root.join("gate-ran").exists());
}
