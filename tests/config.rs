use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};
use storyflow::config::*;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    user: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::at("team/project")
    }
    fn at(relative: &str) -> Self {
        // Windows TEMP can use 8.3 aliases, and User config must be outside
        // any Git checkout. Place fixtures beside the checkout, not in target/.
        #[cfg(windows)]
        let base = std::env::current_dir()
            .unwrap()
            .parent()
            .expect("checkout has a parent")
            .join("storyflow-test-tmp");
        #[cfg(not(windows))]
        let base = std::env::temp_dir();
        let root = base.join(format!(
            "sf4-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join(relative);
        let user = root.join("user");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(user.join("Storyflow")).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.name", "Config Tests"]);
        git(&repo, &["config", "user.email", "config@example.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["config", "core.autocrlf", "false"]);
        git(&repo, &["config", "core.hooksPath", ".git/no-hooks"]);
        Self { root, repo, user }
    }
    fn write(&self, path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn document(&self, path: impl AsRef<Path>, document: &Value) {
        self.write(path, serde_json::to_vec(document).unwrap());
    }
    fn commit(&self) -> String {
        git(&self.repo, &["add", "--all"]);
        git(&self.repo, &["commit", "-qm", "baseline"]);
        git(&self.repo, &["rev-parse", "HEAD"])
    }
    fn input(&self, commit: String) -> ResolveInput {
        ResolveInput {
            repository: GitBaseline {
                root: self.repo.clone(),
                commit,
            },
            user_location: UserLocation {
                base: self.user.clone(),
                directory: "Storyflow".into(),
            },
            workspace_selection: None,
            trust: HostTrust {
                user_base: fs::canonicalize(&self.user).unwrap(),
                workspace_roots: [fs::canonicalize(self.root.join("team")).unwrap()]
                    .into_iter()
                    .collect(),
            },
            supported_capabilities: ["read", "write", "shell"]
                .map(str::to_owned)
                .into_iter()
                .collect(),
            granted_capabilities: ["read", "write", "shell"]
                .map(str::to_owned)
                .into_iter()
                .collect(),
        }
    }
    fn seed(&self, document: &Value) -> ResolveInput {
        let mut document = document.clone();
        fill_workers(&mut document);
        self.document(self.repo.join("storyflow.json"), &document);
        for role in LEGACY_ROLES {
            self.write(
                self.repo.join(format!("agents/{role}.md")),
                format!("# {role}\n"),
            );
        }
        self.input(self.commit())
    }
    fn user(&self, document: &Value) {
        self.document("user/Storyflow/config.json", document);
    }
    fn workspace(&self, document: &Value) {
        self.document("team/storyflow.workspace.json", document);
    }
    fn register(&self) -> Value {
        json!({"version":2,"scope":"user","workspaces":[self.root.join("team").to_str().unwrap()]})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
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
fn check(id: &str, args: Vec<&str>) -> Value {
    json!({"id":id,"program":"cargo","args":args,"timeout_seconds":60})
}
fn repo() -> Value {
    json!({"version":2,"scope":"repo","policy":{"checks":[check("test",vec!["test"])]}})
}
fn fill_workers(doc: &mut Value) {
    if doc["version"] == 1 {
        return;
    }
    if doc.get("agents").is_none() {
        doc["agents"] = json!([]);
    }
    for role in REQUIRED_WORKERS {
        if !doc["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == *role)
        {
            doc["agents"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":role,"source":format!("agents/{role}.md"),"version":"1"}));
        }
    }
}
fn code(result: ConfigResult<EffectiveConfig>, expected: &str) {
    assert_eq!(result.unwrap_err().code, expected);
}

fn hash_blob(root: &Path, bytes: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn git_baseline_isolation_exact_spelling_and_object_types() {
    let f = Fixture::new();
    let input = f.seed(&repo());
    let at = Origin::new(Scope::Repo, "baseline", "$.agents[0].source");
    fs::write(f.repo.join("storyflow.json"), b"invalid JSON").unwrap();
    fs::remove_file(f.repo.join("agents/planner.md")).unwrap();
    assert_eq!(
        input.repository.read("agents/planner.md", &at).unwrap(),
        b"# planner\n"
    );
    assert_eq!(
        input
            .repository
            .read("agents/Planner.md", &at)
            .unwrap_err()
            .code,
        "missing_artifact"
    );
    for (mode, kind) in [
        ("120000", "symlink"),
        ("160000", "gitlink"),
        ("040000", "directory"),
    ] {
        git(&f.repo, &["reset", "--hard", &input.repository.commit]);
        if mode == "040000" {
            fs::remove_file(f.repo.join("agents/planner.md")).unwrap();
            fs::create_dir(f.repo.join("agents/planner.md")).unwrap();
            fs::write(f.repo.join("agents/planner.md/child"), b"directory").unwrap();
            git(&f.repo, &["add", "--all"]);
        } else {
            let object = if mode == "160000" {
                input.repository.commit.clone()
            } else {
                hash_blob(&f.repo, b"../../outside")
            };
            git(
                &f.repo,
                &[
                    "update-index",
                    "--cacheinfo",
                    &format!("{mode},{object},agents/planner.md"),
                ],
            );
        }
        git(&f.repo, &["commit", "-qm", kind]);
        let baseline = GitBaseline {
            root: f.repo.clone(),
            commit: git(&f.repo, &["rev-parse", "HEAD"]),
        };
        assert_eq!(
            baseline.read("agents/planner.md", &at).unwrap_err().code,
            "invalid_tree_entry"
        );
        if mode == "120000" {
            assert_eq!(
                baseline
                    .read("agents/planner.md/child", &at)
                    .unwrap_err()
                    .code,
                "invalid_tree_entry"
            );
        }
    }
    git(&f.repo, &["reset", "--hard", &input.repository.commit]);
    let blob = hash_blob(&f.repo, &[0xff]);
    git(
        &f.repo,
        &[
            "update-index",
            "--cacheinfo",
            &format!("100644,{blob},agents/planner.md"),
        ],
    );
    git(&f.repo, &["commit", "-qm", "invalid UTF-8"]);
    let baseline = GitBaseline {
        root: f.repo.clone(),
        commit: git(&f.repo, &["rev-parse", "HEAD"]),
    };
    assert_eq!(
        baseline.read("agents/planner.md", &at).unwrap_err().code,
        "invalid_utf8"
    );
}
#[test]
fn git_replacement_objects_cannot_redirect_baseline() {
    let f = Fixture::new();
    let input = f.seed(&repo());
    f.write(f.repo.join("agents/planner.md"), b"# replacement");
    let replacement = f.commit();
    git(
        &f.repo,
        &["replace", &input.repository.commit, &replacement],
    );
    let at = Origin::new(Scope::Repo, "baseline", "artifact");
    assert_eq!(
        input.repository.read("agents/planner.md", &at).unwrap(),
        b"# planner\n"
    );
    let mut bad = input.repository.clone();
    bad.commit = "HEAD".into();
    assert_eq!(
        bad.read("agents/planner.md", &at).unwrap_err().code,
        "invalid_baseline"
    );
    bad.commit = git(&f.repo, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        bad.read("agents/planner.md", &at).unwrap_err().code,
        "invalid_baseline"
    );
}

#[test]
fn parser_schema_inventory() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../docs/config-fixtures/cases.json")).unwrap();
    let mut count = 0;
    for case in cases.iter().filter(|c| c["phase"] == "schema") {
        let doc = &case["inputs"]["documents"][0];
        let scope = match doc["scope"].as_str() {
            Some("user") => Scope::User,
            Some("workspace") => Scope::Workspace,
            _ => Scope::Repo,
        };
        let parsed = parse_document(&serde_json::to_vec(doc).unwrap(), scope, "schema-fixture");
        assert_eq!(
            parsed.is_ok(),
            case["expected"]["outcome"] == "accept",
            "{}: {parsed:?}",
            case["name"]
        );
        count += 1;
    }
    assert_eq!(count, 15);
}

#[test]
fn schema_integral_number_spellings_and_legacy_integer_tokens() {
    let raw=br#"{"version":2.0,"scope":"repo","policy":{"checks":[{"id":"test","program":"cargo","args":[],"timeout_seconds":6e1}]}}"#;
    let Document::Repo(document) = parse_document(raw, Scope::Repo, "integral").unwrap() else {
        panic!("repo")
    };
    assert_eq!(document.policy.checks[0].timeout_seconds, 60);
    let raw = br#"{"version":1.0,"protected":[],"checks":[]}"#;
    assert!(parse_document(raw, Scope::Repo, "legacy").is_err());
}
#[test]
fn parser_rejects_duplicates_nulls_types_and_versions() {
    for (raw, expected, field) in [
        (
            r#"{"version":2,"scope":"repo","scope":"repo"}"#,
            "duplicate_key",
            "$.scope",
        ),
        (
            r#"{"version":2,"scope":"repo","models":{"high":"a","high":"b"}}"#,
            "duplicate_key",
            "$.models.high",
        ),
        (
            r#"{"version":2,"scope":"repo","agents":[{"id":"a","id":"b"}]}"#,
            "duplicate_key",
            "$.agents[0].id",
        ),
        (
            r#"{"version":2,"scope":"repo","models":null}"#,
            "null_value",
            "$.models",
        ),
        (
            r#"{"version":2,"scope":"repo","models":{"high":2}}"#,
            "invalid_type",
            "$.models.high",
        ),
        (
            r#"{"version":99,"scope":"repo"}"#,
            "unsupported_version",
            "$.version",
        ),
        (
            r#"{"version":2,"scope":"workspace"}"#,
            "incorrect_scope",
            "$.scope",
        ),
        (
            r#"{"version":2,"scope":"repo","models":{"unknown":"secret-value"}}"#,
            "unknown_field",
            "$.models.unknown",
        ),
        (
            r#"{"version":2,"scope":"repo","tools":{"deny":["read","read"]}}"#,
            "invalid_capability",
            "$.tools.deny[1]",
        ),
    ] {
        let error = parse_document(raw.as_bytes(), Scope::Repo, "parser-test").unwrap_err();
        assert_eq!((error.code, error.origin.field.as_str()), (expected, field));
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("secret-value")
        );
    }
    for raw in ["", "null", "[]", "{", r#"{"version":2,"scope":"repo"}{}"#] {
        assert!(parse_document(raw.as_bytes(), Scope::Repo, "raw").is_err());
    }
}
#[test]
fn parser_check_and_agent_constraints() {
    for id in ["scope", "SCOPE", "", "a.b", "a\n", &"x".repeat(41)] {
        let doc = json!({"version":2,"scope":"repo","policy":{"checks":[check(id,vec![])]}});
        assert_eq!(
            parse_document(&serde_json::to_vec(&doc).unwrap(), Scope::Repo, "checks")
                .unwrap_err()
                .code,
            "invalid_check"
        );
    }
    for key in ["program", "timeout_seconds", "args", "id"] {
        let mut d = repo();
        d["policy"]["checks"][0]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert_eq!(
            parse_document(&serde_json::to_vec(&d).unwrap(), Scope::Repo, "checks")
                .unwrap_err()
                .code,
            "missing_field"
        );
    }
    let mut d = repo();
    d["policy"]["checks"] = json!([check("TEST", vec![]), check("test", vec![])]);
    let e = parse_document(&serde_json::to_vec(&d).unwrap(), Scope::Repo, "checks").unwrap_err();
    assert_eq!(e.code, "duplicate_check");
    assert!(e.related.is_some());
    for source in [".md", "p.MD", "p.txt"] {
        let d = json!({"version":2,"scope":"repo","agents":[{"id":"planner","source":source,"version":"1"}]});
        assert_eq!(
            parse_document(&serde_json::to_vec(&d).unwrap(), Scope::Repo, "agent")
                .unwrap_err()
                .code,
            "invalid_agent"
        );
    }
}
#[test]
fn lexical_paths_and_protection_matching() {
    let at = Origin::new(Scope::Repo, "test", "path");
    for path in [
        "",
        "/a",
        "a/",
        "a//b",
        "a/./b",
        "../a",
        "a\\b",
        "a:b",
        "a<",
        "a?",
        "a*",
        "a|",
        "a\"",
        "a\n",
        "a.",
        "a ",
        "NUL.md",
        "NUL .md",
        "COM1 .txt",
        "COM¹.txt",
        "lpt9/a",
        "a/CONOUT$",
        "PROGRA~1/file",
        "agents/PLANNE~1.md",
    ] {
        assert_eq!(
            validate_relative_path(path, &at).unwrap_err().code,
            "invalid_path",
            "{path}"
        );
    }
    for path in ["future/output", "a-b/c_d.md", "é/中文"] {
        validate_relative_path(path, &at).unwrap();
    }
    assert!(protected_matches("Future/Output/new.txt", "future/output"));
    assert!(!protected_matches(
        "future/outputs/new.txt",
        "future/output"
    ));
    assert!(protected_matches("Ä/new", "ä"));
}
#[test]
fn invoking_environment_paths_are_explicit() {
    let env = UserEnvironment {
        appdata: Some(PathBuf::from("relative")),
        ..Default::default()
    };
    assert!(UserLocation::from_environment(UserPlatform::Windows, &env).is_err());
    #[cfg(windows)]
    let base = std::env::current_dir().unwrap();
    #[cfg(not(windows))]
    let base = std::env::temp_dir();
    let env = UserEnvironment {
        appdata: Some(base.clone()),
        xdg_config_home: Some(PathBuf::from("relative")),
        home: Some(base.clone()),
    };
    assert_eq!(
        UserLocation::from_environment(UserPlatform::Unix, &env)
            .unwrap()
            .base,
        base.join(".config")
    );
    assert_eq!(
        UserLocation::from_environment(UserPlatform::Windows, &env)
            .unwrap()
            .directory,
        "Storyflow"
    );
    let env = UserEnvironment {
        xdg_config_home: Some(base.clone()),
        ..Default::default()
    };
    assert_eq!(
        UserLocation::from_environment(UserPlatform::Unix, &env)
            .unwrap()
            .base,
        base
    );
    assert!(
        UserLocation::from_environment(UserPlatform::Unix, &UserEnvironment::default()).is_err()
    );
}

#[cfg(any(windows, target_os = "linux"))]
mod resolver {
    use super::*;

    const DEFERRED: &[&str] = &[
        "override cannot skip mandatory gate",
        "future protected descendant",
        "protected sibling is outside prefix",
        "platform alias in candidate",
    ];
    #[test]
    fn semantic_fixture_inventory() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../docs/config-fixtures/cases.json")).unwrap();
        let mut tested = 0;
        let mut deferred = 0;
        for case in cases.iter().filter(|c| c["phase"] == "resolver-pending") {
            let name = case["name"].as_str().unwrap();
            if DEFERRED.contains(&name) {
                deferred += 1;
                continue;
            }
            if let Some(raw) = case["inputs"]["raw"].as_str() {
                assert_eq!(
                    parse_document(raw.as_bytes(), Scope::Repo, name)
                        .unwrap_err()
                        .code,
                    case["expected"]["diagnostic"].as_str().unwrap()
                );
                tested += 1;
                continue;
            }
            let inputs = &case["inputs"];
            let repo_relative = inputs["repoRoot"]
                .as_str()
                .unwrap_or("/fixture/team/project")
                .strip_prefix("/fixture/")
                .unwrap();
            let f = Fixture::at(repo_relative);
            let mut documents = inputs["documents"].as_array().unwrap().clone();
            let mut registrations = Vec::new();
            for doc in &mut documents {
                if doc["scope"] == "user"
                    && let Some(roots) = doc.get_mut("workspaces").and_then(Value::as_array_mut)
                {
                    for root in roots {
                        let path = f
                            .root
                            .join(root.as_str().unwrap().strip_prefix("/fixture/").unwrap());
                        registrations.push(path.clone());
                        *root = json!(path.to_str().unwrap());
                    }
                }
            }
            // The semantic fixtures omit unrelated required workers. Supply neutral
            // pinned contracts only for roles not declared anywhere in the fixture.
            let declared: BTreeSet<String> = documents
                .iter()
                .flat_map(|d| {
                    d["agents"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a["id"].as_str().map(str::to_owned))
                })
                .collect();
            let mut workspace_index = 0;
            let mut repo_doc = None;
            for doc in &mut documents {
                let scope = doc["scope"].as_str().unwrap_or("repo");
                let root = match scope {
                    "user" => f.user.join("Storyflow"),
                    "workspace" => {
                        let root = registrations[workspace_index].clone();
                        workspace_index += 1;
                        root
                    }
                    _ => f.repo.clone(),
                };
                if scope == "repo" && doc["version"] == 2 {
                    if doc.get("agents").is_none() {
                        doc["agents"] = json!([]);
                    }
                    for role in REQUIRED_WORKERS {
                        if !declared.contains(*role) {
                            doc["agents"].as_array_mut().unwrap().push(json!({"id":role,"source":format!("agents/{role}.md"),"version":"1"}));
                        }
                    }
                }
                let scope = doc["scope"].as_str().unwrap_or("repo");
                let bytes = inputs["bytes"][scope].as_str().unwrap_or("# contract\n");
                for a in doc["agents"].as_array().into_iter().flatten() {
                    let source = a["source"].as_str().unwrap();
                    if validate_relative_path(
                        source,
                        &Origin::new(Scope::Repo, "fixture", "source"),
                    )
                    .is_ok()
                    {
                        f.write(root.join(source), bytes);
                    }
                }
                match scope {
                    "user" => f.document(root.join("config.json"), doc),
                    "workspace" => f.document(root.join("storyflow.workspace.json"), doc),
                    _ => {
                        repo_doc = Some(doc.clone());
                        f.document(root.join("storyflow.json"), doc);
                    }
                }
            }
            for role in LEGACY_ROLES {
                let path = f.repo.join(format!("agents/{role}.md"));
                if !path.exists() {
                    f.write(path, "# contract\n");
                }
            }
            if inputs.get("baselineEntries").is_some()
                && inputs["baselineEntries"].get("storyflow.json").is_none()
            {
                let _ = fs::remove_file(f.repo.join("storyflow.json"));
            }
            if let Some(entries) = inputs["baselineEntries"].as_object() {
                for (path, entry) in entries {
                    if path == "storyflow.json" {
                        continue;
                    }
                    if let Some(bytes) = entry["bytes"].as_str() {
                        f.write(f.repo.join(path), bytes);
                    }
                    if entry["mode"] == "040000" {
                        let path = f.repo.join(path);
                        let _ = fs::remove_file(&path);
                        fs::create_dir_all(&path).unwrap();
                        f.write(path.join("child"), "directory");
                    }
                }
                if !entries.contains_key("agents/planner.md")
                    && repo_doc.as_ref().is_some_and(|r| {
                        r["agents"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|a| a["id"] == "planner")
                    })
                    && name == "missing pinned agent"
                {
                    fs::remove_file(f.repo.join("agents/planner.md")).unwrap();
                }
            }
            let initial = f.commit();
            if let Some(entries) = inputs["baselineEntries"].as_object() {
                let mut changed = false;
                for (path, entry) in entries {
                    if matches!(entry["mode"].as_str(), Some("120000" | "160000")) {
                        let mode = entry["mode"].as_str().unwrap();
                        let object = if mode == "160000" {
                            initial.clone()
                        } else {
                            hash_blob(&f.repo, b"../../outside")
                        };
                        git(
                            &f.repo,
                            &[
                                "update-index",
                                "--cacheinfo",
                                &format!("{mode},{object},{path}"),
                            ],
                        );
                        changed = true;
                    }
                }
                if changed {
                    git(&f.repo, &["commit", "-qm", "special entries"]);
                }
            }
            let mut input = f.input(git(&f.repo, &["rev-parse", "HEAD"]));
            if let Some(document) = inputs.get("worktreeDocument") {
                f.document(f.repo.join("storyflow.json"), document);
            }
            input.trust.workspace_roots.clear();
            for root in inputs["trustedRoots"].as_array().into_iter().flatten() {
                let path = f
                    .root
                    .join(root.as_str().unwrap().strip_prefix("/fixture/").unwrap());
                fs::create_dir_all(&path).unwrap();
                input
                    .trust
                    .workspace_roots
                    .insert(fs::canonicalize(path).unwrap());
            }
            if let Some(grants) = inputs["hostGrants"].as_array() {
                input.granted_capabilities = grants
                    .iter()
                    .map(|s| s.as_str().unwrap().to_owned())
                    .collect();
            }
            if let Some(entries) = inputs["worktreeEntries"].as_object() {
                for (path, value) in entries {
                    if value.is_null() {
                        fs::remove_file(f.repo.join(path)).unwrap();
                    } else {
                        f.document(f.repo.join(path), value);
                    }
                }
            }
            if inputs.get("unregisteredMarker").is_some() {
                f.write(
                    "team/storyflow.workspace.json",
                    b"MALFORMED - MUST NOT READ",
                );
            }
            if let Some(links) = inputs["links"].as_object() {
                for (link, target) in links {
                    let link = f.root.join(link.strip_prefix("/fixture/").unwrap());
                    let target = f
                        .root
                        .join(target.as_str().unwrap().strip_prefix("/fixture/").unwrap());
                    fs::create_dir_all(&target).unwrap();
                    f.write(target.join("planner.md"), "# outside");
                    let _ = fs::remove_dir_all(&link);
                    junction(&link, &target);
                }
            }
            let result = resolve(&input);
            if case["expected"]["outcome"] == "accept" {
                let effective = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert!(!effective.policy.checks.is_empty(), "{name}");
                if name == "identical inherited agent" {
                    assert_eq!(effective.agents["planner"].origin.scope, Some(Scope::User));
                }
                if name == "explicit workspace override" {
                    assert_eq!(
                        effective.agents["planner"].origin.scope,
                        Some(Scope::Workspace)
                    );
                }
                if name == "explicit repo override" {
                    assert_eq!(effective.agents["planner"].version, "3");
                }
                if name == "pinned baseline survives worktree edits" {
                    assert_eq!(effective.agents["planner"].content, "# Baseline");
                }
            } else {
                assert_eq!(
                    result.unwrap_err().code,
                    case["expected"]["diagnostic"].as_str().unwrap(),
                    "{name}"
                );
            }
            tested += 1;
        }
        assert_eq!((tested, deferred), (26, 4));
    }
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        // Use a native API via PowerShell; junction creation needs no symlink privilege.
        let status=Command::new("pwsh").args(["-NoProfile","-Command","New-Item -ItemType Junction -Path $env:SF_TEST_LINK -Target $env:SF_TEST_TARGET | Out-Null"]).env("SF_TEST_LINK",link).env("SF_TEST_TARGET",target).status().unwrap();
        assert!(status.success());
    }
    #[cfg(target_os = "linux")]
    fn junction(link: &Path, target: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn repo_only_and_missing_optional_user() {
        let f = Fixture::new();
        let input = f.seed(&repo());
        let first = resolve(&input).unwrap();
        let second = resolve(&input).unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
        assert_eq!(first.scopes.len(), 1);
        assert_eq!(first.agents.len(), 3);
        assert_eq!(first.policy.checks.len(), 1);
        for path in BUILTIN_PROTECTED {
            assert!(first.policy.protected.iter().any(|p| p.value == *path));
        }
        fs::remove_dir(f.user.join("Storyflow")).unwrap();
        assert_eq!(resolve(&input).unwrap(), first);
    }
    #[test]
    fn models_and_policy_are_composed_in_order() {
        let f = Fixture::new();
        let mut u = f.register();
        u["models"] = json!({"economy":"cheap","high":"user"});
        u["policy"] = json!({"checks":[check("lint",vec!["clippy"]),check("TEST",vec!["test"])],"protected":["future/output"]});
        f.user(&u);
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project"],"models":{"standard":"shared","high":"workspace"},"policy":{"checks":[check("fmt",vec!["fmt"])]}}));
        let mut r = repo();
        r["models"] = json!({"high":"repo"});
        r["policy"]["protected"] = json!([]);
        let input = f.seed(&r);
        let e = resolve(&input).unwrap();
        assert_eq!(
            serde_json::to_vec(&e).unwrap(),
            serde_json::to_vec(&resolve(&input).unwrap()).unwrap()
        );
        assert_eq!(e.scopes.len(), 3);
        assert_eq!(e.models["economy"].origin.scope, Some(Scope::User));
        assert_eq!(e.models["standard"].origin.scope, Some(Scope::Workspace));
        assert_eq!(e.models["high"].value, "repo");
        assert_eq!(
            e.policy
                .checks
                .iter()
                .map(|c| c.value.id.as_str())
                .collect::<Vec<_>>(),
            ["lint", "TEST", "fmt"]
        );
        assert!(
            e.policy
                .protected
                .iter()
                .any(|p| p.value == "future/output")
        );
    }
    #[test]
    fn user_repo_and_no_matching_workspace() {
        let f = Fixture::new();
        let mut u = f.register();
        u["models"] = json!({"high":"user"});
        f.user(&u);
        let other = f.root.join("team/other");
        fs::create_dir(&other).unwrap();
        git(&other, &["init", "-q"]);
        f.workspace(&json!({"version":2,"scope":"workspace","members":["other"],"agents":[{"id":"unused","source":"absent.md","version":"1"}]}));
        let input = f.seed(&repo());
        let e = resolve(&input).unwrap();
        assert_eq!(e.scopes.len(), 2);
        assert_eq!(e.models["high"].value, "user");
    }
    #[test]
    fn roots_trust_markers_duplicates_members_and_selection() {
        let f = Fixture::new();
        f.user(&f.register());
        let mut input = f.seed(&repo());
        input.trust.workspace_roots.clear();
        code(resolve(&input), "untrusted_workspace");
        input
            .trust
            .workspace_roots
            .insert(fs::canonicalize(f.root.join("team")).unwrap());
        code(resolve(&input), "missing_workspace_marker");
        f.write("team/storyflow.workspace.json", b"{");
        code(resolve(&input), "invalid_json");
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project","project"]}));
        code(resolve(&input), "duplicate_member");
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project/agents"]}));
        code(resolve(&input), "invalid_member");
        f.workspace(&json!({"version":2,"scope":"workspace","members":["../outside"]}));
        code(resolve(&input), "invalid_path");
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project"]}));
        let mut u = f.register();
        let duplicate = u["workspaces"][0].clone();
        u["workspaces"].as_array_mut().unwrap().push(duplicate);
        f.user(&u);
        code(resolve(&input), "duplicate_workspace");
        f.user(&f.register());
        input.workspace_selection = Some(f.root.join("team/project"));
        code(resolve(&input), "unauthorized_selection");
        input.workspace_selection = Some(f.root.join("team"));
        assert!(resolve(&input).is_ok());
        f.workspace(&json!({"version":2,"scope":"workspace","members":[]}));
        code(resolve(&input), "workspace_membership_mismatch");
    }
    #[test]
    fn unavailable_and_untrusted_user_location() {
        let f = Fixture::new();
        let mut input = f.seed(&repo());
        input.trust.user_base = f.root.clone();
        code(resolve(&input), "untrusted_user_location");
        let mut input = f.input(input.repository.commit.clone());
        f.write("user/Storyflow/.git", b"broken gitdir marker");
        code(resolve(&input), "untrusted_user_location");
        input.user_location.base = f.root.join("absent");
        code(resolve(&input), "unavailable_user_location");
        input.user_location.base = f.repo.clone();
        input.trust.user_base = fs::canonicalize(&f.repo).unwrap();
        code(resolve(&input), "untrusted_user_location");
    }
    #[test]
    fn ambiguous_membership_ignores_explicit_selection() {
        let f = Fixture::new();
        let mut input = f.seed(&repo());
        let mut u = f.register();
        u["workspaces"]
            .as_array_mut()
            .unwrap()
            .push(json!(f.root.to_str().unwrap()));
        f.user(&u);
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project"]}));
        f.document(
            "storyflow.workspace.json",
            &json!({"version":2,"scope":"workspace","members":["team/project"]}),
        );
        input
            .trust
            .workspace_roots
            .insert(fs::canonicalize(&f.root).unwrap());
        input.workspace_selection = Some(f.root.join("team"));
        let e = resolve(&input).unwrap_err();
        assert_eq!(e.code, "ambiguous_workspace");
        assert!(e.related.is_some());
    }
    #[test]
    fn inherited_agents_dedup_override_and_conflicts() {
        let f = Fixture::new();
        let mut u = f.register();
        u["agents"] = json!([{"id":"planner","source":"agents/planner.md","version":"1"}]);
        f.user(&u);
        f.write("user/Storyflow/agents/planner.md", "# planner\n");
        let mut w = json!({"version":2,"scope":"workspace","members":["project"],"agents":[{"id":"planner","source":"agents/planner.md","version":"1"}]});
        f.workspace(&w);
        f.write("team/agents/planner.md", "# planner\n");
        let input = f.seed(&repo());
        let e = resolve(&input).unwrap();
        assert_eq!(e.agents["planner"].origin.scope, Some(Scope::User));
        w["agents"][0]["override"] = json!(true);
        f.workspace(&w);
        assert_eq!(
            resolve(&input).unwrap().agents["planner"].origin.scope,
            Some(Scope::Workspace)
        );
        // Equal bytes/version still deduplicate a child's unflagged declaration.
    }
    #[test]
    fn explicit_override_targets_nearest_parent_and_keeps_exact_bytes() {
        let f = Fixture::new();
        f.user(&json!({"version":2,"scope":"user","agents":[{"id":"planner","source":"p.md","version":"1"}]}));
        f.write("user/Storyflow/p.md", "# user\r\n");
        let mut r = repo();
        r["agents"] =
            json!([{"id":"planner","source":"agents/planner.md","version":"2","override":true}]);
        let input = f.seed(&r);
        let e = resolve(&input).unwrap();
        assert_eq!(e.agents["planner"].content, "# planner\n");
        assert_eq!(e.agents["planner"].version, "2");
        fs::remove_file(f.user.join("Storyflow/config.json")).unwrap();
        code(resolve(&input), "missing_override_target");
    }
    #[test]
    fn legacy_normalization_and_implicit_agent_conflict() {
        let f = Fixture::new();
        let r = json!({"version":1,"protected":["src","legacy./odd*"],"checks":[check("test",vec!["test"])]});
        let input = f.seed(&r);
        let e = resolve(&input).unwrap();
        assert_eq!(e.agents.len(), 4);
        assert_eq!(e.agents["planner"].version, "legacy-v1");
        f.user(
            &json!({"version":2,"scope":"user","policy":{"checks":[check("lint",vec!["clippy"])]}}),
        );
        assert_eq!(resolve(&input).unwrap().policy.checks.len(), 2);
        f.user(
            &json!({"version":2,"scope":"user","policy":{"checks":[check("TEST",vec!["check"])]}}),
        );
        code(resolve(&input), "check_conflict");
        f.user(&json!({"version":2,"scope":"user","agents":[{"id":"planner","source":"p.md","version":"1"}]}));
        f.write("user/Storyflow/p.md", "# planner\n");
        let e = resolve(&input).unwrap_err();
        assert_eq!(e.code, "agent_conflict");
        assert_eq!(e.related.unwrap().scope, Some(Scope::User));
        // Explicit v2 migration resolves the implicit legacy collision.
        let mut migrated = repo();
        migrated["policy"]["protected"] = json!(["src"]);
        migrated["agents"] = json!([{"id":"planner","source":"agents/planner.md","version":"legacy-v1","override":true}]);
        fill_workers(&mut migrated);
        f.document(f.repo.join("storyflow.json"), &migrated);
        let migrated_input = f.input(f.commit());
        assert_eq!(
            resolve(&migrated_input).unwrap().agents["planner"].version,
            "legacy-v1"
        );
    }
    #[test]
    fn capabilities_only_reduce_and_deny_wins() {
        let f = Fixture::new();
        f.user(&json!({"version":2,"scope":"user","tools":{"allow":["read","write"],"deny":["write"]}}));
        let mut r = repo();
        r["tools"] = json!({"allow":["read"]});
        let input = f.seed(&r);
        let e = resolve(&input).unwrap();
        assert_eq!(
            e.capabilities.granted,
            ["read".to_owned()].into_iter().collect()
        );
        assert_eq!(e.capabilities.constraints.len(), 3);
        assert_eq!(
            e.capabilities.provenance["write"].origin.field,
            "$.tools.deny[0]"
        );
        assert_eq!(
            e.capabilities.provenance["shell"].origin.field,
            "$.tools.allow"
        );
        f.user(&json!({"version":2,"scope":"user","tools":{"allow":["shell"]}}));
        code(resolve(&input), "capability_expansion");
        f.user(&json!({"version":2,"scope":"user","tools":{"deny":["unknown"]}}));
        code(resolve(&input), "unknown_capability");
    }
    #[test]
    fn requires_declared_check_and_all_workflow_workers() {
        let f = Fixture::new();
        let input = f.seed(&json!({"version":2,"scope":"repo"}));
        code(resolve(&input), "missing_check");
        f.document("team/project/storyflow.json", &repo());
        let input = f.input(f.commit());
        code(resolve(&input), "missing_worker_role");
    }

    #[test]
    fn authorization_precedes_every_marker_and_case_aliases_deduplicate() {
        let f = Fixture::new();
        let mut u = f.register();
        let second = f.root.join("second");
        fs::create_dir(&second).unwrap();
        u["workspaces"]
            .as_array_mut()
            .unwrap()
            .push(json!(second.to_str().unwrap()));
        f.user(&u);
        f.write(
            "team/storyflow.workspace.json",
            b"MALFORMED - NOT AUTHORIZED TO READ YET",
        );
        let input = f.seed(&repo());
        code(resolve(&input), "untrusted_workspace");
        #[cfg(windows)]
        {
            let mut u = f.register();
            let alias = f.root.join("TEAM");
            u["workspaces"]
                .as_array_mut()
                .unwrap()
                .push(json!(alias.to_str().unwrap()));
            f.user(&u);
            code(resolve(&input), "duplicate_workspace");
        }
    }

    #[test]
    fn missing_optional_file_differs_from_invalid_or_unavailable_file() {
        let f = Fixture::new();
        let input = f.seed(&repo());
        fs::create_dir(f.user.join("Storyflow/config.json")).unwrap();
        code(resolve(&input), "invalid_artifact_type");
        fs::remove_dir(f.user.join("Storyflow/config.json")).unwrap();
        f.write("user/Storyflow/config.json", b"{");
        code(resolve(&input), "invalid_json");
        fs::write(f.user.join("Storyflow/config.json"), [0xff]).unwrap();
        code(resolve(&input), "invalid_utf8");
    }

    #[test]
    fn explicit_false_and_byte_differences_do_not_replace_inherited_contracts() {
        let f = Fixture::new();
        f.user(&json!({"version":2,"scope":"user","agents":[{"id":"planner","source":"p.md","version":"1"}]}));
        f.write("user/Storyflow/p.md", b"# planner\r\n");
        let mut r = repo();
        r["agents"] =
            json!([{"id":"planner","source":"agents/planner.md","version":"1","override":false}]);
        let input = f.seed(&r);
        code(resolve(&input), "agent_conflict");
    }

    #[test]
    fn regrant_conflict_points_to_the_actual_removal() {
        let f = Fixture::new();
        let mut u = f.register();
        u["tools"] = json!({"deny":["write"]});
        f.user(&u);
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project"],"tools":{"deny":["shell"]}}));
        let mut r = repo();
        r["tools"] = json!({"allow":["write"]});
        let input = f.seed(&r);
        let e = resolve(&input).unwrap_err();
        assert_eq!(e.code, "capability_expansion");
        let related = e.related.unwrap();
        assert_eq!(related.scope, Some(Scope::User));
        assert_eq!(related.field, "$.tools.deny[0]");
    }

    #[test]
    fn content_digest_is_over_consumed_bytes() {
        use sha2::{Digest, Sha256};
        let f = Fixture::new();
        let input = f.seed(&repo());
        let e = resolve(&input).unwrap();
        assert_eq!(
            e.agents["planner"].sha256,
            format!("{:x}", Sha256::digest(b"# planner\n"))
        );
        assert_eq!(
            e.agents["planner"].source_path,
            format!("{}:agents/planner.md", input.repository.commit)
        );
        assert_eq!(e.policy.checks[0].origin.field, "$.policy.checks[0]");
        assert_eq!(
            e.scopes[0].baseline.as_deref(),
            Some(input.repository.commit.as_str())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_user_config_redirect_into_checkout_is_rejected_before_read() {
        let f = Fixture::new();
        let input = f.seed(&repo());
        let checkout = f.user.join("Storyflow/checkout");
        fs::create_dir(&checkout).unwrap();
        git(&checkout, &["init", "-q"]);
        f.document(
            checkout.join("config.json"),
            &json!({"version":2,"scope":"user","models":{"high":"project-controlled"}}),
        );
        std::os::unix::fs::symlink("checkout/config.json", f.user.join("Storyflow/config.json"))
            .unwrap();
        let error = resolve(&input).unwrap_err();
        assert_eq!(error.code, "untrusted_user_location");
        assert!(error.origin.source.ends_with("Storyflow/config.json"));
        // The target's bytes must not be consumed before its ancestry is trusted.
        fs::write(checkout.join("config.json"), [0xff]).unwrap();
        code(resolve(&input), "untrusted_user_location");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_dangling_optional_config_is_invalid_but_absent_entry_is_allowed() {
        let f = Fixture::new();
        let input = f.seed(&repo());
        let target = f.user.join("Storyflow/user.json");
        f.document(&target, &json!({"version":2,"scope":"user","policy":{"checks":[check("lint",vec!["clippy"])],"protected":["future/output"]},"tools":{"deny":["write"]}}));
        let link = f.user.join("Storyflow/config.json");
        std::os::unix::fs::symlink("user.json", &link).unwrap();
        let effective = resolve(&input).unwrap();
        assert_eq!(effective.policy.checks.len(), 2);
        assert!(!effective.capabilities.granted.contains("write"));
        fs::remove_file(target).unwrap();
        let error = resolve(&input).unwrap_err();
        assert_eq!(error.code, "dangling_symlink");
        assert!(error.origin.source.ends_with("Storyflow/config.json"));
        fs::remove_file(link).unwrap();
        assert_eq!(resolve(&input).unwrap().scopes.len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_dangling_optional_user_directory_is_invalid() {
        let f = Fixture::new();
        let mut input = f.seed(&repo());
        let directory = f.user.join("Storyflow");
        fs::remove_dir(&directory).unwrap();
        std::os::unix::fs::symlink("missing-directory", &directory).unwrap();
        code(resolve(&input), "dangling_symlink");
        fs::remove_file(directory).unwrap();
        assert_eq!(resolve(&input).unwrap().scopes.len(), 1);
        // Also inspect intermediate entries in an explicitly supplied location.
        std::os::unix::fs::symlink("missing-parent", f.user.join("alias")).unwrap();
        input.user_location.directory = "alias/Storyflow".into();
        code(resolve(&input), "dangling_symlink");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_relative_in_root_links_and_member_identity() {
        let f = Fixture::new();
        let input = f.seed(&repo());
        let mut u = f.register();
        u["agents"] = json!([{"id":"helper","source":"linked.md","version":"1"}]);
        f.user(&u);
        f.write("user/Storyflow/physical.md", b"# in root");
        std::os::unix::fs::symlink("physical.md", f.user.join("Storyflow/linked.md")).unwrap();
        std::os::unix::fs::symlink("project", f.root.join("team/alias")).unwrap();
        f.workspace(&json!({"version":2,"scope":"workspace","members":["project","alias"]}));
        code(resolve(&input), "duplicate_member");
        f.workspace(&json!({"version":2,"scope":"workspace","members":["alias"]}));
        let e = resolve(&input).unwrap();
        assert_eq!(e.agents["helper"].content, "# in root");
        fs::remove_file(f.user.join("Storyflow/linked.md")).unwrap();
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&f.user, outside.join("return")).unwrap();
        std::os::unix::fs::symlink(
            "../../outside/return/Storyflow/physical.md",
            f.user.join("Storyflow/linked.md"),
        )
        .unwrap();
        code(resolve(&input), "path_escape");
    }
}
