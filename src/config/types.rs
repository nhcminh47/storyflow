use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Workspace,
    Repo,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ModelHints {
    pub economy: Option<String>,
    pub standard: Option<String>,
    pub high: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicyDeclaration {
    #[serde(default)]
    pub protected: Vec<String>,
    #[serde(default)]
    pub checks: Vec<Check>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ToolCapabilities {
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserAgent {
    pub id: String,
    pub source: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDeclaration {
    pub id: String,
    pub source: String,
    pub version: String,
    #[serde(default, rename = "override")]
    pub replace: bool,
}

macro_rules! config {
    ($name:ident, $agent:ty $(, $field:ident : $type:ty)?) => {
        #[derive(Clone, Debug, Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub version: u32,
            pub scope: Scope,
            #[serde(default)] pub agents: Vec<$agent>,
            #[serde(default)] pub models: ModelHints,
            #[serde(default)] pub policy: PolicyDeclaration,
            #[serde(default)] pub tools: ToolCapabilities,
            $(pub $field: $type,)?
        }
    };
}
config!(UserConfig, UserAgent, workspaces: Vec<String>);
config!(WorkspaceConfig, AgentDeclaration, members: Vec<String>);
config!(RepoConfig, AgentDeclaration);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRepoConfig {
    pub version: u32,
    pub protected: Vec<String>,
    pub checks: Vec<Check>,
}

#[derive(Clone, Debug)]
pub enum Document {
    User(UserConfig),
    Workspace(WorkspaceConfig),
    Repo(RepoConfig),
    Legacy(LegacyRepoConfig),
}

/// An origin never contains a field value, command arguments or artifact text.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Origin {
    pub scope: Option<Scope>,
    pub source: String,
    pub field: String,
}
impl Origin {
    pub fn new(scope: Scope, source: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            scope: Some(scope),
            source: source.into(),
            field: field.into(),
        }
    }
    pub(crate) fn at(&self, field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            ..self.clone()
        }
    }
    pub(crate) fn host(field: &str) -> Self {
        Self {
            scope: None,
            source: "host".into(),
            field: field.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub origin: Origin,
    pub related: Option<Box<Origin>>,
    pub message: &'static str,
}
impl Diagnostic {
    pub(crate) fn new(code: &'static str, origin: &Origin, message: &'static str) -> Self {
        Self {
            code,
            origin: origin.clone(),
            related: None,
            message,
        }
    }
    pub(crate) fn conflict(code: &'static str, origin: &Origin, related: &Origin) -> Self {
        Self {
            related: Some(Box::new(related.clone())),
            ..Self::new(code, origin, "declaration conflicts with an earlier origin")
        }
    }
}
impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} at {} {}",
            self.code, self.message, self.origin.source, self.origin.field
        )
    }
}
impl std::error::Error for Diagnostic {}
pub type ConfigResult<T> = Result<T, Diagnostic>;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub origin: Origin,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub id: String,
    pub version: String,
    pub source_path: String,
    pub origin: Origin,
    pub sha256: String,
    /// Exact UTF-8 bytes consumed (JSON escapes preserve them on serialization).
    pub content: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EffectivePolicy {
    pub protected: Vec<Sourced<String>>,
    pub checks: Vec<Sourced<Check>>,
    pub builtin_scope_gate: Sourced<bool>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ScopeIdentity {
    pub scope: Scope,
    pub root: String,
    pub source: String,
    pub baseline: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EffectiveCapabilities {
    pub granted: BTreeSet<String>,
    /// One decision for every host-supported capability, including the precise
    /// field that removed a grant. Unchanged grants retain their earlier origin.
    pub provenance: BTreeMap<String, Sourced<bool>>,
    /// Ordered host grant and scope constraints, including origins of removals.
    pub constraints: Vec<Sourced<ToolCapabilitiesRecord>>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ToolCapabilitiesRecord {
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub scopes: Vec<ScopeIdentity>,
    pub models: BTreeMap<String, Sourced<String>>,
    pub agents: BTreeMap<String, ResolvedAgent>,
    pub policy: EffectivePolicy,
    pub capabilities: EffectiveCapabilities,
}
