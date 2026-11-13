use crate::{
    Result,
    model::{Check, CheckResult, Outcome, Policy, Story},
    repo,
};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub struct GateOptions {
    pub validate_namespace: bool,
    pub backend: storyflow::security::SandboxBackend,
}

pub fn run(
    root: &Path,
    run_dir: &Path,
    policy: &Policy,
    story: &Story,
    base: &str,
    candidate: &str,
    options: GateOptions,
) -> Result<Vec<CheckResult>> {
    let validate_namespace = options.validate_namespace;
    if validate_namespace
        && let Err(error) = repo::validate_candidate_namespace(root, base, candidate)
    {
        return Ok(vec![CheckResult {
            id: "scope".into(),
            outcome: Outcome::Fail,
            exit_code: None,
            detail: error.to_string(),
            log: None,
        }]);
    }
    let changed = repo::changed_paths(root, base, candidate)?;
    let denied: Vec<_> = changed
        .iter()
        .filter(|path| {
            let fold = |s: &str| {
                if validate_namespace {
                    repo::portable_identity(s)
                } else {
                    s.to_lowercase()
                }
            };
            let folded = fold(path);
            repo::RESERVED
                .iter()
                .copied()
                .chain(policy.protected.iter().map(String::as_str))
                .any(|prefix| repo::under(&folded, &fold(prefix)))
                || !story.scope.iter().any(|prefix| repo::under(path, prefix))
        })
        .collect();
    let scope_ok = denied.is_empty() && !changed.is_empty();
    let detail = if changed.is_empty() {
        "candidate has no tree changes".into()
    } else if scope_ok {
        format!("{} changed path(s) within scope", changed.len())
    } else {
        format!(
            "{} denied path(s); first entries: {:?}",
            denied.len(),
            denied.iter().take(8).collect::<Vec<_>>()
        )
    };
    let mut results = vec![CheckResult {
        id: "scope".into(),
        outcome: if scope_ok {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        exit_code: None,
        detail,
        log: None,
    }];
    if !scope_ok {
        return Ok(results);
    }
    if let Err(error) = options.backend.authorize_command() {
        results.push(CheckResult {
            id: "sandbox".into(),
            outcome: Outcome::Error,
            exit_code: None,
            detail: error.to_string(),
            log: None,
        });
        return Ok(results);
    }
    let attempt = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let logs = run_dir.join("logs").join(&attempt);
    fs::create_dir_all(&logs)?;
    let checkout = Checkout::new(root, run_dir.join("checkouts").join(&attempt), candidate)?;
    for check in &policy.checks {
        let result = run_check(check, &checkout.path, &logs, &attempt)?;
        let passed = result.passed();
        results.push(result);
        if !passed {
            break;
        }
    }
    if checkout.remove().is_err() {
        results.push(CheckResult {
            id: "cleanup".into(),
            outcome: Outcome::Error,
            exit_code: None,
            detail: "checkout_cleanup_failed: inspect retained checkout".into(),
            log: None,
        });
    }
    Ok(results)
}

fn run_check(check: &Check, checkout: &Path, logs: &Path, attempt: &str) -> Result<CheckResult> {
    let stdout_path = logs.join(format!("{}.stdout", check.id));
    let stderr_path = logs.join(format!("{}.stderr", check.id));
    let spec = storyflow::security::process::CommandSpec {
        program: check.program.clone(),
        args: check.args.clone(),
        timeout: Duration::from_secs(check.timeout_seconds),
    };
    let (outcome, exit_code, detail) = match storyflow::security::process::run(&spec, checkout) {
        Err(error) => {
            fs::write(&stdout_path, [])?;
            fs::write(&stderr_path, [])?;
            (
                Outcome::Error,
                None,
                format!("process_lifecycle_error: {:?}", error.kind()),
            )
        }
        Ok(result) => {
            fs::write(&stdout_path, result.stdout)?;
            fs::write(&stderr_path, result.stderr)?;
            let suffix = if result.truncated {
                "; output truncated"
            } else {
                ""
            };
            if result.timed_out {
                (
                    Outcome::Timeout,
                    None,
                    format!("exceeded {}s{suffix}", check.timeout_seconds),
                )
            } else if let Some(status) = result.status {
                (
                    if status.success() {
                        Outcome::Pass
                    } else {
                        Outcome::Fail
                    },
                    status.code(),
                    format!("{status}{suffix}"),
                )
            } else {
                (
                    Outcome::Error,
                    None,
                    "process_lifecycle_error: no exit status".into(),
                )
            }
        }
    };
    let detail = if matches!(outcome, Outcome::Pass) {
        detail
    } else {
        format!(
            "{detail}\n{}",
            tail(&stderr_path, 1200)? + &tail(&stdout_path, 800)?
        )
    };
    Ok(CheckResult {
        id: check.id.clone(),
        outcome,
        exit_code,
        detail,
        log: Some(format!("logs/{attempt}/{}", check.id)),
    })
}

fn tail(path: &Path, limit: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let offset = file.metadata()?.len().saturating_sub(limit);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct Checkout<'a> {
    root: &'a Path,
    path: PathBuf,
    active: bool,
}

impl<'a> Checkout<'a> {
    fn new(root: &'a Path, path: PathBuf, commit: &str) -> Result<Self> {
        fs::create_dir_all(path.parent().ok_or("invalid checkout path")?)?;
        checkout_git(
            root,
            vec![
                "worktree".into(),
                "add".into(),
                "--detach".into(),
                "--quiet".into(),
                path.to_str().ok_or("invalid checkout path")?.into(),
                commit.into(),
            ],
        )?;
        Ok(Self {
            root,
            path,
            active: true,
        })
    }

    fn remove(mut self) -> Result<()> {
        self.active = false;
        self.cleanup()
    }

    fn cleanup(&self) -> Result<()> {
        checkout_git(
            self.root,
            vec![
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                self.path.to_str().ok_or("invalid checkout path")?.into(),
            ],
        )?;
        Ok(())
    }
}

impl Drop for Checkout<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.cleanup();
        }
    }
}

// Checkout maintenance uses the same bounded process lifecycle as gate commands.
fn checkout_git(root: &Path, args: Vec<String>) -> Result<()> {
    let mut safe_args: Vec<String> = [
        "--no-replace-objects",
        "--no-lazy-fetch",
        "-c",
        if cfg!(windows) {
            "core.hooksPath=NUL"
        } else {
            "core.hooksPath=/dev/null"
        },
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.longpaths=true",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    safe_args.extend(args);
    let output = storyflow::security::process::run(
        &storyflow::security::process::CommandSpec {
            program: "git".into(),
            args: safe_args,
            timeout: Duration::from_secs(60),
        },
        root,
    )?;
    if output.timed_out || !output.status.is_some_and(|s| s.success()) {
        return Err("checkout_git_failed: bounded Git maintenance failed".into());
    }
    Ok(())
}
