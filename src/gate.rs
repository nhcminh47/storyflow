use crate::{
    Result,
    model::{Check, CheckResult, Outcome, Policy, Story},
    repo,
};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn run(
    root: &Path,
    run_dir: &Path,
    policy: &Policy,
    story: &Story,
    base: &str,
    candidate: &str,
    validate_namespace: bool,
) -> Result<Vec<CheckResult>> {
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
    checkout.remove();
    Ok(results)
}

fn run_check(check: &Check, checkout: &Path, logs: &Path, attempt: &str) -> Result<CheckResult> {
    let stdout_path = logs.join(format!("{}.stdout", check.id));
    let stderr_path = logs.join(format!("{}.stderr", check.id));
    let mut command = Command::new(&check.program);
    command
        .args(&check.args)
        .current_dir(checkout)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(File::create(&stdout_path)?)
        .stderr(File::create(&stderr_path)?);
    let (outcome, exit_code, detail) = match command.spawn() {
        Err(error) => (Outcome::Error, None, error.to_string()),
        Ok(mut child) => {
            let start = Instant::now();
            let timeout = Duration::from_secs(check.timeout_seconds);
            loop {
                if let Some(status) = child.try_wait()? {
                    let outcome = if status.success() {
                        Outcome::Pass
                    } else {
                        Outcome::Fail
                    };
                    break (outcome, status.code(), status.to_string());
                }
                if start.elapsed() >= timeout {
                    if let Err(error) = child.kill()
                        && child.try_wait()?.is_none()
                    {
                        return Err(error.into());
                    }
                    child.wait()?;
                    break (
                        Outcome::Timeout,
                        None,
                        format!("exceeded {}s", check.timeout_seconds),
                    );
                }
                thread::sleep(Duration::from_millis(50));
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
        repo::checked(
            repo::git(root)
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&path)
                .arg(commit)
                .output()?,
        )?;
        Ok(Self {
            root,
            path,
            active: true,
        })
    }

    fn remove(mut self) {
        self.active = false;
        if let Err(error) = self.cleanup() {
            eprintln!(
                "could not remove gate checkout {}: {error}",
                self.path.display()
            );
        }
    }

    fn cleanup(&self) -> Result<()> {
        repo::checked(
            repo::git(self.root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path)
                .output()?,
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
