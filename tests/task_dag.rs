use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
};
use storyflow::tasks::*;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    context: RunContext,
}
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/task-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(root.join(".storyflow/demo-001/events")).unwrap();
        Self {
            root,
            context: RunContext {
                story: "demo-001".into(),
                base: "1".repeat(40),
                story_revision: 0,
                stage: TaskStage::Plan,
                blocked: false,
                scope: vec!["app".into(), "future".into()],
                protected: vec!["app/private".into()],
            },
        }
    }
    fn open(&self) -> TaskJournal {
        TaskJournal::open(&self.root, self.context.clone()).unwrap()
    }
    fn events(&self) -> PathBuf {
        self.root.join(".storyflow/demo-001/tasks/events")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn spec(id: &str, deps: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        story: "demo-001".into(),
        prerequisites: deps.iter().map(|s| (*s).into()).collect(),
        stage: TaskStage::Plan,
        read_scope: vec!["app".into()],
        write_scope: vec![format!("app/{id}.txt")],
        acceptance_criteria: vec!["AC1".into()],
        decision_owner: "orchestrator".into(),
    }
}
fn graph() -> TaskGraph {
    TaskGraph {
        schema: 1,
        story: "demo-001".into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        tasks: vec![spec("a", &[]), spec("b", &[]), spec("c", &["a", "b"])],
    }
}
fn result(id: &str, status: ResultStatus) -> WorkerResult {
    WorkerResult {
        schema: 1,
        story: "demo-001".into(),
        task: id.into(),
        task_revision: 1,
        status,
        report: "AC1 verified".into(),
        commit: None,
        artifacts: Vec::new(),
    }
}
fn code<T>(result: TaskResult<T>, expected: &str) {
    match result {
        Ok(_) => panic!("expected {expected}"),
        Err(error) => assert_eq!(error.code, expected, "{error}"),
    }
}

#[test]
fn dag_rejects_duplicates_cycles_unknown_self_dependencies_and_scopes() {
    let f = Fixture::new();
    graph().validate(&f.context).unwrap();
    let mut g = graph();
    g.tasks.push(spec("a", &[]));
    code(g.validate(&f.context), "duplicate_task");
    let mut g = graph();
    g.tasks[0].prerequisites = vec!["c".into()];
    code(g.validate(&f.context), "task_cycle");
    let mut g = graph();
    g.tasks[0].prerequisites = vec!["missing".into()];
    code(g.validate(&f.context), "unknown_task_dependency");
    let mut g = graph();
    g.tasks[0].prerequisites = vec!["a".into()];
    code(g.validate(&f.context), "invalid_task_dependency");
    let mut g = graph();
    g.tasks[2].prerequisites = vec!["a".into(), "a".into()];
    code(g.validate(&f.context), "invalid_task_dependency");
    for path in [
        "../escape",
        "app/../escape",
        "C:/escape",
        "app/CON",
        "app/SHORT~1",
        "app//file",
    ] {
        let mut g = graph();
        g.tasks[0].write_scope = vec![path.into()];
        code(g.validate(&f.context), "invalid_task_scope");
    }
    for path in ["outside", "app-sibling/file"] {
        let mut g = graph();
        g.tasks[0].write_scope = vec![path.into()];
        code(g.validate(&f.context), "task_scope");
    }
    for path in ["app/private", "app/private/child", "app/PRIVATE/child"] {
        let mut g = graph();
        g.tasks[0].write_scope = vec![path.into()];
        code(g.validate(&f.context), "task_protected");
    }
    let mut g = graph();
    g.tasks[0].write_scope = vec!["app/private-sibling".into()];
    g.validate(&f.context).unwrap();
}

