use super::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Resolve fresh inputs. This does not authorize execution or persist a run snapshot.
pub fn resolve(input: &ResolveInput) -> ConfigResult<EffectiveConfig> {
    validate_capabilities(&input.supported_capabilities, &input.granted_capabilities)?;
    compose(
        discovery::discover(input)?,
        &input.supported_capabilities,
        &input.granted_capabilities,
    )
}

/// Explicit repository-only mode; never discovers ambient user/workspace inputs.
pub fn resolve_repository(
    repository: &GitBaseline,
    supported: &BTreeSet<String>,
    granted: &BTreeSet<String>,
) -> ConfigResult<EffectiveConfig> {
    validate_capabilities(supported, granted)?;
    compose(vec![discovery::repository(repository)?], supported, granted)
}

fn validate_capabilities(
    supported: &BTreeSet<String>,
    granted: &BTreeSet<String>,
) -> ConfigResult<()> {
    for id in supported {
        if !parser::identifier(id) {
            return Err(Diagnostic::new(
                "invalid_capability",
                &Origin::host("supported_capabilities"),
                "host capability ID is invalid",
            ));
        }
    }
    if !granted.is_subset(supported) {
        return Err(Diagnostic::new(
            "unknown_capability",
            &Origin::host("granted_capabilities"),
            "host grants an unsupported capability",
        ));
    }
    Ok(())
}

