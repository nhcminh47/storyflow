use crate::{
    Result,
    model::{Policy, Story},
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub const POLICY_PATH: &str = "storyflow.json";
pub use storyflow::config::BUILTIN_PROTECTED as RESERVED;

pub fn root() -> Result<PathBuf> {
    let output = checked(
        git(&std::env::current_dir()?)
            .args(["rev-parse", "--show-toplevel"])
            .output()?,
    )?;
    Ok(PathBuf::from(
        String::from_utf8(output)?.trim_end_matches(['\r', '\n']),
    ))
}

pub fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command.current_dir(root).env("GIT_NO_REPLACE_OBJECTS", "1");
    if cfg!(windows) {
        command.args(["-c", "core.longpaths=true"]);
    }
    command
}

pub fn checked(output: Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        return Err(format!(
            "git failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(output.stdout)
}

pub fn is_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn resolve(root: &Path, rev: &str) -> Result<String> {
    let output = checked(
        git(root)
            .args([
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{rev}^{{commit}}"),
            ])
            .output()?,
    )?;
    let commit = String::from_utf8(output)?.trim().to_owned();
    if !is_commit_id(&commit) {
        return Err("invalid commit id returned by Git".into());
    }
    Ok(commit)
}

pub fn assert_descendant(root: &Path, base: &str, candidate: &str) -> Result<()> {
    let status = git(root)
        .args(["merge-base", "--is-ancestor", base, candidate])
        .status()?;
    if !status.success() || base == candidate {
        return Err("candidate must be a commit after the pinned base".into());
    }
    Ok(())
}

pub fn pin(root: &Path, id: &str, revision: usize, commit: &str) -> Result<()> {
    let worktree = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    checked(
        git(root)
            .args([
                "update-ref",
                &format!("refs/storyflow/{worktree}/{id}/rev-{revision}"),
                commit,
            ])
            .output()?,
    )?;
    Ok(())
}

pub fn read(root: &Path, commit: &str, path: &str, limit: usize) -> Result<String> {
    let bytes = checked(
        git(root)
            .args(["show", &format!("{commit}:{path}")])
            .output()?,
    )?;
    let content = String::from_utf8(bytes)?;
    if content.trim().is_empty() || content.len() > limit {
        return Err(format!("{path} must contain between 1 and {limit} UTF-8 bytes").into());
    }
    Ok(content)
}

pub fn policy(root: &Path, base: &str) -> Result<Policy> {
    let policy: Policy = serde_json::from_str(&read(root, base, POLICY_PATH, 16_384)?)?;
    if policy.version != 1 || policy.checks.is_empty() {
        return Err("policy must use version 1 and define at least one check".into());
    }
    validate_paths(&policy.protected)?;
    let mut ids = std::collections::HashSet::new();
    for check in &policy.checks {
        if check.id.eq_ignore_ascii_case("scope")
            || check.id.is_empty()
            || check.id.len() > 40
            || !check
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            || !ids.insert(check.id.to_ascii_lowercase())
            || check.program.trim().is_empty()
            || !(1..=3600).contains(&check.timeout_seconds)
        {
            return Err(format!("invalid or duplicate check: {}", check.id).into());
        }
    }
    Ok(policy)
}

pub fn story(root: &Path, base: &str, id: &str) -> Result<Story> {
    let story: Story = serde_json::from_str(&read(
        root,
        base,
        &format!("stories/{id}/story.json"),
        8192,
    )?)?;
    if story.scope.is_empty() {
        return Err("story scope is empty".into());
    }
    validate_paths(&story.scope)?;
    Ok(story)
}

pub fn spec(root: &Path, base: &str, id: &str) -> Result<String> {
    read(root, base, &format!("stories/{id}/story.md"), 8192)
}

fn validate_paths(paths: &[String]) -> Result<()> {
    for path in paths {
        if path.contains(['\\', ':'])
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|component| matches!(component, "" | "." | ".."))
        {
            return Err(format!("invalid repository-relative path: {path:?}").into());
        }
    }
    Ok(())
}

pub fn changed_paths(root: &Path, base: &str, candidate: &str) -> Result<Vec<String>> {
    let bytes = checked(
        git(root)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--ignore-submodules=none",
                "--name-only",
                "-z",
                base,
                candidate,
                "--",
            ])
            .output()?,
    )?;
    Ok(String::from_utf8(bytes)?
        .split_terminator('\0')
        .map(str::to_owned)
        .collect())
}

/// Validate both trees as one portable namespace before a candidate checkout exists.
pub fn validate_candidate_namespace(root: &Path, base: &str, candidate: &str) -> Result<()> {
    use std::collections::BTreeMap;
    use storyflow::config::{Origin, Scope, validate_relative_path};
    let mut spellings = BTreeMap::<String, String>::new();
    for commit in [base, candidate] {
        let bytes = checked(
            git(root)
                .args(["ls-tree", "-r", "-z", "--name-only", commit])
                .output()?,
        )?;
        let names =
            String::from_utf8(bytes).map_err(|_| "invalid_path: non-UTF-8 candidate namespace")?;
        for path in names.split_terminator('\0') {
            let at = Origin::new(Scope::Repo, commit, "candidate_path");
            validate_relative_path(path, &at)?;
            let parts: Vec<_> = path.split('/').collect();
            for end in 1..=parts.len() {
                let prefix = parts[..end].join("/");
                let key = portable_identity(&prefix);
                if let Some(previous) = spellings.insert(key, prefix.clone())
                    && previous != prefix
                {
                    return Err(
                        "invalid_path: candidate aliases a baseline or candidate namespace entry"
                            .into(),
                    );
                }
            }
        }
    }
    Ok(())
}

pub fn portable_identity(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    path.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfc()
        .collect()
}

pub fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}
