//! Strict scoped configuration, trusted discovery, pinned artifacts and ordered composition.
mod compose;
mod discovery;
mod external;
mod git;
mod parser;
mod paths;
mod types;

pub use compose::{resolve, resolve_repository};
pub use discovery::{HostTrust, ResolveInput, UserEnvironment, UserLocation, UserPlatform};
pub use git::GitBaseline;
pub use parser::parse_document;
pub use paths::{protected_matches, validate_relative_path};
pub use types::*;

/// The worker mapping in main.rs's context dispatcher.
pub const REQUIRED_WORKERS: &[&str] = &["planner", "implementer", "reviewer"];
/// Contracts required by the existing v1 init path, including the orchestrator.
pub const LEGACY_ROLES: &[&str] = &["orchestrator", "planner", "implementer", "reviewer"];
pub const BUILTIN_PROTECTED: &[&str] = &[
    "storyflow.json",
    ".storyflow",
    ".git",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    "stories",
    "agents",
    "AGENTS.md",
    "CLAUDE.md",
];
