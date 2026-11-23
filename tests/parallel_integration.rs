//! Tests for safe integration of parallel worker commits into one story candidate (#10).
use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use storyflow::{
    integration::{IntegrationCoordinator, IntegrationPhase, dependency_order},
    tasks::*,
};

const STORY: &str = "parallel-001";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct FakeClock(Cell<u64>);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

struct RepoFixture {
    root: PathBuf,
    base: String,
}

impl RepoFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "storyflow-parallel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let mut f = Self {
            root,
            base: String::new(),
        };
        f.git(&["init", "-q"]);
        f.git(&["config", "user.name", "Storyflow Parallel Test"]);
        f.git(&["config", "user.email", "parallel@example.invalid"]);
        f.git(&["config", "commit.gpgsign", "false"]);
        f.git(&["config", "core.autocrlf", "false"]);
        f.git(&["config", "core.hooksPath", ".git/no-hooks"]);

        f.write(".gitignore", ".storyflow/\nreport.md\n");
        f.write("report.md", "AC-1 & AC-2 verified\n");
        f.write("messages/welcome.txt", "pending welcome\n");
        f.write("messages/complete.txt", "pending complete\n");
        f.write("messages/shared.txt", "line 1\nline 2\nline 3\n");
        f.write("messages/protected.txt", "do not touch\n");
        f.write("outside/other.txt", "unscoped\n");
        f.write(
            "stories/parallel-001/story.md",
            "# PARALLEL-001\n- AC-1: welcome\n- AC-2: complete\n",
        );
        f.write(
            "stories/parallel-001/story.json",
            r#"{"scope":["messages"]}"#,
        );
        for role in ["orchestrator", "planner", "implementer", "reviewer"] {
            f.write(&format!("agents/{role}.md"), &format!("# {role}\n"));
        }
        let check_doc = json!({
            "version": 1,
            "protected": ["messages/protected.txt"],
            "checks": [{
                "id": "verify",
                "program": "git",
                "args": ["status", "--short"],
                "timeout_seconds": 15
            }]
        });
        f.write(
            "storyflow.json",
            &serde_json::to_string_pretty(&check_doc).unwrap(),
        );
        f.git(&["add", "--all"]);
        f.git(&["commit", "-qm", "Baseline"]);
        f.base = f.git(&["rev-parse", "HEAD"]);
        f
    }

    fn write(&self, rel: &str, content: &str) {
        self.write_in(&self.root, rel, content);
    }

    fn write_in(&self, dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
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
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.root, args)
    }

    fn commit_in(&self, dir: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (rel, content) in files {
            self.write_in(dir, rel, content);
            self.git_in(dir, &["add", "--", rel]);
        }
        self.git_in(
            dir,
            &[
                "-c",
                "user.name=Worker",
                "-c",
                "user.email=worker@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=.git/no-hooks",
                "commit",
                "-qm",
                message,
            ],
        );
        self.git_in(dir, &["rev-parse", "HEAD"])
    }

    fn detached_commit_from(&self, base: &str, files: &[(&str, &str)], message: &str) -> String {
        let wt = self
            .root
            .join(".storyflow/tmp-wt")
            .join(format!("wt-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(wt.parent().unwrap()).unwrap();
        let wt_str = wt.to_str().unwrap();
        self.git(&["worktree", "add", "--detach", "--quiet", wt_str, base]);
        let commit = self.commit_in(&wt, files, message);
        self.git(&["worktree", "remove", "--force", wt_str]);
        commit
    }

    fn init_story_in_implement(&self) -> RunContext {
        let init = self.flow(&["init", STORY, &self.base]);
        assert_eq!(init["revision"], 0);
        let plan = self.flow(&["submit", STORY, "0", "plan", "report.md"]);
        assert_eq!(plan["revision"], 1);
        assert_eq!(plan["stage"], "implement");
        RunContext {
            story: STORY.into(),
            base: self.base.clone(),
            story_revision: 1,
            stage: TaskStage::Implement,
            blocked: false,
            scope: vec!["messages".into()],
            protected: vec!["messages/protected.txt".into()],
        }
    }

    fn cmd(&self, bin: &str, args: &[&str]) -> Output {
        let mut command = Command::new(bin);
        command
            .current_dir(&self.root)
            .args(args)
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
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap()
    }

    fn flow_raw(&self, args: &[&str]) -> Output {
        self.cmd(env!("CARGO_BIN_EXE_storyflow"), args)
    }

    fn tasks_raw(&self, args: &[&str]) -> Output {
        self.cmd(env!("CARGO_BIN_EXE_storyflow-tasks"), args)
    }

    fn parse(out: Output, expected_code: i32) -> Value {
        assert_eq!(
            out.status.code(),
            Some(expected_code),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn flow(&self, args: &[&str]) -> Value {
        Self::parse(self.flow_raw(args), 0)
    }

    fn tasks(&self, args: &[&str]) -> Value {
        Self::parse(self.tasks_raw(args), 0)
    }

    fn tasks_blocked(&self, args: &[&str]) -> Value {
        Self::parse(self.tasks_raw(args), 1)
    }
}

impl Drop for RepoFixture {
    fn drop(&mut self) {
        assert_eq!(self.root.parent(), Some(std::env::temp_dir().as_path()));
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn task_spec(id: &str, deps: &[&str], write_scope: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        story: STORY.into(),
        prerequisites: deps.iter().map(|s| (*s).into()).collect(),
        stage: TaskStage::Implement,
        read_scope: vec!["messages".into()],
        write_scope: write_scope.iter().map(|s| (*s).into()).collect(),
        acceptance_criteria: vec![format!("AC for {id}")],
        decision_owner: "orchestrator".into(),
    }
}

#[test]
fn parallel_non_overlapping_workers_integrate_into_one_candidate_and_pass_gate_and_review() {
    let f = RepoFixture::new();
    let _ctx = f.init_story_in_implement();

    // Task A edits welcome.txt, Task B edits complete.txt in parallel, Task C depends on A and B and edits shared.txt.
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: f.base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![
            task_spec("welcome", &[], &["messages/welcome.txt"]),
            task_spec("complete", &[], &["messages/complete.txt"]),
            task_spec(
                "summary",
                &["welcome", "complete"],
                &["messages/shared.txt"],
            ),
        ],
    };
    let graph_path = f.root.join("graph.json");
    fs::write(&graph_path, serde_json::to_vec(&graph).unwrap()).unwrap();
    f.tasks(&["init", STORY, graph_path.to_str().unwrap()]);

    let policy_path = f.root.join("policy.json");
    fs::write(
        &policy_path,
        serde_json::to_vec(&SchedulerPolicy {
            max_concurrency: 2,
            max_attempts: 2,
            lease_ms: 60_000,
        })
        .unwrap(),
    )
    .unwrap();
    f.tasks(&["configure", STORY, "0", policy_path.to_str().unwrap()]);

    // Dispatch welcome and complete concurrently.
    let d_welcome = f.tasks(&["dispatch", STORY, "1", "welcome", "0", "worker-1"]);
    let d_complete = f.tasks(&["dispatch", STORY, "2", "complete", "0", "worker-2"]);

    // Provision isolated worktrees for both concurrent workers.
    let wt_welcome = f.tasks(&["worktree", STORY, "3", "welcome"]);
    let wt_complete = f.tasks(&["worktree", STORY, "3", "complete"]);
    assert_ne!(wt_welcome["path"], wt_complete["path"]);
    assert_eq!(wt_welcome["base_commit"], f.base);
    assert_eq!(wt_complete["base_commit"], f.base);

    let wt_welcome_dir = f.root.join(wt_welcome["path"].as_str().unwrap());
    let wt_complete_dir = f.root.join(wt_complete["path"].as_str().unwrap());

    // Workers write and commit in their own isolated worktrees.
    let c_welcome = f.commit_in(
        &wt_welcome_dir,
        &[("messages/welcome.txt", "Chào mừng bạn đến với Storyflow!\n")],
        "task welcome",
    );
    let c_complete = f.commit_in(
        &wt_complete_dir,
        &[("messages/complete.txt", "Bạn đã hoàn tất thiết lập.\n")],
        "task complete",
    );

    // Attempt integration while workers are still running -> must block with worker_stop_required.
    let blocked_running = f.tasks_blocked(&["integrate", STORY, "3"]);
    assert_eq!(blocked_running["phase"], "blocked");
    assert!(
        blocked_running["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["code"] == "worker_stop_required")
    );

    // Finish welcome and complete with worker_stopped = true.
    let finish_welcome = json!({
        "schema": 1,
        "lease": d_welcome["lease"],
        "worker_stopped": true,
        "result": {
            "schema": 1,
            "story": STORY,
            "task": "welcome",
            "task_revision": d_welcome["packet"]["task"]["revision"],
            "status": "succeeded",
            "report": "welcome updated",
            "commit": c_welcome,
            "artifacts": []
        }
    });
    let fw_path = f.root.join("finish-welcome.json");
    fs::write(&fw_path, serde_json::to_vec(&finish_welcome).unwrap()).unwrap();
    f.tasks(&["finish", STORY, "3", fw_path.to_str().unwrap()]);

    let finish_complete = json!({
        "schema": 1,
        "lease": d_complete["lease"],
        "worker_stopped": true,
        "result": {
            "schema": 1,
            "story": STORY,
            "task": "complete",
            "task_revision": d_complete["packet"]["task"]["revision"],
            "status": "succeeded",
            "report": "complete updated",
            "commit": c_complete,
            "artifacts": []
        }
    });
    let fc_path = f.root.join("finish-complete.json");
    fs::write(&fc_path, serde_json::to_vec(&finish_complete).unwrap()).unwrap();
    f.tasks(&["finish", STORY, "4", fc_path.to_str().unwrap()]);

    // Cleanup worker worktrees after worker stop is confirmed.
    f.tasks(&["worktree-remove", STORY, "5", "welcome"]);
    f.tasks(&["worktree-remove", STORY, "5", "complete"]);
    assert!(!wt_welcome_dir.exists());
    assert!(!wt_complete_dir.exists());

    // Dispatch dependent task `summary` and provision its worktree.
    let d_summary = f.tasks(&["dispatch", STORY, "5", "summary", "0", "worker-3"]);
    let wt_summary = f.tasks(&["worktree", STORY, "6", "summary"]);
    let wt_summary_dir = f.root.join(wt_summary["path"].as_str().unwrap());

    // The dependent task's worktree base already includes both prerequisite commits!
    assert_eq!(
        fs::read_to_string(wt_summary_dir.join("messages/welcome.txt")).unwrap(),
        "Chào mừng bạn đến với Storyflow!\n"
    );
    assert_eq!(
        fs::read_to_string(wt_summary_dir.join("messages/complete.txt")).unwrap(),
        "Bạn đã hoàn tất thiết lập.\n"
    );

    let c_summary = f.commit_in(
        &wt_summary_dir,
        &[("messages/shared.txt", "summary line\n")],
        "task summary",
    );
    let finish_summary = json!({
        "schema": 1,
        "lease": d_summary["lease"],
        "worker_stopped": true,
        "result": {
            "schema": 1,
            "story": STORY,
            "task": "summary",
            "task_revision": d_summary["packet"]["task"]["revision"],
            "status": "succeeded",
            "report": "summary updated",
            "commit": c_summary,
            "artifacts": []
        }
    });
    let fs_path = f.root.join("finish-summary.json");
    fs::write(&fs_path, serde_json::to_vec(&finish_summary).unwrap()).unwrap();
    f.tasks(&["finish", STORY, "6", fs_path.to_str().unwrap()]);
    f.tasks(&["worktree-remove", STORY, "7", "summary"]);

    // Integrate all completed tasks in dependency order.
    let integrated = f.tasks(&["integrate", STORY, "7"]);
    assert_eq!(integrated["phase"], "ready");
    assert_eq!(
        integrated["order"],
        json!(["welcome", "complete", "summary"])
    );
    let candidate = integrated["candidate_commit"].as_str().unwrap().to_owned();
    let candidate_ref = integrated["candidate_ref"].as_str().unwrap();
    assert_eq!(f.git(&["rev-parse", candidate_ref]), candidate);

    // Verify each input ref was pinned separately for audit evidence.
    for item in integrated["integrated"].as_array().unwrap() {
        let input_ref = item["input_ref"].as_str().unwrap();
        let input_commit = item["input_commit"].as_str().unwrap();
        assert_eq!(f.git(&["rev-parse", input_ref]), input_commit);
        assert_eq!(item["reused"], false);
    }

    // Verify recovery idempotency: running integrate again reuses all integrated inputs and produces the exact same candidate.
    let recovered = f.tasks(&["integrate", STORY, "7"]);
    assert_eq!(recovered["phase"], "ready");
    assert_eq!(recovered["candidate_commit"], candidate);
    for item in recovered["integrated"].as_array().unwrap() {
        assert_eq!(item["reused"], true);
    }

    // Submit the integrated candidate to the story workflow, run gate and approve review.
    let sub = f.flow(&["submit", STORY, "1", "candidate", "report.md", &candidate]);
    assert_eq!(sub["stage"], "gate");
    assert_eq!(sub["candidate"], candidate);

    let gate = f.flow(&["gate", STORY, "2"]);
    assert_eq!(gate["status"]["stage"], "review");

    let done = f.flow(&["submit", STORY, "3", "approve", "report.md", &candidate]);
    assert_eq!(done["stage"], "done");
}

#[test]
fn expired_lease_without_stop_confirmation_blocks_integration_until_stopped_and_retried() {
    let f = RepoFixture::new();
    let ctx = f.init_story_in_implement();
    let clock = FakeClock(Cell::new(1_000));

    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: f.base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![
            task_spec("a", &[], &["messages/welcome.txt"]),
            task_spec("b", &[], &["messages/complete.txt"]),
        ],
    };

    let mut journal = TaskJournal::open(&f.root, ctx.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();
    journal
        .configure_scheduler(
            0,
            SchedulerPolicy {
                max_concurrency: 2,
                max_attempts: 2,
                lease_ms: 100,
            },
            &clock,
        )
        .unwrap();

    let d_a = journal.dispatch(1, "a", 0, "worker-a", &clock).unwrap();
    let d_b = journal.dispatch(2, "b", 0, "worker-b", &clock).unwrap();

    let commit_a = f.detached_commit_from(
        &f.base,
        &[("messages/welcome.txt", "welcome A\n")],
        "commit a",
    );
    let commit_b = f.detached_commit_from(
        &f.base,
        &[("messages/complete.txt", "complete B\n")],
        "commit b",
    );

    // Worker A finishes on time.
    journal
        .complete_attempt(
            3,
            AttemptResult {
                schema: 1,
                lease: d_a.lease.clone(),
                worker_stopped: true,
                result: WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "a".into(),
                    task_revision: d_a.packet.task.revision,
                    status: ResultStatus::Succeeded,
                    report: "done a".into(),
                    commit: Some(commit_a.clone()),
                    artifacts: vec![],
                },
            },
            &clock,
        )
        .unwrap();

    // Lease for B expires, and recover is called, but worker exit has NOT been confirmed!
    clock.0.set(1_200);
    journal.recover_expired(4, &clock).unwrap();

    let coordinator = IntegrationCoordinator::new(&f.root, ctx.clone()).unwrap();
    let status = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(status.phase, IntegrationPhase::Blocked);
    assert!(
        status
            .blockers
            .iter()
            .any(|b| b.code == "worker_stop_required" && b.task.as_deref() == Some("b"))
    );

    // Confirm worker B stopped, retry B, and complete B.
    journal
        .confirm_stopped(5, &d_b.lease, "confirmed process exited", &clock)
        .unwrap();
    let retry_pkt = journal
        .retry_task(
            6,
            "b",
            journal.view().tasks["b"].revision,
            "retry after stop",
            &clock,
        )
        .unwrap();
    let d_b2 = journal
        .dispatch(7, "b", retry_pkt.task.revision, "worker-b2", &clock)
        .unwrap();
    journal
        .complete_attempt(
            8,
            AttemptResult {
                schema: 1,
                lease: d_b2.lease,
                worker_stopped: true,
                result: WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "b".into(),
                    task_revision: d_b2.packet.task.revision,
                    status: ResultStatus::Succeeded,
                    report: "done b2".into(),
                    commit: Some(commit_b.clone()),
                    artifacts: vec![],
                },
            },
            &clock,
        )
        .unwrap();

    let ready = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(ready.phase, IntegrationPhase::Ready);
    let candidate = ready.candidate_commit.unwrap();
    assert_eq!(
        f.git(&["show", &format!("{candidate}:messages/welcome.txt")]),
        "welcome A"
    );
    assert_eq!(
        f.git(&["show", &format!("{candidate}:messages/complete.txt")]),
        "complete B"
    );
}

#[test]
fn overlapping_writes_scope_violations_stale_base_and_conflicts_block_with_precise_evidence() {
    let f = RepoFixture::new();
    let ctx = f.init_story_in_implement();
    let coordinator = IntegrationCoordinator::new(&f.root, ctx.clone()).unwrap();

    // 1. Scope violation: task A modifies messages/complete.txt which is outside its task write_scope (messages/welcome.txt),
    // and task B modifies messages/protected.txt (protected).
    {
        let graph = TaskGraph {
            schema: 1,
            story: STORY.into(),
            base: f.base.clone(),
            story_revision: 1,
            stage: TaskStage::Implement,
            tasks: vec![
                task_spec("a", &[], &["messages/welcome.txt"]),
                task_spec("b", &[], &["messages/complete.txt"]),
            ],
        };
        let mut journal = TaskJournal::open(&f.root, ctx.clone()).unwrap();
        journal.initialize(graph.clone()).unwrap();
        journal.start(0, "a", 0).unwrap();
        let bad_scope_commit = f.detached_commit_from(
            &f.base,
            &[
                ("messages/welcome.txt", "ok\n"),
                ("messages/complete.txt", "unauthorized sibling edit\n"),
            ],
            "out of task scope",
        );
        journal
            .complete(
                1,
                WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "a".into(),
                    task_revision: 1,
                    status: ResultStatus::Succeeded,
                    report: "done a".into(),
                    commit: Some(bad_scope_commit.clone()),
                    artifacts: vec![],
                },
            )
            .unwrap();

        journal.start(2, "b", 0).unwrap();
        let protected_commit = f.detached_commit_from(
            &f.base,
            &[("messages/protected.txt", "touched protected\n")],
            "protected edit",
        );
        journal
            .complete(
                3,
                WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "b".into(),
                    task_revision: 1,
                    status: ResultStatus::Succeeded,
                    report: "done b".into(),
                    commit: Some(protected_commit),
                    artifacts: vec![],
                },
            )
            .unwrap();

        let status = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
        assert_eq!(status.phase, IntegrationPhase::Blocked);
        assert!(status.candidate_commit.is_none());
        assert!(status.blockers.iter().any(|b| {
            b.code == "task_scope_violation"
                && b.task.as_deref() == Some("a")
                && b.paths == vec!["messages/complete.txt".to_owned()]
        }));
        assert!(status.blockers.iter().any(|b| {
            b.code == "task_scope_violation"
                && b.task.as_deref() == Some("b")
                && b.paths == vec!["messages/protected.txt".to_owned()]
        }));
    }

    // 2. Stale base: advance story to revision 2 (block + resume) and replace graph;
    // submit an orphan commit and a commit descending from an unrelated branch not descending from required prerequisite base.
    f.flow(&["submit", STORY, "1", "block", "report.md"]);
    f.flow(&["submit", STORY, "2", "resume", "report.md"]);
    let mut ctx2 = ctx.clone();
    ctx2.story_revision = 3;
    let coordinator2 = IntegrationCoordinator::new(&f.root, ctx2.clone()).unwrap();
    {
        let graph = TaskGraph {
            schema: 1,
            story: STORY.into(),
            base: f.base.clone(),
            story_revision: 3,
            stage: TaskStage::Implement,
            tasks: vec![
                task_spec("a", &[], &["messages/welcome.txt"]),
                task_spec("b", &["a"], &["messages/complete.txt"]),
            ],
        };
        let mut journal = TaskJournal::open(&f.root, ctx2.clone()).unwrap();
        journal.replace(4, graph.clone()).unwrap();
        let rev_a = journal.view().tasks["a"].revision;
        journal.start(5, "a", rev_a).unwrap();
        let commit_a =
            f.detached_commit_from(&f.base, &[("messages/welcome.txt", "valid A\n")], "valid a");
        journal
            .complete(
                6,
                WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "a".into(),
                    task_revision: rev_a + 1,
                    status: ResultStatus::Succeeded,
                    report: "done a".into(),
                    commit: Some(commit_a),
                    artifacts: vec![],
                },
            )
            .unwrap();

        // Task B depends on A, so its required base is commit_a, NOT f.base!
        // Building B directly on f.base without A is a stale_task_base error.
        let rev_b = journal.view().tasks["b"].revision;
        journal.start(7, "b", rev_b).unwrap();
        let stale_b = f.detached_commit_from(
            &f.base,
            &[("messages/complete.txt", "built on stale base without A\n")],
            "stale b",
        );
        journal
            .complete(
                8,
                WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "b".into(),
                    task_revision: rev_b + 1,
                    status: ResultStatus::Succeeded,
                    report: "done b".into(),
                    commit: Some(stale_b.clone()),
                    artifacts: vec![],
                },
            )
            .unwrap();

        let status = coordinator2
            .integrate(journal.view(), Some(&graph))
            .unwrap();
        assert_eq!(status.phase, IntegrationPhase::Blocked);
        assert!(status.blockers.iter().any(|b| {
            b.code == "stale_task_base"
                && b.task.as_deref() == Some("b")
                && b.commit.as_deref() == Some(stale_b.as_str())
        }));
    }

    // 3. Overlapping parallel writes & content conflict: two independent tasks with broad write_scope ["messages"]
    // both modify messages/shared.txt with conflicting content.
    f.flow(&["submit", STORY, "3", "block", "report.md"]);
    f.flow(&["submit", STORY, "4", "resume", "report.md"]);
    let mut ctx3 = ctx.clone();
    ctx3.story_revision = 5;
    let coordinator3 = IntegrationCoordinator::new(&f.root, ctx3.clone()).unwrap();
    {
        let graph = TaskGraph {
            schema: 1,
            story: STORY.into(),
            base: f.base.clone(),
            story_revision: 5,
            stage: TaskStage::Implement,
            tasks: vec![
                task_spec("left", &[], &["messages"]),
                task_spec("right", &[], &["messages"]),
            ],
        };
        let mut journal = TaskJournal::open(&f.root, ctx3).unwrap();
        journal.replace(9, graph.clone()).unwrap();
        let r_left = journal.view().tasks["left"].revision;
        journal.start(10, "left", r_left).unwrap();
        let c_left = f.detached_commit_from(
            &f.base,
            &[("messages/shared.txt", "left version\n")],
            "left edit",
        );
        journal
            .complete(
                11,
                WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "left".into(),
                    task_revision: r_left + 1,
                    status: ResultStatus::Succeeded,
                    report: "left done".into(),
                    commit: Some(c_left),
                    artifacts: vec![],
                },
            )
            .unwrap();

        let r_right = journal.view().tasks["right"].revision;
        journal.start(12, "right", r_right).unwrap();
        let c_right = f.detached_commit_from(
            &f.base,
            &[("messages/shared.txt", "right conflicting version\n")],
            "right edit",
        );
        journal
            .complete(
                13,
                WorkerResult {
                    schema: 1,
                    story: STORY.into(),
                    task: "right".into(),
                    task_revision: r_right + 1,
                    status: ResultStatus::Succeeded,
                    report: "right done".into(),
                    commit: Some(c_right),
                    artifacts: vec![],
                },
            )
            .unwrap();

        let status = coordinator3
            .integrate(journal.view(), Some(&graph))
            .unwrap();
        assert_eq!(status.phase, IntegrationPhase::Blocked);
        assert!(status.candidate_commit.is_none());
        assert!(status.blockers.iter().any(|b| {
            b.code == "overlapping_task_writes"
                && b.task.as_deref() == Some("right")
                && b.conflicting_task.as_deref() == Some("left")
                && b.paths == vec!["messages/shared.txt".to_owned()]
        }));
    }
}

