//! Versioned immutable inputs. The journal binds their digest; replay never resolves parents.
use crate::config::{self, EffectiveConfig, GitBaseline, ResolveInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{self, Write},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Includes the trailing newline in the persisted compact JSON representation.
pub const MAX_SNAPSHOT_BYTES: usize = 1_048_576;

struct SnapshotBytes(Vec<u8>);
impl Write for SnapshotBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_SNAPSHOT_BYTES - 1).saturating_sub(self.0.len()) {
            return Err(io::Error::other(
                "config_snapshot_too_large: serialized snapshot exceeds 1 MiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunSnapshot {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub effective: EffectiveConfig,
    /// Opt-in managed-only admission; omission preserves historical snapshot digests.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub managed_only: bool,
}

impl RunSnapshot {
    pub fn resolve(story: &str, input: &ResolveInput) -> Result<Self> {
        Self::new(story, &input.repository.commit, config::resolve(input)?)
    }

    pub fn repository(
        story: &str,
        baseline: &GitBaseline,
        capabilities: &BTreeSet<String>,
    ) -> Result<Self> {
        Self::new(
            story,
            &baseline.commit,
            config::resolve_repository(baseline, capabilities, capabilities)?,
        )
    }

    fn new(story: &str, base: &str, effective: EffectiveConfig) -> Result<Self> {
        let snapshot = Self {
            schema: 1,
            story: story.into(),
            base: base.into(),
            effective,
            managed_only: false,
        };
        snapshot.validate(story, base)?;
        snapshot.canonical_bytes()?;
        Ok(snapshot)
    }

    /// A host must verify its own enforceable permissions before dispatching.
    /// Reduced grants require the explicitly pinned managed-only host profile.
    pub fn require_cli_capabilities(&self, capabilities: &BTreeSet<String>) -> Result<()> {
        if !self.managed_only && &self.effective.capabilities.granted != capabilities {
            return Err("unsupported_capability_enforcement: this CLI host cannot enforce reduced worker permissions; run refused".into());
        }
        for role in config::REQUIRED_WORKERS {
            let agent = self
                .effective
                .agents
                .get(*role)
                .ok_or("pinned worker missing")?;
            if agent.content.trim().is_empty() || agent.content.len() > 4096 {
                return Err(
                    "invalid_run_contract: worker contracts require 1–4096 nonblank UTF-8 bytes"
                        .into(),
                );
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(sha256(&self.canonical_bytes()?))
    }

    /// Bounded canonical encoding; reserve one byte for the on-disk newline.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = SnapshotBytes(Vec::new());
        serde_json::to_writer(&mut bytes, self)?;
        Ok(bytes.0)
    }

    pub fn verify(&self, story: &str, base: &str, expected: &str) -> Result<()> {
        if self.digest()? != expected {
            return Err("config_snapshot_mismatch: pinned run inputs were modified; restore the original snapshot; rebind is unsupported".into());
        }
        self.validate(story, base)
    }

    fn validate(&self, story: &str, base: &str) -> Result<()> {
        if self.schema != 1 || self.story != story || self.base != base {
            return Err("invalid_config_snapshot: schema, story or baseline mismatch; rebind is unsupported".into());
        }
        let policy = &self.effective.policy;
        if !policy.builtin_scope_gate.value
            || policy.checks.is_empty()
            || config::BUILTIN_PROTECTED.iter().any(|floor| {
                !policy
                    .protected
                    .iter()
                    .any(|p| p.value.eq_ignore_ascii_case(floor))
            })
        {
            return Err("invalid_config_snapshot: mandatory policy floor missing".into());
        }
        let repos: Vec<_> = self
            .effective
            .scopes
            .iter()
            .filter(|s| s.scope == config::Scope::Repo)
            .collect();
        if repos.len() != 1 || repos[0].baseline.as_deref() != Some(base) {
            return Err("invalid_config_snapshot: repository baseline mismatch".into());
        }
        for role in config::REQUIRED_WORKERS {
            if !self.effective.agents.contains_key(*role) {
                return Err("invalid_config_snapshot: worker missing".into());
            }
        }
        for (id, agent) in &self.effective.agents {
            if agent.content.len() > config::MAX_AGENT_BYTES {
                return Err("invalid_config_snapshot: agent exceeds 64 KiB".into());
            }
            if id != &agent.id || agent.sha256 != sha256(agent.content.as_bytes()) {
                return Err("invalid_config_snapshot: agent digest mismatch".into());
            }
        }
        Ok(())
    }

    /// Audit metadata deliberately excludes command arguments, model values and contract bytes.
    pub fn audit(&self) -> Result<Value> {
        Ok(json!({
            "schema": self.schema,
            "sha256": self.digest()?,
            "scopes": self.effective.scopes,
            "agents": self.effective.agents.iter().map(|(id, a)| (id.clone(), json!({
                "version": a.version, "sha256": a.sha256, "origin": a.origin
            }))).collect::<std::collections::BTreeMap<_, _>>(),
            "protected": self.effective.policy.protected,
            "checks": self.effective.policy.checks.iter().map(|c| json!({"id": c.value.id, "origin": c.origin})).collect::<Vec<_>>(),
            "models": self.effective.models.iter().map(|(tier, m)| (tier.clone(), m.origin.clone())).collect::<std::collections::BTreeMap<_, _>>(),
            "capabilities": self.effective.capabilities,
            "managed_only": self.managed_only,
        }))
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
