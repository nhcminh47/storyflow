//! CLI admission. External scopes require explicit invoking-host inputs, never repo trust.
use crate::Result;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
use storyflow::{
    config::{GitBaseline, HostTrust, ResolveInput, UserLocation, read_host_context},
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

pub fn initialize(root: &Path, id: &str, base: &str, managed_only: bool) -> Result<RunSnapshot> {
    let repository = GitBaseline {
        root: root.into(),
        commit: base.into(),
    };
    let context_path = std::env::var_os("STORYFLOW_HOST_CONTEXT");
    let authorized_root = std::env::var_os("STORYFLOW_HOST_CONTEXT_ROOT");
    let mut snapshot = if let Some(path) = context_path {
        let authorized_root = authorized_root.ok_or("missing_host_context_root: separately authorize an external host directory using STORYFLOW_HOST_CONTEXT_ROOT")?;
        let bytes = read_host_context(Path::new(&path), Path::new(&authorized_root))?;
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
        if authorized_root.is_some() {
            return Err("missing_host_context: STORYFLOW_HOST_CONTEXT_ROOT requires an explicit manifest path".into());
        }
        RunSnapshot::repository(id, &repository, &capabilities())?
    };
    snapshot.managed_only = managed_only;
    if managed_only {
        // Built-in support is a ceiling, not an implicit managed-worker grant.
        // Require a declared allow from at least one pinned configuration scope.
        let declared: BTreeSet<String> = snapshot
            .effective
            .capabilities
            .constraints
            .iter()
            .filter(|c| c.origin.scope.is_some())
            .filter_map(|c| c.value.allow.as_ref())
            .flatten()
            .cloned()
            .collect();
        let removed: Vec<_> = snapshot
            .effective
            .capabilities
            .granted
            .difference(&declared)
            .cloned()
            .collect();
        let origin = storyflow::config::Origin {
            scope: None,
            source: "host".into(),
            field: "managed_only.deny_default".into(),
        };
        for capability in removed {
            snapshot.effective.capabilities.granted.remove(&capability);
            snapshot.effective.capabilities.provenance.insert(
                capability,
                storyflow::config::Sourced {
                    value: false,
                    origin: origin.clone(),
                },
            );
        }
        snapshot
            .effective
            .capabilities
            .constraints
            .push(storyflow::config::Sourced {
                value: storyflow::config::ToolCapabilitiesRecord {
                    allow: Some(
                        snapshot
                            .effective
                            .capabilities
                            .granted
                            .iter()
                            .cloned()
                            .collect(),
                    ),
                    deny: None,
                },
                origin,
            });
    }
    snapshot.require_cli_capabilities(&capabilities())?;
    Ok(snapshot)
}