#[test]
fn dependencies_unlock_only_after_both_validated_successes_and_replay_exactly() {
    let f = Fixture::new();
    let mut j = f.open();
    j.initialize(graph()).unwrap();
    assert_eq!(j.view().ready, ["a", "b"]);
    code(j.packet("c"), "task_not_ready");
    code(j.start(0, "c", 0), "task_not_ready");
    code(
        j.complete(0, result("c", ResultStatus::Succeeded)),
        "stale_task_revision",
    );
    let packet = j.start(0, "a", 0).unwrap();
    assert_eq!(packet.task.status, TaskStatus::Running);
    assert_eq!(packet.task.revision, 1);
    j.complete(1, result("a", ResultStatus::Succeeded)).unwrap();
    assert_eq!(j.view().ready, ["b"]);
    code(
        j.complete(2, result("a", ResultStatus::Succeeded)),
        "stale_task_revision",
    );
    j.start(2, "b", 0).unwrap();
    j.complete(3, result("b", ResultStatus::Succeeded)).unwrap();
    assert_eq!(j.view().ready, ["c"]);
    let packet = j.packet("c").unwrap();
    assert_eq!(packet.prerequisites.len(), 2);
    let state = j.view().clone();
    let export = state.markdown();
    drop(j);
    let j = f.open();
    assert_eq!(&state, j.view());
    assert_eq!(export, j.view().markdown());
    assert_eq!(packet, j.packet("c").unwrap());
}

#[test]
fn failed_or_blocked_results_do_not_unlock_dependencies() {
    for status in [ResultStatus::Failed, ResultStatus::Blocked] {
        let f = Fixture::new();
        let mut j = f.open();
        j.initialize(graph()).unwrap();
        j.start(0, "a", 0).unwrap();
        j.complete(1, result("a", status)).unwrap();
        j.start(2, "b", 0).unwrap();
        j.complete(3, result("b", ResultStatus::Succeeded)).unwrap();
        assert!(j.view().ready.is_empty());
        code(j.packet("c"), "task_not_ready");
    }
}

#[test]
fn stale_store_task_and_story_revisions_fail_without_publishing_events() {
    let f = Fixture::new();
    let mut j = f.open();
    j.initialize(graph()).unwrap();
    j.start(0, "a", 0).unwrap();
    code(j.start(0, "b", 0), "stale_task_store_revision");
    code(j.start(1, "b", 1), "stale_task_revision");
    let before = j.view().clone();
    let mut bad = result("a", ResultStatus::Succeeded);
    bad.report = " ".into();
    code(j.complete(1, bad), "invalid_task_result");
    assert_eq!(&before, j.view());
    drop(j);
    let mut context = f.context.clone();
    context.story_revision = 1;
    context.stage = TaskStage::Implement;
    let mut j = TaskJournal::open(&f.root, context).unwrap();
    assert_eq!(&before, j.view());
    code(j.packet("a"), "stale_story_context");
    code(
        j.complete(1, result("a", ResultStatus::Succeeded)),
        "stale_story_context",
    );
}

#[test]
fn empty_task_state_is_additive_and_immutable_graph_has_exact_parent_binding() {
    let f = Fixture::new();
    let mut j = f.open();
    assert_eq!(j.view().revision, None);
    assert!(j.view().tasks.is_empty());
    let mut g = graph();
    g.base = "2".repeat(40);
    code(j.initialize(g), "task_identity");
    let mut g = graph();
    g.story_revision = 1;
    code(j.initialize(g), "stale_story_context");
    let mut g = graph();
    g.stage = TaskStage::Implement;
    code(j.initialize(g), "stale_story_context");
    j.initialize(graph()).unwrap();
    code(j.initialize(graph()), "task_already_initialized");
}

#[test]
fn task_lock_refuses_concurrent_writer_and_releases_after_drop() {
    let f = Fixture::new();
    let j = f.open();
    let root = f.root.clone();
    let context = f.context.clone();
    let result = std::thread::spawn(move || TaskJournal::open(&root, context).map(|_| ()))
        .join()
        .unwrap();
    code(result, "task_locked");
    drop(j);
    f.open();
}

#[test]
fn corrupt_gap_unknown_and_partial_events_fail_closed() {
    for kind in ["checksum", "gap", "unknown", "partial", "oversized"] {
        let f = Fixture::new();
        let mut j = f.open();
        j.initialize(graph()).unwrap();
        j.start(0, "a", 0).unwrap();
        drop(j);
        let first = f.events().join("000000000000.json");
        match kind {
            "checksum" => {
                let mut v: Value = serde_json::from_slice(&fs::read(&first).unwrap()).unwrap();
                v["sha256"] = json!("0".repeat(64));
                fs::write(first, serde_json::to_vec(&v).unwrap()).unwrap();
            }
            "gap" => fs::remove_file(first).unwrap(),
            "unknown" => fs::write(f.events().join("unexpected.json"), "{}").unwrap(),
            "partial" => fs::write(first, "{\"body\":").unwrap(),
            "oversized" => fs::write(first, vec![b' '; MAX_EVENT_BYTES + 1]).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            TaskJournal::open(&f.root, f.context.clone()).is_err(),
            "{kind}"
        );
    }
}

