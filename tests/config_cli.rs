use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    context: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        #[cfg(windows)]
        let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("storyflow-test-tmp");
        #[cfg(not(windows))]
        let parent = std::env::temp_dir();
        let root = parent.join(format!(
            "sf6-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join("team/project");
        fs::create_dir_all(&repo).unwrap();
        let fixture = Self {
            context: root.join("host.json"),
            root,
            repo,
        };
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Config CLI Tests"],
            vec!["config", "user.email", "cli@example.invalid"],
            vec!["config", "core.autocrlf", "false"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", ".git/no-hooks"],
        ] {
            fixture.git(&args);
        }
        for role in ["orchestrator", "planner", "implementer", "reviewer"] {
            fixture.write(
                &format!("agents/{role}.md"),
                "# PRIVATE_CONTRACT_SENTINEL\n",
            );
        }
        fixture.write("storyflow.json", &json!({"version":1,"protected":[],"checks":[{"id":"project","program":"PRIVATE_PROGRAM_SENTINEL","args":["PRIVATE_ARG_SENTINEL"],"timeout_seconds":5}]}).to_string());
        fixture.write(
            "stories/slug-001/story.md",
            "# CLI compatibility\nAC1: initialize a run.\n",
        );
        fixture.write("stories/slug-001/story.json", "{\"scope\":[\"app\"]}");
        fixture.git(&["add", "--all"]);
        fixture.git(&["commit", "-qm", "baseline"]);
        fixture
    }
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn write(&self, name: &str, bytes: &str) {
        let path = self.repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_storyflow"));
        command
            .current_dir(&self.repo)
            .env_remove("STORYFLOW_HOST_CONTEXT")
            .env_remove("STORYFLOW_HOST_CONTEXT_ROOT");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn json(&self, args: &[&str], exit: i32) -> Value {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn scoped(&self) {
        let user = self.root.join("user");
        fs::create_dir_all(user.join("Storyflow")).unwrap();
        fs::write(user.join("Storyflow/config.json"), json!({"version":2,"scope":"user","workspaces":[self.root.join("team")],"models":{"high":"PRIVATE_MODEL_SENTINEL"},"policy":{"protected":["user-policy"]}}).to_string()).unwrap();
        fs::write(self.root.join("team/storyflow.workspace.json"), json!({"version":2,"scope":"workspace","members":["project"],"policy":{"protected":["workspace-policy"]}}).to_string()).unwrap();
        fs::write(&self.context, json!({"version":1,"user_base":user,"trusted_user_base":fs::canonicalize(&user).unwrap(),"user_directory":"Storyflow","trusted_workspace_roots":[fs::canonicalize(self.root.join("team")).unwrap()]}).to_string()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn scope_bootstrap_preserves_hand_edits_and_creates_no_run() {
    let f = Fixture::new();
    for (scope, name) in [
        ("user", "config.json"),
        ("workspace", "storyflow.workspace.json"),
        ("repo", "storyflow.json"),
    ] {
        let directory = f.root.join(format!("bootstrap-{scope}"));
        let path = directory.to_str().unwrap();
        let first = f.json(&["init", "--scope", scope, "--path", path], 0);
        assert_eq!(first["created"], true);
        assert_eq!(first["trust_granted"], false);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(directory.join(name)).unwrap()).unwrap()["scope"],
            scope
        );
        fs::write(
            directory.join(name),
            "hand edited; even invalid JSON remains intact\n",
        )
        .unwrap();
        let next = f.json(&["init", "--path", path, "--scope", scope], 0);
        assert_eq!(next["status"], "unchanged");
        assert_eq!(
            fs::read_to_string(directory.join(name)).unwrap(),
            "hand edited; even invalid JSON remains intact\n"
        );
        assert!(!directory.join(".storyflow").exists());
    }
    assert!(!f.repo.join(".storyflow").exists());
}
#[test]
fn default_repo_bootstrap_uses_git_root_and_preserves_legacy_file() {
    let f = Fixture::new();
    fs::create_dir_all(f.repo.join("nested")).unwrap();
    let before = fs::read(f.repo.join("storyflow.json")).unwrap();
    let output = f
        .command()
        .current_dir(f.repo.join("nested"))
        .args(["init", "--scope", "repo"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read(f.repo.join("storyflow.json")).unwrap(), before);
    assert!(!f.repo.join("nested/storyflow.json").exists());
}
#[test]
fn default_user_bootstrap_uses_host_environment_without_authorizing_trust() {
    let f = Fixture::new();
    let base = f.root.join("user-default");
    fs::create_dir_all(&base).unwrap();
    let output = f
        .command()
        .env("APPDATA", &base)
        .env("XDG_CONFIG_HOME", &base)
        .args(["init", "--scope", "user"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let directory = if cfg!(windows) {
        "Storyflow"
    } else {
        "storyflow"
    };
    assert!(base.join(directory).join("config.json").is_file());
    assert!(!f.context.exists());
}
#[test]
fn bootstrap_rejects_malformed_grammar_before_any_mutation() {
    let f = Fixture::new();
    for args in [
        vec!["init", "--scope"],
        vec!["init", "--scope", "bogus"],
        vec!["init", "--scope", "repo", "--scope", "repo"],
        vec!["init", "--scope", "repo", "--path"],
        vec!["init", "--scope", "repo", "--path", "a", "--path", "b"],
        vec!["init", "slug-001", "--scope", "repo"],
        vec!["init", "--scope", "repo", "HEAD"],
        vec!["init", "--path", "x"],
        vec!["init", "--unknown"],
        vec!["config"],
        vec!["config", "inspect"],
        vec!["config", "inspect", "--effective", "--effective"],
        vec!["config", "validate", "--effective"],
    ] {
        assert_eq!(
            f.json(&args, 2)["error"]["code"],
            "invalid_arguments",
            "{args:?}"
        );
    }
    assert!(!f.repo.join(".storyflow").exists());
    assert!(!f.repo.join("a").exists());
    assert!(!f.repo.join("x").exists());
}
#[test]
fn all_new_commands_have_help_without_git_or_mutation() {
    let f = Fixture::new();
    for args in [
        vec!["init", "--help"],
        vec!["init", "--scope", "user", "--help"],
        vec!["config", "--help"],
        vec!["config", "inspect", "--help"],
        vec!["config", "validate", "--help"],
    ] {
        let output = f
            .command()
            .current_dir(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("--effective"));
    }
    assert!(!f.repo.join(".storyflow").exists());
}
#[test]
fn effective_inspection_is_deterministic_redacted_and_read_only() {
    let f = Fixture::new();
    let first = f.run(&["config", "inspect", "--effective"]);
    let second = f.run(&["config", "inspect", "--effective"]);
    assert!(first.status.success());
    assert_eq!(first.stdout, second.stdout);
    let text = String::from_utf8(first.stdout).unwrap();
    for secret in [
        "PRIVATE_PROGRAM_SENTINEL",
        "PRIVATE_ARG_SENTINEL",
        "PRIVATE_CONTRACT_SENTINEL",
    ] {
        assert!(!text.contains(secret));
    }
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["configuration"]["policy"]["checks"][0]["timeout_seconds"],
        5
    );
    assert_eq!(
        value["configuration"]["policy"]["checks"][0]["origin"]["scope"],
        "repo"
    );
    assert_eq!(f.json(&["config", "validate"], 0)["valid"], true);
    assert!(!f.repo.join(".storyflow").exists());
}
#[test]
fn inspect_and_validate_use_committed_head_not_dirty_configuration() {
    let f = Fixture::new();
    f.write("storyflow.json", "not JSON");
    assert_eq!(f.json(&["config", "validate"], 0)["valid"], true);
    f.git(&["add", "storyflow.json"]);
    f.git(&["commit", "-qm", "invalid config"]);
    let error = f.json(&["config", "validate"], 2);
    assert_eq!(error["error"]["code"], "invalid_json");
    assert!(error["error"]["origin"].is_object());
}
#[test]
fn explicit_three_scope_inspection_respects_host_trust_and_redaction() {
    let f = Fixture::new();
    f.scoped();
    for args in [
        ["config", "inspect", "--effective"].as_slice(),
        ["config", "validate"].as_slice(),
    ] {
        let output = f
            .command()
            .env("STORYFLOW_HOST_CONTEXT", &f.context)
            .env("STORYFLOW_HOST_CONTEXT_ROOT", &f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("PRIVATE_MODEL_SENTINEL"));
        assert!(!text.contains("PRIVATE_PROGRAM_SENTINEL"));
        assert!(!text.contains("PRIVATE_CONTRACT_SENTINEL"));
        let value: Value = serde_json::from_str(&text).unwrap();
        let scopes = if args.len() == 3 {
            &value["configuration"]["scopes"]
        } else {
            &value["scopes"]
        };
        assert_eq!(scopes.as_array().unwrap().len(), 3);
    }
    let missing = f
        .command()
        .env("STORYFLOW_HOST_CONTEXT", &f.context)
        .args(["config", "validate"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&missing.stdout).unwrap()["error"]["code"],
        "missing_host_context_root"
    );
    let local = f.repo.join("host.json");
    fs::copy(&f.context, &local).unwrap();
    let refused = f
        .command()
        .env("STORYFLOW_HOST_CONTEXT", &local)
        .env("STORYFLOW_HOST_CONTEXT_ROOT", &f.repo)
        .args(["config", "validate"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.stdout).unwrap()["error"]["code"],
        "untrusted_host_context"
    );
}
#[test]
fn reduced_capabilities_can_be_validated_but_do_not_admit_execution() {
    let f = Fixture::new();
    f.scoped();
    let user = f.root.join("user/Storyflow/config.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&user).unwrap()).unwrap();
    document["tools"] = json!({"deny":["shell"]});
    fs::write(user, document.to_string()).unwrap();
    let output = f
        .command()
        .env("STORYFLOW_HOST_CONTEXT", &f.context)
        .env("STORYFLOW_HOST_CONTEXT_ROOT", &f.root)
        .args(["config", "inspect", "--effective"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["configuration"]["execution_admitted"], false);
    assert_eq!(
        value["configuration"]["capabilities"]["granted"],
        json!(["git", "read", "write"])
    );
    let init = f
        .command()
        .env("STORYFLOW_HOST_CONTEXT", &f.context)
        .env("STORYFLOW_HOST_CONTEXT_ROOT", &f.root)
        .args(["init", "slug-001", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(init.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&init.stderr).contains("unsupported_capability_enforcement"));
}
#[test]
fn legacy_positional_init_keeps_existing_revision_and_snapshot_semantics() {
    let f = Fixture::new();
    let result = f.json(&["init", "slug-001", "HEAD"], 0);
    assert_eq!(result["revision"], 0);
    assert_eq!(result["stage"], "plan");
    assert_eq!(result["config_mode"], "snapshot-v1");
    assert!(
        f.repo
            .join(".storyflow/slug-001/config-snapshot.json")
            .is_file()
    );
    assert_eq!(f.json(&["next", "slug-001"], 0)["action"], "delegate");
}
#[test]
fn bootstrap_refuses_traversal_existing_directories_and_user_checkout() {
    let f = Fixture::new();
    assert_eq!(
        f.json(&["init", "--scope", "repo", "--path", "../escaped"], 2)["error"]["code"],
        "invalid_path"
    );
    let directory = f.root.join("target-directory");
    fs::create_dir_all(directory.join("storyflow.json")).unwrap();
    assert_eq!(
        f.json(
            &[
                "init",
                "--scope",
                "repo",
                "--path",
                directory.to_str().unwrap()
            ],
            2
        )["error"]["code"],
        "unsafe_bootstrap_path"
    );
    assert_eq!(
        f.json(
            &[
                "init",
                "--scope",
                "user",
                "--path",
                f.repo.to_str().unwrap()
            ],
            2
        )["error"]["code"],
        "untrusted_user_location"
    );
}
#[test]
fn bootstrap_refuses_redirected_parent_and_keeps_outside_untouched() {
    let f = Fixture::new();
    let outside = f.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let link = f.root.join("redirect");
    #[cfg(windows)]
    {
        let command = format!(
            "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
            link.display(),
            outside.display()
        );
        assert!(
            Command::new("pwsh")
                .args(["-NoProfile", "-Command", &command])
                .status()
                .unwrap()
                .success()
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let target = link.join("child");
    assert_eq!(
        f.json(
            &[
                "init",
                "--scope",
                "repo",
                "--path",
                target.to_str().unwrap()
            ],
            2
        )["error"]["code"],
        "unsafe_bootstrap_path"
    );
    assert!(!outside.join("child").exists());
}
