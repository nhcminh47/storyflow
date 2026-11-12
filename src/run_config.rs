//! CLI admission. External scopes require explicit invoking-host inputs, never repo trust.
use crate::Result;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use storyflow::{
    config::{GitBaseline, HostTrust, ResolveInput, UserLocation},
    snapshot::RunSnapshot,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostContext {
    version: u32,
    user_base: PathBuf,
    trusted_user_base: PathBuf,
    user_directory: String,
    trusted_workspace_roots: BTreeSet<PathBuf>,
    workspace_selection: Option<PathBuf>,
}

pub fn capabilities() -> BTreeSet<String> {
    ["read", "write", "git", "shell"]
        .map(str::to_owned)
        .into_iter()
        .collect()
}

pub fn initialize(root: &Path, id: &str, base: &str) -> Result<RunSnapshot> {
    let repository = GitBaseline {
        root: root.into(),
        commit: base.into(),
    };
    let snapshot = if let Some(path) = std::env::var_os("STORYFLOW_HOST_CONTEXT") {
        let mut bytes = Vec::new();
        File::open(path)?.take(65_537).read_to_end(&mut bytes)?;
        if bytes.len() > 65_536 {
            return Err("host context exceeds 64 KiB".into());
        }
        let context: HostContext = serde_json::from_slice(&bytes)
            .map_err(|_| "invalid_host_context: expected versioned host trust inputs")?;
        if context.version != 1 {
            return Err("unsupported_host_context_version".into());
        }
        // Canonical identities must be provided by the invoking host. Documents cannot add trust.
        let input = ResolveInput {
            repository,
            user_location: UserLocation {
                base: context.user_base.clone(),
                directory: context.user_directory,
            },
            trust: HostTrust {
                user_base: context.trusted_user_base,
                workspace_roots: context.trusted_workspace_roots,
            },
            workspace_selection: context.workspace_selection,
            supported_capabilities: capabilities(),
            granted_capabilities: capabilities(),
        };
        RunSnapshot::resolve(id, &input)?
    } else {
        RunSnapshot::repository(id, &repository, &capabilities())?
    };
    snapshot.require_cli_capabilities(&capabilities())?;
    Ok(snapshot)
}