#[test]
fn interrupted_pending_write_does_not_publish_and_bad_result_cannot_unlock() {
    let f = Fixture::new();
    let mut j = f.open();
    j.initialize(graph()).unwrap();
    j.start(0, "a", 0).unwrap();
    let before = j.view().clone();
    drop(j);
    fs::write(f.events().join("pending.tmp"), "incomplete").unwrap();
    let mut j = f.open();
    assert_eq!(&before, j.view());
    let mut bad = result("a", ResultStatus::Succeeded);
    bad.story = "other-001".into();
    code(j.complete(1, bad), "task_identity");
    let mut bad = result("a", ResultStatus::Succeeded);
    bad.artifacts.push(Artifact {
        path: "../escape".into(),
        sha256: "0".repeat(64),
    });
    code(j.complete(1, bad), "invalid_task_scope");
    let mut bad = result("a", ResultStatus::Succeeded);
    bad.artifacts.push(Artifact {
        path: "evidence/log".into(),
        sha256: "not-a-digest".into(),
    });
    code(j.complete(1, bad), "invalid_task_artifact");
    j.complete(1, result("a", ResultStatus::Succeeded)).unwrap();
    assert_eq!(j.view().ready, ["b"]);
}

#[test]
fn packet_and_result_fixture_contracts_are_strict_and_roundtrip() {
    let packet: TaskPacket =
        serde_json::from_str(include_str!("../docs/task-fixtures/packet.json")).unwrap();
    let result: WorkerResult =
        serde_json::from_str(include_str!("../docs/task-fixtures/result.json")).unwrap();
    assert_eq!(packet.task.spec.id, result.task);
    assert_eq!(packet.task.revision, result.task_revision);
    assert_eq!(
        packet,
        serde_json::from_slice::<TaskPacket>(&serde_json::to_vec(&packet).unwrap()).unwrap()
    );
    let mut value = serde_json::to_value(result).unwrap();
    value["skip_gate"] = json!(true);
    assert!(serde_json::from_value::<WorkerResult>(value).is_err());
}