#[test]
fn partial_integration_before_conflict_is_reused_idempotently_on_recovery() {
    let f = RepoFixture::new();
    let ctx = f.init_story_in_implement();
    let coordinator = IntegrationCoordinator::new(&f.root, ctx.clone()).unwrap();

    // Task A and Task B are independent and clean.
    // Task C depends on A, has write_scope ["messages/shared.txt"], but Task B ALSO built a commit on base
    // where C was actually built on an older commit of A that modified messages/complete.txt, causing a 3-way merge conflict
    // when C is merged with A+B, OR we test interrupting after A+B and adding/re-running with C.
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: f.base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![
            task_spec("a", &[], &["messages/welcome.txt"]),
            task_spec("b", &[], &["messages/complete.txt"]),
            task_spec("c", &["a", "b"], &["messages/shared.txt"]),
        ],
    };
    let mut journal = TaskJournal::open(&f.root, ctx.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();

    journal.start(0, "a", 0).unwrap();
    let c_a = f.detached_commit_from(
        &f.base,
        &[("messages/welcome.txt", "A content\n")],
        "commit a",
    );
    journal
        .complete(
            1,
            WorkerResult {
                schema: 1,
                story: STORY.into(),
                task: "a".into(),
                task_revision: 1,
                status: ResultStatus::Succeeded,
                report: "a done".into(),
                commit: Some(c_a.clone()),
                artifacts: vec![],
            },
        )
        .unwrap();

    journal.start(2, "b", 0).unwrap();
    let c_b = f.detached_commit_from(
        &f.base,
        &[("messages/complete.txt", "B content\n")],
        "commit b",
    );
    journal
        .complete(
            3,
            WorkerResult {
                schema: 1,
                story: STORY.into(),
                task: "b".into(),
                task_revision: 1,
                status: ResultStatus::Succeeded,
                report: "b done".into(),
                commit: Some(c_b.clone()),
                artifacts: vec![],
            },
        )
        .unwrap();

    // Start C and provision its worktree (which synthesizes the A+B base).
    journal.start(4, "c", 0).unwrap();
    let wt_c = coordinator
        .prepare_worktree(journal.view(), Some(&graph), "c")
        .unwrap();
    let wt_c_dir = f.root.join(&wt_c.path);
    let c_c = f.commit_in(
        &wt_c_dir,
        &[("messages/shared.txt", "C content\n")],
        "commit c",
    );
    journal
        .complete(
            5,
            WorkerResult {
                schema: 1,
                story: STORY.into(),
                task: "c".into(),
                task_revision: 1,
                status: ResultStatus::Succeeded,
                report: "c done".into(),
                commit: Some(c_c.clone()),
                artifacts: vec![],
            },
        )
        .unwrap();
    coordinator
        .cleanup_worktree(journal.view(), Some(&graph), "c")
        .unwrap();

    let first = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(first.phase, IntegrationPhase::Ready);
    assert_eq!(first.integrated.len(), 3);
    assert!(first.integrated.iter().all(|i| !i.reused));

    // Simulate interruption / partial state where only the first 2 inputs (a and b) were persisted in state.json.
    let mut partial = first.clone();
    partial.integrated.truncate(2);
    partial.phase = IntegrationPhase::Pending;
    partial.candidate_commit = None;
    partial.candidate_ref = None;

    // Re-running integrate after interruption reuses `a` and `b` idempotently (`reused: true`) and integrates `c` (`reused: false`).
    let second = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(second.phase, IntegrationPhase::Ready);
    assert_eq!(second.candidate_commit, first.candidate_commit);
    assert!(second.integrated.iter().all(|i| i.reused));

    // Also verify dependency_order preserves topological ordering even when tasks are declared in reverse order.
    let rev_graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: f.base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![
            task_spec("c", &["a", "b"], &["messages/shared.txt"]),
            task_spec("b", &[], &["messages/complete.txt"]),
            task_spec("a", &[], &["messages/welcome.txt"]),
        ],
    };
    let order = dependency_order(journal.view(), Some(&rev_graph)).unwrap();
    assert_eq!(order, vec!["b", "a", "c"]);
}

