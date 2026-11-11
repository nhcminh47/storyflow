use super::{external::TrustedRoot, *};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug)]
pub enum UserPlatform {
    Windows,
    Unix,
}
/// Capture the invoking user's environment in the host; resolution never reads ambient env.
#[derive(Clone, Debug, Default)]
pub struct UserEnvironment {
    pub appdata: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
}
#[derive(Clone, Debug)]
pub struct UserLocation {
    pub base: PathBuf,
    pub directory: String,
}
impl UserLocation {
    pub fn from_environment(platform: UserPlatform, env: &UserEnvironment) -> ConfigResult<Self> {
        let at = Origin::host("user_location");
        let (base, directory) = match platform {
            UserPlatform::Windows => (env.appdata.clone(), "Storyflow"),
            UserPlatform::Unix => (
                env.xdg_config_home
                    .clone()
                    .filter(|p| p.is_absolute())
                    .or_else(|| {
                        env.home
                            .as_ref()
                            .filter(|p| p.is_absolute())
                            .map(|p| p.join(".config"))
                    }),
                "storyflow",
            ),
        };
        let base = base.ok_or_else(|| {
            Diagnostic::new(
                "unavailable_user_location",
                &at,
                "invoking user configuration base is unavailable",
            )
        })?;
        external::absolute(&base, &at)?;
        Ok(Self {
            base,
            directory: directory.into(),
        })
    }
}
/// These exact canonical identities are authorized by the host, never by documents.
#[derive(Clone, Debug)]
pub struct HostTrust {
    pub user_base: PathBuf,
    pub workspace_roots: BTreeSet<PathBuf>,
}
#[derive(Clone, Debug)]
pub struct ResolveInput {
    pub repository: GitBaseline,
    pub user_location: UserLocation,
    pub workspace_selection: Option<PathBuf>,
    pub trust: HostTrust,
    pub supported_capabilities: BTreeSet<String>,
    pub granted_capabilities: BTreeSet<String>,
}

pub(crate) enum ArtifactRoot {
    External(TrustedRoot),
    Git(GitBaseline),
}
pub(crate) struct LoadedScope {
    pub identity: ScopeIdentity,
    pub document: Document,
    pub artifacts: ArtifactRoot,
}
impl LoadedScope {
    pub fn origin(&self) -> Origin {
        Origin::new(self.identity.scope, &self.identity.source, "$")
    }
    pub fn artifact(&self, path: &str, origin: &Origin) -> ConfigResult<Vec<u8>> {
        match &self.artifacts {
            ArtifactRoot::External(root) => root
                .read(path, false, origin)?
                .ok_or_else(|| Diagnostic::new("missing_artifact", origin, "artifact absent")),
            ArtifactRoot::Git(baseline) => baseline.read(path, origin),
        }
    }
    pub fn artifact_path(&self, path: &str) -> String {
        match &self.artifacts {
            ArtifactRoot::External(root) => root.path.join(path).to_string_lossy().into_owned(),
            ArtifactRoot::Git(baseline) => format!("{}:{path}", baseline.commit),
        }
    }
}
fn identity(scope: Scope, root: &Path, source: String, baseline: Option<String>) -> ScopeIdentity {
    ScopeIdentity {
        scope,
        root: root.to_string_lossy().into_owned(),
        source,
        baseline,
    }
}