struct CliFixture {
    f: Fixture,
    base: String,
}
impl CliFixture {
    fn new() -> Self {
        let f = Fixture::new();
        git(&f.root, &["init", "-q"]);
        git(&f.root, &["config", "user.name", "Task Tests"]);
        git(&f.root, &["config", "user.email", "tasks@example.invalid"]);
        fs::create_dir_all(f.root.join("agents")).unwrap();
        fs::create_dir_all(f.root.join("stories/demo-001")).unwrap();
        fs::create_dir_all(f.root.join("app")).unwrap();
        fs::write(f.root.join(".gitignore"), ".storyflow/\n").unwrap();
        fs::write(f.root.join("app/value"), "initial").unwrap();
        for role in ["orchestrator", "planner", "implementer", "reviewer"] {
            fs::write(
                f.root.join(format!("agents/{role}.md")),
                format!("# {role}\n"),
            )
            .unwrap();
        }
        fs::write(
            f.root.join("stories/demo-001/story.md"),
            "# Task fixture\nAC1\n",
        )
        .unwrap();
        fs::write(
            f.root.join("stories/demo-001/story.json"),
            "{\"scope\":[\"app\"]}",
        )
        .unwrap();
        fs::write(f.root.join("storyflow.json"), serde_json::to_vec(&json!({"version":1,"protected":[],"checks":[{"id":"check","program":"git","args":["--version"],"timeout_seconds":10}]})).unwrap()).unwrap();
        git(&f.root, &["add", "."]);
        git(
            &f.root,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=.git/no-hooks",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        let base = git(&f.root, &["rev-parse", "HEAD"]);
        let out = cli(&f.root, false, &["init", "demo-001", "HEAD"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self { f, base }
    }
    fn graph_file(&self) -> PathBuf {
        let mut g = graph();
        g.base = self.base.clone();
        let path = self.f.root.join("graph.json");
        fs::write(&path, serde_json::to_vec(&g).unwrap()).unwrap();
        path
    }
    fn tasks(&self, args: &[&str]) -> Output {
        cli(&self.f.root, true, args)
    }
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = clean_command("git", root).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn clean_command(program: &str, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .env_remove("STORYFLOW_HOST_CONTEXT")
        .env_remove("STORYFLOW_HOST_CONTEXT_ROOT");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1").env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    command
}
fn cli(root: &Path, tasks: bool, args: &[&str]) -> Output {
    clean_command(
        if tasks {
            env!("CARGO_BIN_EXE_storyflow-tasks")
        } else {
            env!("CARGO_BIN_EXE_storyflow")
        },
        root,
    )
    .args(args)
    .output()
    .unwrap()
}
fn ok(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn rejected(out: Output, text: &str) {
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(text),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn companion_adds_authoritative_tasks_without_mutating_legacy_story_events() {
    let f = CliFixture::new();
    let event =
        f.f.root
            .join(".storyflow/demo-001/events/000000000000.json");
    let before = fs::read(&event).unwrap();
    assert_eq!(ok(f.tasks(&["view", "demo-001"]))["revision"], Value::Null);
    let graph = f.graph_file();
    ok(f.tasks(&["init", "demo-001", graph.to_str().unwrap()]));
    let packet = ok(f.tasks(&["start", "demo-001", "0", "a", "0"]));
    assert_eq!(packet["task"]["status"], "running");
    let path = f.f.root.join("result.json");
    fs::write(
        &path,
        serde_json::to_vec(&result("a", ResultStatus::Succeeded)).unwrap(),
    )
    .unwrap();
    ok(f.tasks(&["complete", "demo-001", "1", path.to_str().unwrap()]));
    assert_eq!(fs::read(&event).unwrap(), before);
    assert_eq!(
        ok(cli(&f.f.root, false, &["status", "demo-001"]))["revision"],
        0
    );
    let export = f.tasks(&["view", "demo-001", "--markdown"]);
    assert!(String::from_utf8_lossy(&export.stdout).contains("Succeeded"));
}

#[test]
fn companion_binds_story_lock_snapshot_and_live_stage_revision() {
    let f = CliFixture::new();
    let graph = f.graph_file();
    ok(f.tasks(&["init", "demo-001", graph.to_str().unwrap()]));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.f.root.join(".storyflow/demo-001/run.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    rejected(
        f.tasks(&["start", "demo-001", "0", "a", "0"]),
        "cannot lock run",
    );
    drop(lock);
    fs::write(f.f.root.join("report.md"), "AC1 planned").unwrap();
    ok(cli(
        &f.f.root,
        false,
        &["submit", "demo-001", "0", "plan", "report.md"],
    ));
    rejected(
        f.tasks(&["start", "demo-001", "0", "a", "0"]),
        "stale_story_context",
    );
    ok(f.tasks(&["view", "demo-001"]));
    fs::write(
        f.f.root.join(".storyflow/demo-001/config-snapshot.json"),
        "{}",
    )
    .unwrap();
    rejected(f.tasks(&["view", "demo-001"]), "invalid_config_snapshot");
}

#[test]
fn competing_cli_mutations_cannot_both_publish_same_revision() {
    let f = CliFixture::new();
    let graph = f.graph_file();
    ok(f.tasks(&["init", "demo-001", graph.to_str().unwrap()]));
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for task in ["a", "b"] {
        let root = f.f.root.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            cli(&root, true, &["start", "demo-001", "0", task, "0"])
        }));
    }
    let outputs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(outputs.iter().filter(|o| o.status.success()).count(), 1);
    let view = ok(f.tasks(&["view", "demo-001"]));
    assert_eq!(view["revision"], 1);
    assert_eq!(
        view["tasks"]
            .as_object()
            .unwrap()
            .values()
            .filter(|t| t["status"] == "running")
            .count(),
        1
    );
}

#[test]
fn missing_run_and_invalid_flags_are_rejected_without_initialization() {
    let f = Fixture::new();
    git(&f.root, &["init", "-q"]);
    rejected(
        cli(&f.root, true, &["view", "invented-001"]),
        "has not been initialized",
    );
    rejected(
        cli(&f.root, true, &["init", "invented-001"]),
        "storyflow-tasks init",
    );
    rejected(
        cli(&f.root, true, &["view", "invented-001", "--bad"]),
        "storyflow-tasks init",
    );
    assert!(!f.root.join(".storyflow/invented-001").exists());
}
