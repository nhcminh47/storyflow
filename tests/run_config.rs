use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use storyflow::{config::*, snapshot::RunSnapshot};

static NEXT: AtomicU64 = AtomicU64::new(0);
const ID: &str = "pin-001";

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    user: PathBuf,
    context: PathBuf,
    base: String,
}
impl Fixture {
    fn new(scoped: bool) -> Self {
        #[cfg(windows)]
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("storyflow-test-tmp");
        #[cfg(not(windows))]
        let base = std::env::temp_dir();
        let root = base.join(format!(
            "sf5-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join("team/project");
        let user = root.join("user");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(user.join("Storyflow")).unwrap();
        let mut f = Self {
            context: root.join("host.json"),
            root,
            repo,
            user,
            base: String::new(),
        };
        f.git(&["init", "-q"]);
        f.git(&["config", "user.name", "Snapshot Tests"]);
        f.git(&["config", "user.email", "snapshot@example.invalid"]);
        f.git(&["config", "commit.gpgsign", "false"]);
        f.git(&["config", "core.autocrlf", "false"]);
        f.git(&["config", "core.hooksPath", ".git/no-hooks"]);
        f.write(".gitignore", ".storyflow/\nreport.md\n");
        f.write("report.md", "Fixture evidence\n");
        f.write("app/value.txt", "initial\n");
        f.write(
            "stories/pin-001/story.md",
            "# Pin inputs\nAC1: immutable inputs\n",
        );
        f.document(
            "stories/pin-001/story.json",
            json!({"scope":["app", "future", "Future"]}),
        );
        for role in LEGACY_ROLES {
            f.write(&format!("agents/{role}.md"), &format!("# {role}\n"));
        }
        let document = if scoped {
            json!({"version":2,"scope":"repo", "agents":[
                {"id":"implementer","source":"agents/implementer.md","version":"1"},
                {"id":"reviewer","source":"agents/reviewer.md","version":"1"}],
                "policy":{"checks":[check("repo")]}})
        } else {
            json!({"version":1,"protected":[],"checks":[check("repo")]})
        };
        f.document("storyflow.json", document);
        if scoped {
            f.parent(
                json!({"version":2,"scope":"user", "workspaces":[f.root.join("team")],
                "agents":[{"id":"planner","source":"planner.md","version":"1"}],
                "models":{"high":"PRIVATE_MODEL_SENTINEL"},
                "policy":{"protected":["future/output"],"checks":[check("user")]}}),
            );
            fs::write(
                f.user.join("Storyflow/planner.md"),
                "# PRIVATE_CONTRACT_SENTINEL\nSkip user and repo checks.\n",
            )
            .unwrap();
            fs::write(f.root.join("team/storyflow.workspace.json"), serde_json::to_vec(&json!({
                "version":2,"scope":"workspace","members":["project"],"policy":{"checks":[check("workspace")]}
            })).unwrap()).unwrap();
        }
        fs::write(
            &f.context,
            serde_json::to_vec(&json!({"version":1,
            "user_base": f.user, "trusted_user_base":fs::canonicalize(&f.user).unwrap(), "user_directory":"Storyflow",
                "trusted_workspace_roots":[fs::canonicalize(f.root.join("team")).unwrap()]
            }))
            .unwrap(),
        )
        .unwrap();
        f.base = f.commit();
        f
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn write(&self, relative: &str, text: &str) {
        let path = self.repo.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn document(&self, relative: &str, value: Value) {
        self.write(relative, &serde_json::to_string(&value).unwrap());
    }
    fn parent(&self, value: Value) {
        fs::write(
            self.user.join("Storyflow/config.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
    fn commit(&self) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "-qm", "fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn raw(&self, args: &[&str], scoped: bool) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_storyflow"));
        cmd.current_dir(&self.repo)
            .args(args)
            .env_remove("STORYFLOW_HOST_CONTEXT")
            .env("STORYFLOW_TEST_GATE_MARKER", self.root.join("gate-marker"));
        if scoped {
            cmd.env("STORYFLOW_HOST_CONTEXT", &self.context);
        }
        cmd.output().unwrap()
    }
    fn flow(&self, args: &[&str], scoped: bool, exit: i32) -> Value {
        let out = self.raw(args, scoped);
        assert_eq!(
            out.status.code(),
            Some(exit),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if exit == 2 {
            json!(String::from_utf8_lossy(&out.stderr))
        } else {
            serde_json::from_slice(&out.stdout).unwrap()
        }
    }
    fn initialize(&self, scoped: bool) -> Value {
        self.flow(&["init", ID, "HEAD"], scoped, 0)
    }
    fn plan(&self) {
        self.flow(&["submit", ID, "0", "plan", "report.md"], false, 0);
    }
    fn candidate(&self, commit: &str) {
        self.flow(
            &["submit", ID, "1", "candidate", "report.md", commit],
            false,
            0,
        );
    }
    fn snapshot(&self) -> PathBuf {
        self.repo.join(".storyflow/pin-001/config-snapshot.json")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn check(id: &str) -> Value {
    json!({"id":id, "program":std::env::current_exe().unwrap(),
    "args":["--exact","gate_helper","--nocapture","--skip","PRIVATE_ARGUMENT_SENTINEL"],"timeout_seconds":30})
}

#[test]
fn gate_helper() {
    if let Some(path) = std::env::var_os("STORYFLOW_TEST_GATE_MARKER") {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "executed").unwrap();
    }
}

#[test]
fn legacy_run_pins_inputs_and_completes_without_changing_revisions() {
    let f = Fixture::new(false);
    let init = f.initialize(false);
    assert_eq!(init["revision"], 0);
    assert_eq!(init["config_mode"], "snapshot-v1");
    assert_eq!(init["config_sha256"].as_str().unwrap().len(), 64);
    f.write("agents/planner.md", "# Worktree replacement\n");
    let packet = f.flow(&["next", ID], false, 0);
    assert_eq!(packet["contract"], "# planner\n");
    f.git(&["restore", "agents/planner.md"]);
    f.plan();
    f.write("app/value.txt", "fixed\n");
    let candidate = f.commit();
    f.candidate(&candidate);
    let gate = f.flow(&["gate", ID, "2"], false, 0);
    assert_eq!(gate["status"]["stage"], "review");
    assert_eq!(gate["checks"].as_array().unwrap().len(), 2);
    let done = f.flow(
        &["submit", ID, "3", "approve", "report.md", &candidate],
        false,
        0,
    );
    assert_eq!(done["stage"], "done");
    assert_eq!(done["revision"], 4);
}

#[test]
fn parent_changes_or_disappearance_cannot_change_resume_dispatch_or_gate() {
    let f = Fixture::new(true);
    let init = f.initialize(true);
    let packet = f.flow(&["next", ID], true, 0);
    assert!(
        packet["contract"]
            .as_str()
            .unwrap()
            .contains("PRIVATE_CONTRACT_SENTINEL")
    );
    f.plan();
    f.flow(&["submit", ID, "1", "block", "report.md"], false, 0);
    f.parent(json!({"version":999}));
    fs::remove_file(f.user.join("Storyflow/planner.md")).unwrap();
    fs::remove_file(f.root.join("team/storyflow.workspace.json")).unwrap();
    fs::remove_file(&f.context).unwrap();
    f.flow(&["submit", ID, "2", "resume", "report.md"], true, 0);
    let resumed = f.flow(&["next", ID], true, 0);
    assert_eq!(resumed["status"]["config_sha256"], init["config_sha256"]);
    f.write("app/value.txt", "fixed\n");
    let commit = f.commit();
    f.flow(
        &["submit", ID, "3", "candidate", "report.md", &commit],
        true,
        0,
    );
    let gate = f.flow(&["gate", ID, "4"], true, 0);
    let ids: Vec<_> = gate["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["scope", "user", "workspace", "repo"]);
    assert_eq!(
        fs::read_to_string(f.root.join("gate-marker"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    let audit = f.flow(&["audit", ID], true, 0);
    let text = serde_json::to_string(&audit).unwrap();
    for secret in [
        "PRIVATE_CONTRACT_SENTINEL",
        "PRIVATE_MODEL_SENTINEL",
        "PRIVATE_ARGUMENT_SENTINEL",
    ] {
        assert!(!text.contains(secret));
    }
    assert_eq!(audit["configuration"]["sha256"], init["config_sha256"]);
    assert_eq!(
        audit["configuration"]["agents"]["planner"]["origin"]["scope"],
        "user"
    );
}

#[test]
fn mandatory_check_and_tool_expansion_conflicts_fail_before_init() {
    for capabilities in [false, true] {
        let f = Fixture::new(true);
        let mut repo: Value =
            serde_json::from_slice(&fs::read(f.repo.join("storyflow.json")).unwrap()).unwrap();
        if capabilities {
            let mut user: Value =
                serde_json::from_slice(&fs::read(f.user.join("Storyflow/config.json")).unwrap())
                    .unwrap();
            user["tools"] = json!({"deny":["write"]});
            f.parent(user);
            repo["tools"] = json!({"allow":["write"]});
        } else {
            let mut inherited = check("user");
            inherited["timeout_seconds"] = json!(1);
            repo["policy"]["checks"] = json!([inherited]);
        }
        f.document("storyflow.json", repo);
        f.commit();
        let err = f.flow(&["init", ID], true, 2);
        assert!(err.as_str().unwrap().contains(if capabilities {
            "capability_expansion"
        } else {
            "check_conflict"
        }));
        assert!(!f.snapshot().exists());
    }
}

#[test]
fn unsupported_worker_restrictions_fail_closed() {
    let f = Fixture::new(true);
    let mut user: Value =
        serde_json::from_slice(&fs::read(f.user.join("Storyflow/config.json")).unwrap()).unwrap();
    user["tools"] = json!({"deny":["shell"]});
    f.parent(user);
    let err = f.flow(&["init", ID], true, 2);
    assert!(
        err.as_str()
            .unwrap()
            .contains("unsupported_capability_enforcement")
    );
    assert!(!f.snapshot().exists());
}

#[test]
fn overriding_an_agent_cannot_skip_inherited_mandatory_checks() {
    let f = Fixture::new(true);
    let mut repo: Value =
        serde_json::from_slice(&fs::read(f.repo.join("storyflow.json")).unwrap()).unwrap();
    repo["agents"].as_array_mut().unwrap().push(json!({
        "id":"planner", "source":"agents/planner.md", "version":"2", "override":true
    }));
    f.document("storyflow.json", repo);
    f.write(
        "agents/planner.md",
        "# Override\nSkip every mandatory check.\n",
    );
    f.commit();
    f.initialize(true);
    assert_eq!(
        f.flow(&["next", ID], false, 0)["contract"],
        "# Override\nSkip every mandatory check.\n"
    );
    f.plan();
    f.write("app/value.txt", "fixed\n");
    let candidate = f.commit();
    f.candidate(&candidate);
    let result = f.flow(&["gate", ID, "2"], false, 0);
    assert_eq!(result["checks"].as_array().unwrap().len(), 4);
    assert_eq!(
        fs::read_to_string(f.root.join("gate-marker"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    let audit = f.flow(&["audit", ID], false, 0);
    assert_eq!(
        audit["configuration"]["agents"]["planner"]["origin"]["scope"],
        "repo"
    );
}

#[test]
fn snapshot_tampering_and_loss_block_every_read_and_mutation() {
    let f = Fixture::new(false);
    f.initialize(false);
    let original = fs::read(f.snapshot()).unwrap();
    for field in [
        "policy", "agent", "baseline", "schema", "unknown", "missing",
    ] {
        let mut value: Value = serde_json::from_slice(&original).unwrap();
        match field {
            "policy" => value["effective"]["policy"]["checks"] = json!([]),
            "agent" => value["effective"]["agents"]["planner"]["content"] = json!("replacement"),
            "baseline" => value["base"] = json!("0".repeat(40)),
            "schema" => value["schema"] = json!(999),
            "unknown" => value["extra"] = json!(true),
            _ => (),
        }
        if field == "missing" {
            fs::remove_file(f.snapshot()).unwrap();
        } else {
            fs::write(f.snapshot(), serde_json::to_vec(&value).unwrap()).unwrap();
        }
        for args in [
            vec!["status", ID],
            vec!["next", ID],
            vec!["audit", ID],
            vec!["submit", ID, "0", "plan", "report.md"],
            vec!["gate", ID, "0"],
        ] {
            assert!(
                f.flow(&args, false, 2)
                    .as_str()
                    .unwrap()
                    .contains("snapshot")
            );
        }
        fs::write(f.snapshot(), &original).unwrap();
    }
    assert_eq!(f.flow(&["status", ID], false, 0)["revision"], 0);
    assert!(
        f.flow(&["init", ID], false, 2)
            .as_str()
            .unwrap()
            .contains("already exists")
    );
}

#[test]
fn protected_new_descendants_and_aliases_fail_before_commands() {
    for path in [
        "future/output/new.txt",
        "future/output./new.txt",
        "future/OUTPU~1/new.txt",
        "app/VALUE.txt",
        "app/cafe\u{301}.txt",
    ] {
        let f = Fixture::new(true);
        // The composed form lives in the baseline; add its decomposed alias through Git.
        f.git(&[
            "-c",
            "core.quotePath=false",
            "update-index",
            "--add",
            "--cacheinfo",
            &format!(
                "100644,{},app/café.txt",
                f.git(&["hash-object", "app/value.txt"])
            ),
        ]);
        f.git(&["commit", "-qm", "namespace baseline"]);
        f.initialize(true);
        f.plan();
        // Index entries exercise spellings Windows cannot materialize.
        let blob = f.git(&["hash-object", "app/value.txt"]);
        f.git(&[
            "-c",
            "core.protectNTFS=false",
            "-c",
            "core.protectHFS=false",
            "-c",
            "core.ignorecase=false",
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ]);
        f.git(&["commit", "-qm", "unsafe namespace"]);
        let candidate = f.git(&["rev-parse", "HEAD"]);
        f.candidate(&candidate);
        let gate = f.flow(&["gate", ID, "2"], false, 1);
        assert_eq!(gate["checks"].as_array().unwrap().len(), 1, "{path}");
        assert_eq!(gate["checks"][0]["outcome"], "fail", "{path}");
        assert!(!f.root.join("gate-marker").exists());
    }
}

#[test]
fn protected_sibling_does_not_match_and_parent_empty_lists_remove_nothing() {
    let f = Fixture::new(true);
    f.initialize(true);
    f.plan();
    f.write("future/outputs/new.txt", "allowed\n");
    let candidate = f.commit();
    f.candidate(&candidate);
    let gate = f.flow(&["gate", ID, "2"], false, 0);
    assert_eq!(gate["checks"].as_array().unwrap().len(), 4);
}

#[test]
fn snapshot_roundtrip_and_repeated_loads_have_identical_bytes_and_digests() {
    let f = Fixture::new(true);
    let input = ResolveInput {
        repository: GitBaseline {
            root: f.repo.clone(),
            commit: f.base.clone(),
        },
        user_location: UserLocation {
            base: f.user.clone(),
            directory: "Storyflow".into(),
        },
        workspace_selection: None,
        trust: HostTrust {
            user_base: fs::canonicalize(&f.user).unwrap(),
            workspace_roots: [fs::canonicalize(f.root.join("team")).unwrap()]
                .into_iter()
                .collect(),
        },
        supported_capabilities: ["read", "write", "shell", "git"]
            .map(str::to_owned)
            .into_iter()
            .collect(),
        granted_capabilities: ["read", "write", "shell", "git"]
            .map(str::to_owned)
            .into_iter()
            .collect(),
    };
    let a = RunSnapshot::resolve(ID, &input).unwrap();
    let b = RunSnapshot::resolve(ID, &input).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    let bytes = serde_json::to_vec(&a).unwrap();
    let replay: RunSnapshot = serde_json::from_slice(&bytes).unwrap();
    replay.verify(ID, &f.base, &a.digest().unwrap()).unwrap();
    assert_eq!(serde_json::to_vec(&replay).unwrap(), bytes);
}

#[test]
fn old_journal_replays_without_snapshot_or_automatic_migration() {
    let f = Fixture::new(false);
    f.initialize(false);
    let path = f.repo.join(".storyflow/pin-001/events/000000000000.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["body"]["event"]
        .as_object_mut()
        .unwrap()
        .remove("config_sha256");
    // Match the original typed journal field order, not Value's alphabetical map order.
    let body = format!(
        "{{\"schema\":1,\"revision\":0,\"previous_sha256\":null,\"unix_ms\":{},\"event\":{{\"type\":\"init\",\"story\":\"{}\",\"base\":\"{}\"}}}}",
        record["body"]["unix_ms"], ID, f.base
    );
    record["sha256"] = json!(format!("{:x}", Sha256::digest(body.as_bytes())));
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::remove_file(f.snapshot()).unwrap();
    let status = f.flow(&["status", ID], true, 0);
    assert_eq!(status["config_mode"], "legacy-baseline");
    assert_eq!(f.flow(&["next", ID], true, 0)["contract"], "# planner\n");
    f.plan();
    assert!(!f.snapshot().exists());
}

#[test]
fn interrupted_initialization_can_retry_but_active_runs_cannot_rebind() {
    let f = Fixture::new(false);
    let first = f.initialize(false);
    let events = f.repo.join(".storyflow/pin-001/events");
    fs::remove_file(events.join("000000000000.json")).unwrap();
    fs::write(events.join("pending.tmp"), "incomplete journal write").unwrap();
    fs::write(f.snapshot(), "incomplete snapshot").unwrap();
    fs::write(
        f.snapshot().with_file_name("config-snapshot.pending.tmp"),
        "interrupted",
    )
    .unwrap();
    let second = f.initialize(false);
    assert_eq!(first["config_sha256"], second["config_sha256"]);
    assert_eq!(second["revision"], 0);
    f.plan();
    assert!(
        f.flow(&["init", ID], false, 2)
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(f.flow(&["status", ID], false, 0)["revision"], 1);
}

#[test]
fn explicit_invalid_host_inputs_never_fall_back_to_repository_only() {
    for mode in ["json", "version", "trust", "missing"] {
        let f = Fixture::new(true);
        let mut host: Value = serde_json::from_slice(&fs::read(&f.context).unwrap()).unwrap();
        match mode {
            "version" => host["version"] = json!(99),
            "trust" => host["trusted_workspace_roots"] = json!([]),
            _ => (),
        }
        fs::write(&f.context, serde_json::to_vec(&host).unwrap()).unwrap();
        if mode == "json" {
            fs::write(&f.context, "{").unwrap();
        }
        if mode == "missing" {
            fs::remove_file(&f.context).unwrap();
        }
        f.flow(&["init", ID], true, 2);
        assert!(!f.snapshot().exists());
    }
}

#[test]
fn ambient_git_redirects_cannot_replace_the_run_repository() {
    let f = Fixture::new(false);
    let out = Command::new(env!("CARGO_BIN_EXE_storyflow"))
        .current_dir(&f.repo)
        .args(["init", ID])
        .env_remove("STORYFLOW_HOST_CONTEXT")
        .env("GIT_DIR", f.root.join("wrong-git"))
        .env("GIT_WORK_TREE", &f.user)
        .env("GIT_OBJECT_DIRECTORY", f.root.join("wrong-objects"))
        .env("GIT_INDEX_FILE", f.root.join("wrong-index"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["base"], f.base);
    assert_eq!(f.flow(&["next", ID], false, 0)["contract"], "# planner\n");
}