fn reject_checkout(root: &Path, origin: &Origin) -> ConfigResult<()> {
    // A broken Git marker must not turn a checkout into a trusted user directory.
    // Only inspect marker metadata; never consume project-controlled file bytes.
    for ancestor in root.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {
                return Err(Diagnostic::new(
                    "untrusted_user_location",
                    origin,
                    "user configuration cannot be inside a Git checkout",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(Diagnostic::new(
                    "untrusted_user_location",
                    origin,
                    "cannot establish that user configuration is outside a checkout",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn discover(input: &ResolveInput) -> ConfigResult<Vec<LoadedScope>> {
    let mut scopes = Vec::new();
    input.repository.validate()?;
    let repo_at = Origin::new(
        Scope::Repo,
        input.repository.root.to_string_lossy(),
        "repository",
    );
    let repo = git::top_level(&input.repository.root, &repo_at)?;
    let supplied = std::fs::canonicalize(&input.repository.root)
        .map_err(|_| Diagnostic::new("invalid_repository", &repo_at, "repository unavailable"))?;
    if supplied != repo {
        return Err(Diagnostic::new(
            "invalid_repository",
            &repo_at,
            "repository context must be its Git top-level",
        ));
    }

    let user_at = Origin::new(
        Scope::User,
        input.user_location.base.to_string_lossy(),
        "user_location",
    );
    let base = TrustedRoot::establish(&input.user_location.base, &user_at).map_err(|e| {
        if e.code == "missing_artifact" {
            Diagnostic {
                code: "unavailable_user_location",
                ..e
            }
        } else {
            e
        }
    })?;
    if base.path != input.trust.user_base {
        return Err(Diagnostic::new(
            "untrusted_user_location",
            &user_at,
            "host has not authorized this canonical user base",
        ));
    }
    // A physical configuration directory inside any checkout is not a user root.
    reject_checkout(&base.path, &user_at)?;
    let user_root = match base.directory(&input.user_location.directory, &user_at) {
        Ok(root) => Some(root),
        Err(e) if e.code == "missing_artifact" => None,
        Err(e) => return Err(e),
    };
    let mut registrations = Vec::new();
    if let Some(root) = user_root {
        reject_checkout(&root.path, &user_at)?;
        let source = root.path.join("config.json").to_string_lossy().into_owned();
        if let Some(bytes) =
            root.read("config.json", true, &Origin::new(Scope::User, &source, "$"))?
        {
            let document = parse_document(&bytes, Scope::User, &source)?;
            if let Document::User(user) = &document {
                registrations = user.workspaces.clone();
            }
            scopes.push(LoadedScope {
                identity: identity(Scope::User, &root.path, source, None),
                document,
                artifacts: ArtifactRoot::External(root),
            });
        }
    }

    let mut roots = Vec::new();
    let mut seen = std::collections::BTreeMap::new();
    // Establish authorization for ALL registrations before opening ANY marker.
    for (i, registration) in registrations.iter().enumerate() {
        let at = scopes[0].origin().at(format!("$.workspaces[{i}]"));
        let root = TrustedRoot::establish(Path::new(registration), &at)?;
        if let Some(previous) = seen.insert(root.path.clone(), at.clone()) {
            return Err(Diagnostic::conflict("duplicate_workspace", &at, &previous));
        }
        if !input.trust.workspace_roots.contains(&root.path) {
            return Err(Diagnostic::new(
                "untrusted_workspace",
                &at,
                "registered root lacks host authorization",
            ));
        }
        roots.push((root, at));
    }
    let selection = if let Some(selected) = &input.workspace_selection {
        let at = Origin::host("workspace_selection");
        let root = TrustedRoot::establish(selected, &at)?;
        if !seen.contains_key(&root.path) {
            return Err(Diagnostic::new(
                "unauthorized_selection",
                &at,
                "selection must refer to a registered root",
            ));
        }
        Some(root.path.clone())
    } else {
        None
    };
    let mut matches = Vec::new();
    for (root, at) in roots {
        let source = root
            .path
            .join("storyflow.workspace.json")
            .to_string_lossy()
            .into_owned();
        let marker_at = Origin::new(Scope::Workspace, &source, "$");
        let bytes = root
            .read("storyflow.workspace.json", false, &marker_at)
            .map_err(|e| {
                if e.code == "missing_artifact" {
                    Diagnostic::new(
                        "missing_workspace_marker",
                        &at,
                        "registered workspace marker is absent",
                    )
                } else {
                    e
                }
            })?
            .expect("required read");
        let document = parse_document(&bytes, Scope::Workspace, &source)?;
        let Document::Workspace(workspace) = &document else {
            unreachable!()
        };
        let mut members = std::collections::BTreeMap::new();
        let mut claims = false;
        for (i, member) in workspace.members.iter().enumerate() {
            let at = marker_at.at(format!("$.members[{i}]"));
            let directory = root.directory(member, &at)?;
            let top = git::top_level(&directory.path, &at)?;
            if top != directory.path {
                return Err(Diagnostic::new(
                    "invalid_member",
                    &at,
                    "member must be a Git top-level",
                ));
            }
            if let Some(previous) = members.insert(top.clone(), at.clone()) {
                return Err(Diagnostic::conflict("duplicate_member", &at, &previous));
            }
            if top == repo {
                claims = true;
            }
        }
        if claims {
            matches.push(LoadedScope {
                identity: identity(Scope::Workspace, &root.path, source, None),
                document,
                artifacts: ArtifactRoot::External(root),
            });
        }
    }
    if matches.len() > 1 {
        return Err(Diagnostic::conflict(
            "ambiguous_workspace",
            &matches[1].origin(),
            &matches[0].origin(),
        ));
    }
    if let Some(selected) = selection
        && matches
            .first()
            .is_none_or(|scope| Path::new(&scope.identity.root) != selected)
    {
        return Err(Diagnostic::new(
            "workspace_membership_mismatch",
            &Origin::host("workspace_selection"),
            "selection must match the unique member claim",
        ));
    }
    scopes.extend(matches);
    let source = format!("{}:storyflow.json", input.repository.commit);
    let bytes = input
        .repository
        .read("storyflow.json", &Origin::new(Scope::Repo, &source, "$"))?;
    let document = parse_document(&bytes, Scope::Repo, &source)?;
    scopes.push(LoadedScope {
        identity: identity(
            Scope::Repo,
            &repo,
            source,
            Some(input.repository.commit.clone()),
        ),
        document,
        artifacts: ArtifactRoot::Git(input.repository.clone()),
    });
    Ok(scopes)
}