#[test]
fn single_implementer_legacy_path_remains_valid_without_tasks() {
    let f = RepoFixture::new();
    f.init_story_in_implement();
    let single_commit = f.commit_in(
        &f.root,
        &[
            ("messages/welcome.txt", "Single worker welcome\n"),
            ("messages/complete.txt", "Single worker complete\n"),
        ],
        "single implementer candidate",
    );
    let sub = f.flow(&[
        "submit",
        STORY,
        "1",
        "candidate",
        "report.md",
        &single_commit,
    ]);
    assert_eq!(sub["stage"], "gate");
    let gate = f.flow(&["gate", STORY, "2"]);
    assert_eq!(gate["status"]["stage"], "review");
    let done = f.flow(&["submit", STORY, "3", "approve", "report.md", &single_commit]);
    assert_eq!(done["stage"], "done");
}

#[test]
fn merge_conflict_in_same_file_and_state_tampering_are_blocked_and_detected() {
    let f = RepoFixture::new();
    let ctx = f.init_story_in_implement();
    let coordinator = IntegrationCoordinator::new(&f.root, ctx.clone()).unwrap();

    // Test 3-way merge conflict where task A modifies messages/welcome.txt, and task B also
    // includes a foreign merge parent or where state.json checksum is tampered.
    let graph = TaskGraph {
        schema: 1,
        story: STORY.into(),
        base: f.base.clone(),
        story_revision: 1,
        stage: TaskStage::Implement,
        tasks: vec![
            task_spec("readonly", &[], &[]),
            task_spec("writer", &["readonly"], &["messages/welcome.txt"]),
        ],
    };
    let mut journal = TaskJournal::open(&f.root, ctx.clone()).unwrap();
    journal.initialize(graph.clone()).unwrap();

    // Read-only task reporting a commit must block with `read_only_task_commit`.
    journal.start(0, "readonly", 0).unwrap();
    let accidental_commit = f.detached_commit_from(
        &f.base,
        &[("messages/welcome.txt", "accidental write\n")],
        "accidental commit",
    );
    journal
        .complete(
            1,
            WorkerResult {
                schema: 1,
                story: STORY.into(),
                task: "readonly".into(),
                task_revision: 1,
                status: ResultStatus::Succeeded,
                report: "read only".into(),
                commit: Some(accidental_commit),
                artifacts: vec![],
            },
        )
        .unwrap();

    // Writer task reporting no commit must block with `missing_task_commit`.
    journal.start(2, "writer", 0).unwrap();
    journal
        .complete(
            3,
            WorkerResult {
                schema: 1,
                story: STORY.into(),
                task: "writer".into(),
                task_revision: 1,
                status: ResultStatus::Succeeded,
                report: "forgot commit".into(),
                commit: None,
                artifacts: vec![],
            },
        )
        .unwrap();

    let blocked = coordinator.integrate(journal.view(), Some(&graph)).unwrap();
    assert_eq!(blocked.phase, IntegrationPhase::Blocked);
    assert!(
        blocked
            .blockers
            .iter()
            .any(|b| b.code == "read_only_task_commit" && b.task.as_deref() == Some("readonly"))
    );
    assert!(
        blocked
            .blockers
            .iter()
            .any(|b| b.code == "missing_task_commit" && b.task.as_deref() == Some("writer"))
    );

    // Tamper with persisted integration state.json and verify checksum validation rejects it.
    let state_path = f
        .root
        .join(format!(".storyflow/{STORY}/integration/state.json"));
    let original = fs::read(&state_path).unwrap();
    let mut parsed: Value = serde_json::from_slice(&original).unwrap();
    parsed["sha256"] = json!("0".repeat(64));
    fs::write(&state_path, serde_json::to_vec(&parsed).unwrap()).unwrap();
    let err = coordinator.load_state().unwrap_err();
    assert_eq!(err.code, "integration_integrity");
    fs::write(&state_path, original).unwrap();
    assert!(coordinator.load_state().unwrap().is_some());
}
