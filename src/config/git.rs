use super::*;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// An exact approved commit, never a moving revision or working-tree document.
#[derive(Clone, Debug)]
pub struct GitBaseline {
    pub root: PathBuf,
    pub commit: String,
}

pub(crate) fn command(root: &Path) -> Command {
    let mut command = Command::new(crate::security::process::git_executable());
    crate::security::process::configure_environment(&mut command);
    command
        .args(["--no-replace-objects", "--no-lazy-fetch", "-C"])
        .arg(root);
    // A host's unrelated Git environment must not redirect authoritative reads.
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    command
}
fn output(root: &Path, args: &[&str], origin: &Origin) -> ConfigResult<Vec<u8>> {
    let result = command(root)
        .args(args)
        .output()
        .map_err(|_| Diagnostic::new("git_unavailable", origin, "Git could not be started"))?;
    if !result.status.success() {
        return Err(Diagnostic::new(
            "git_lookup_failed",
            origin,
            "authoritative Git lookup failed",
        ));
    }
    Ok(result.stdout)
}

fn bounded_output(
    root: &Path,
    args: &[&str],
    origin: &Origin,
    limit: usize,
) -> ConfigResult<Vec<u8>> {
    let mut child = command(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Diagnostic::new("git_unavailable", origin, "Git could not be started"))?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped Git stdout")
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Diagnostic::new(
            if read.is_err() {
                "artifact_read_failed"
            } else {
                "artifact_too_large"
            },
            origin,
            "bounded Git artifact read failed or exceeded byte limit",
        ));
    }
    let status = child
        .wait()
        .map_err(|_| Diagnostic::new("git_lookup_failed", origin, "cannot collect Git result"))?;
    if !status.success() {
        return Err(Diagnostic::new(
            "git_lookup_failed",
            origin,
            "authoritative Git lookup failed",
        ));
    }
    Ok(bytes)
}
pub(crate) fn top_level(root: &Path, origin: &Origin) -> ConfigResult<PathBuf> {
    let bytes = output(root, &["rev-parse", "--show-toplevel"], origin)?;
    let path = std::str::from_utf8(&bytes)
        .map_err(|_| Diagnostic::new("invalid_utf8", origin, "Git top-level is not UTF-8"))?;
    std::fs::canonicalize(path.trim_end_matches(['\r', '\n']))
        .map_err(|_| Diagnostic::new("invalid_repository", origin, "Git top-level unavailable"))
}
impl GitBaseline {
    pub(crate) fn validate(&self) -> ConfigResult<()> {
        let origin = Origin::new(Scope::Repo, self.root.to_string_lossy(), "baseline");
        if !matches!(self.commit.len(), 40 | 64)
            || !self.commit.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Diagnostic::new(
                "invalid_baseline",
                &origin,
                "an exact approved commit ID is required",
            ));
        }
        let kind = output(&self.root, &["cat-file", "-t", &self.commit], &origin)?;
        if kind != b"commit\n" {
            return Err(Diagnostic::new(
                "invalid_baseline",
                &origin,
                "approved baseline must be a commit",
            ));
        }
        Ok(())
    }
    pub fn read(&self, path: &str, origin: &Origin) -> ConfigResult<Vec<u8>> {
        self.read_with_limit(path, origin, None)
    }
    pub fn read_bounded(&self, path: &str, origin: &Origin, limit: usize) -> ConfigResult<Vec<u8>> {
        self.read_with_limit(path, origin, Some(limit))
    }
    fn read_with_limit(
        &self,
        path: &str,
        origin: &Origin,
        limit: Option<usize>,
    ) -> ConfigResult<Vec<u8>> {
        validate_relative_path(path, origin)?;
        self.validate()?;
        let tree = output(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{tree}}", self.commit),
            ],
            origin,
        )?;
        let mut object = String::from_utf8(tree)
            .map_err(|_| Diagnostic::new("invalid_utf8", origin, "invalid Git object ID"))?
            .trim()
            .to_owned();
        let parts: Vec<_> = path.split('/').collect();
        for (index, part) in parts.iter().enumerate() {
            // No pathspec lookup: enumerate this tree and compare raw names exactly.
            let entries = output(&self.root, &["ls-tree", "-z", &object], origin)?;
            let mut selected = None;
            for entry in entries.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                let tab = entry.iter().position(|b| *b == b'\t').ok_or_else(|| {
                    Diagnostic::new("invalid_tree_entry", origin, "malformed Git tree entry")
                })?;
                let name = std::str::from_utf8(&entry[tab + 1..]).map_err(|_| {
                    Diagnostic::new("invalid_utf8", origin, "tree path is not UTF-8")
                })?;
                if name == *part {
                    if selected.is_some() {
                        return Err(Diagnostic::new(
                            "invalid_tree_entry",
                            origin,
                            "ambiguous duplicate Git tree entry",
                        ));
                    }
                    let header = std::str::from_utf8(&entry[..tab]).map_err(|_| {
                        Diagnostic::new("invalid_tree_entry", origin, "invalid Git header")
                    })?;
                    let fields: Vec<_> = header.split(' ').collect();
                    if fields.len() != 3 {
                        return Err(Diagnostic::new(
                            "invalid_tree_entry",
                            origin,
                            "malformed tree entry",
                        ));
                    }
                    selected = Some((
                        fields[0].to_owned(),
                        fields[1].to_owned(),
                        fields[2].to_owned(),
                    ));
                }
            }
            let Some((mode, kind, id)) = selected else {
                return Err(Diagnostic::new(
                    if path == "storyflow.json" {
                        "missing_repo_config"
                    } else {
                        "missing_artifact"
                    },
                    origin,
                    "required pinned path is absent",
                ));
            };
            let leaf = index == parts.len() - 1;
            if (leaf && (!(mode == "100644" || mode == "100755") || kind != "blob"))
                || (!leaf && (mode != "040000" || kind != "tree"))
            {
                return Err(Diagnostic::new(
                    "invalid_tree_entry",
                    origin,
                    "expected trees and a regular blob leaf",
                ));
            }
            let actual = output(&self.root, &["cat-file", "-t", &id], origin)?;
            if actual != format!("{kind}\n").as_bytes() {
                return Err(Diagnostic::new(
                    "invalid_tree_entry",
                    origin,
                    "Git object type mismatch",
                ));
            }
            object = id;
        }
        let args = ["cat-file", "blob", &object];
        let bytes = if let Some(limit) = limit {
            bounded_output(&self.root, &args, origin, limit)?
        } else {
            output(&self.root, &args, origin)?
        };
        std::str::from_utf8(&bytes)
            .map_err(|_| Diagnostic::new("invalid_utf8", origin, "pinned artifact is not UTF-8"))?;
        Ok(bytes)
    }
}
