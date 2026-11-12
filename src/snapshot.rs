//! Versioned immutable inputs. The journal binds their digest; replay never resolves parents.
use crate::config::{self, EffectiveConfig, GitBaseline, ResolveInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunSnapshot {
    pub schema: u32,
    pub story: String,
    pub base: String,
    pub effective: EffectiveConfig,
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
        };
        snapshot.validate(story, base)?;
        Ok(snapshot)
    }

    /// A host must verify its own enforceable permissions before dispatching.
    /// The CLI cannot enforce worker restrictions yet, so it admits only its full profile.
    pub fn require_cli_capabilities(&self, capabilities: &BTreeSet<String>) -> Result<()> {
        if &self.effective.capabilities.granted != capabilities {
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
        Ok(sha256(&serde_json::to_vec(self)?))
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
        }))
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