fn compose(
    scopes: Vec<discovery::LoadedScope>,
    supported: &BTreeSet<String>,
    granted: &BTreeSet<String>,
) -> ConfigResult<EffectiveConfig> {
    let mut effective = EffectiveConfig {
        scopes: scopes.iter().map(|s| s.identity.clone()).collect(),
        models: BTreeMap::new(),
        agents: BTreeMap::new(),
        policy: EffectivePolicy {
            protected: BUILTIN_PROTECTED
                .iter()
                .map(|s| Sourced {
                    value: (*s).into(),
                    origin: Origin {
                        scope: None,
                        source: "builtin".into(),
                        field: format!("protected.{s}"),
                    },
                })
                .collect(),
            checks: Vec::new(),
            builtin_scope_gate: Sourced {
                value: true,
                origin: Origin {
                    scope: None,
                    source: "builtin".into(),
                    field: "scope".into(),
                },
            },
        },
        capabilities: EffectiveCapabilities {
            granted: granted.clone(),
            provenance: supported
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        Sourced {
                            value: granted.contains(id),
                            origin: Origin::host("granted_capabilities"),
                        },
                    )
                })
                .collect(),
            constraints: vec![Sourced {
                value: ToolCapabilitiesRecord {
                    allow: Some(granted.iter().cloned().collect()),
                    deny: None,
                },
                origin: Origin::host("granted_capabilities"),
            }],
        },
    };
    let mut check_positions = BTreeMap::<String, usize>::new();
    let mut protected: BTreeSet<_> = effective
        .policy
        .protected
        .iter()
        .map(|p| p.value.to_lowercase())
        .collect();
    for scope in scopes {
        let origin = scope.origin();
        let data = scope.document.data();
        for (tier, hint) in [
            ("economy", data.models.economy),
            ("standard", data.models.standard),
            ("high", data.models.high),
        ] {
            if let Some(value) = hint {
                effective.models.insert(
                    tier.into(),
                    Sourced {
                        value,
                        origin: origin.at(format!("$.models.{tier}")),
                    },
                );
            }
        }
        for (i, agent) in data.agents.iter().enumerate() {
            let at = origin.at(if matches!(scope.document, Document::Legacy(_)) {
                format!("legacy.agents.{}", agent.id)
            } else {
                format!("$.agents[{i}]")
            });
            let bytes = scope.artifact(&agent.source, &at)?;
            let content = String::from_utf8(bytes)
                .map_err(|_| Diagnostic::new("invalid_utf8", &at, "agent is not UTF-8"))?;
            if matches!(scope.document, Document::Legacy(_))
                && (content.trim().is_empty() || content.len() > 4096)
            {
                return Err(Diagnostic::new(
                    "invalid_legacy_artifact",
                    &at,
                    "legacy contracts require 1–4096 nonblank UTF-8 bytes",
                ));
            }
            let sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
            match effective.agents.get(&agent.id) {
                None if agent.replace => {
                    return Err(Diagnostic::new(
                        "missing_override_target",
                        &at,
                        "explicit override has no inherited target",
                    ));
                }
                Some(parent) if !agent.replace => {
                    if parent.version == agent.version && parent.content == content {
                        continue;
                    }
                    return Err(Diagnostic::conflict("agent_conflict", &at, &parent.origin));
                }
                _ => {}
            }
            effective.agents.insert(
                agent.id.clone(),
                ResolvedAgent {
                    id: agent.id.clone(),
                    version: agent.version.clone(),
                    source_path: scope.artifact_path(&agent.source),
                    origin: at,
                    sha256,
                    content,
                },
            );
        }
        let prefix = if matches!(scope.document, Document::Legacy(_)) {
            "$"
        } else {
            "$.policy"
        };
        for (i, path) in data.policy.protected.into_iter().enumerate() {
            if protected.insert(path.to_lowercase()) {
                effective.policy.protected.push(Sourced {
                    value: path,
                    origin: origin.at(format!("{prefix}.protected[{i}]")),
                });
            }
        }
        for (i, check) in data.policy.checks.into_iter().enumerate() {
            let at = origin.at(format!("{prefix}.checks[{i}]"));
            let id = check.id.to_ascii_lowercase();
            if let Some(index) = check_positions.get(&id) {
                let parent = &effective.policy.checks[*index];
                if check.program != parent.value.program
                    || check.args != parent.value.args
                    || check.timeout_seconds != parent.value.timeout_seconds
                {
                    return Err(Diagnostic::conflict("check_conflict", &at, &parent.origin));
                }
            } else {
                check_positions.insert(id, effective.policy.checks.len());
                effective.policy.checks.push(Sourced {
                    value: check,
                    origin: at,
                });
            }
        }
        for (name, list) in [("allow", &data.tools.allow), ("deny", &data.tools.deny)] {
            for (i, id) in list.iter().flatten().enumerate() {
                let at = origin.at(format!("$.tools.{name}[{i}]"));
                if !supported.contains(id) {
                    return Err(Diagnostic::new(
                        "unknown_capability",
                        &at,
                        "capability is not supported by the host",
                    ));
                }
                if name == "allow" && !effective.capabilities.granted.contains(id) {
                    let earlier = effective.capabilities.provenance[id].origin.clone();
                    return Err(Diagnostic::conflict("capability_expansion", &at, &earlier));
                }
            }
        }
        if let Some(allow) = &data.tools.allow {
            for id in effective
                .capabilities
                .granted
                .iter()
                .filter(|id| !allow.contains(id))
            {
                effective.capabilities.provenance.insert(
                    id.clone(),
                    Sourced {
                        value: false,
                        origin: origin.at("$.tools.allow"),
                    },
                );
            }
            effective
                .capabilities
                .granted
                .retain(|id| allow.contains(id));
        }
        for (i, id) in data.tools.deny.iter().flatten().enumerate() {
            if effective.capabilities.granted.remove(id) {
                effective.capabilities.provenance.insert(
                    id.clone(),
                    Sourced {
                        value: false,
                        origin: origin.at(format!("$.tools.deny[{i}]")),
                    },
                );
            }
        }
        if data.tools.allow.is_some() || data.tools.deny.is_some() {
            effective.capabilities.constraints.push(Sourced {
                value: ToolCapabilitiesRecord {
                    allow: data.tools.allow,
                    deny: data.tools.deny,
                },
                origin: origin.at("$.tools"),
            });
        }
    }
    if effective.policy.checks.is_empty() {
        return Err(Diagnostic::new(
            "missing_check",
            &Origin::host("effective.policy.checks"),
            "at least one declared mandatory check is required",
        ));
    }
    for role in REQUIRED_WORKERS {
        if !effective.agents.contains_key(*role) {
            return Err(Diagnostic::new(
                "missing_worker_role",
                &Origin::host(&format!("effective.agents.{role}")),
                "required workflow worker contract is missing",
            ));
        }
    }
    Ok(effective)
}
