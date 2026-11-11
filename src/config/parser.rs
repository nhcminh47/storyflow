use super::*;
use serde::{
    Deserialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

// DeserializeSeed visits every object before serde_json::Value can collapse keys.
struct StrictValue(String);
impl<'de> DeserializeSeed<'de> for StrictValue {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for StrictValue {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a non-null JSON value")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Value, M::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let path = format!("{}.{}", self.0, key);
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate_key:{path}")));
            }
            values.insert(key, map.next_value_seed(StrictValue(path))?);
        }
        Ok(Value::Object(values))
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Value, S::Error> {
        let mut values = Vec::new();
        while let Some(v) =
            seq.next_element_seed(StrictValue(format!("{}[{}]", self.0, values.len())))?
        {
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Err(E::custom(format!("null_value:{}", self.0)))
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(v.into())
    }
}

#[derive(Clone, Copy)]
enum Kind {
    String,
    Strings,
    Number,
    Bool,
    Object(&'static [Field]),
    Array(&'static [Field]),
}
type Field = (&'static str, Kind, bool);
const CHECK: &[Field] = &[
    ("id", Kind::String, true),
    ("program", Kind::String, true),
    ("args", Kind::Strings, true),
    ("timeout_seconds", Kind::Number, true),
];
const AGENT: &[Field] = &[
    ("id", Kind::String, true),
    ("source", Kind::String, true),
    ("version", Kind::String, true),
    ("override", Kind::Bool, false),
];
const USER_AGENT: &[Field] = &[
    ("id", Kind::String, true),
    ("source", Kind::String, true),
    ("version", Kind::String, true),
];
const MODELS: &[Field] = &[
    ("economy", Kind::String, false),
    ("standard", Kind::String, false),
    ("high", Kind::String, false),
];
const POLICY: &[Field] = &[
    ("protected", Kind::Strings, false),
    ("checks", Kind::Array(CHECK), false),
];
const TOOLS: &[Field] = &[
    ("allow", Kind::Strings, false),
    ("deny", Kind::Strings, false),
];

fn shape(v: &Value, fields: &[Field], origin: &Origin) -> ConfigResult<()> {
    let object = v
        .as_object()
        .ok_or_else(|| Diagnostic::new("invalid_type", origin, "expected an object"))?;
    for key in object.keys() {
        if !fields.iter().any(|f| f.0 == key) {
            return Err(Diagnostic::new(
                "unknown_field",
                &origin.at(format!("{}.{key}", origin.field)),
                "unknown field",
            ));
        }
    }
    for &(key, kind, required) in fields {
        let at = origin.at(format!("{}.{key}", origin.field));
        let Some(value) = object.get(key) else {
            if required {
                return Err(Diagnostic::new(
                    "missing_field",
                    &at,
                    "required field is absent",
                ));
            }
            continue;
        };
        let valid = match kind {
            Kind::String => value.is_string(),
            Kind::Number => value.as_u64().is_some(),
            Kind::Bool => value.is_boolean(),
            Kind::Strings => {
                let arr = value
                    .as_array()
                    .ok_or_else(|| Diagnostic::new("invalid_type", &at, "expected an array"))?;
                for (i, item) in arr.iter().enumerate() {
                    if !item.is_string() {
                        return Err(Diagnostic::new(
                            "invalid_type",
                            &at.at(format!("{}[{i}]", at.field)),
                            "expected a string",
                        ));
                    }
                }
                true
            }
            Kind::Object(fields) => {
                shape(value, fields, &at)?;
                true
            }
            Kind::Array(fields) => {
                let arr = value
                    .as_array()
                    .ok_or_else(|| Diagnostic::new("invalid_type", &at, "expected an array"))?;
                for (i, item) in arr.iter().enumerate() {
                    shape(item, fields, &at.at(format!("{}[{i}]", at.field)))?;
                }
                true
            }
        };
        if !valid {
            return Err(Diagnostic::new("invalid_type", &at, "incorrect field type"));
        }
    }
    Ok(())
}

pub fn parse_document(bytes: &[u8], scope: Scope, source: &str) -> ConfigResult<Document> {
    let origin = Origin::new(scope, source, "$");
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let mut value = StrictValue("$".into())
        .deserialize(&mut deserializer)
        .map_err(|e| {
            let msg = e.to_string();
            for code in ["duplicate_key", "null_value"] {
                if let Some(field) = msg.strip_prefix(&format!("{code}:")) {
                    return Diagnostic::new(
                        code,
                        &origin.at(field.split(" at line ").next().unwrap_or("$")),
                        "invalid JSON field",
                    );
                }
            }
            Diagnostic::new("invalid_json", &origin, "malformed JSON")
        })?;
    deserializer
        .end()
        .map_err(|_| Diagnostic::new("invalid_json", &origin, "trailing JSON input"))?;
    if !value.is_object() {
        return Err(Diagnostic::new(
            "invalid_type",
            &origin,
            "expected a configuration object",
        ));
    }
    // JSON Schema integers include integral number spellings such as 2.0 and
    // 6e1. Normalize only v2's bounded integer fields before typed decoding;
    // retain the existing v1 deserializer's integer-token behavior.
    if value.get("version").and_then(Value::as_f64) == Some(2.0) {
        value["version"] = Value::from(2u32);
        if let Some(checks) = value
            .get_mut("policy")
            .and_then(|p| p.get_mut("checks"))
            .and_then(Value::as_array_mut)
        {
            for check in checks {
                if let Some(timeout) = check.get_mut("timeout_seconds")
                    && let Some(number) = timeout.as_f64()
                    && number.fract() == 0.0
                    && (0.0..=3600.0).contains(&number)
                {
                    *timeout = Value::from(number as u64);
                }
            }
        }
    }
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            Diagnostic::new(
                "invalid_version",
                &origin.at("$.version"),
                "version must be an integer",
            )
        })?;
    if version != 2 && !(version == 1 && scope == Scope::Repo) {
        return Err(Diagnostic::new(
            "unsupported_version",
            &origin.at("$.version"),
            "unsupported version for this source",
        ));
    }
    if version == 1 {
        if bytes.len() > 16_384 {
            return Err(Diagnostic::new(
                "invalid_legacy_config",
                &origin,
                "legacy policy exceeds the v0.1 byte limit",
            ));
        }
        shape(
            &value,
            &[
                ("version", Kind::Number, true),
                ("protected", Kind::Strings, true),
                ("checks", Kind::Array(CHECK), true),
            ],
            &origin,
        )?;
        let doc: LegacyRepoConfig = decode(value, &origin)?;
        if doc.checks.is_empty() {
            return Err(Diagnostic::new(
                "missing_check",
                &origin.at("$.checks"),
                "legacy policy requires a check",
            ));
        }
        validate_policy(
            &PolicyDeclaration {
                protected: doc.protected.clone(),
                checks: doc.checks.clone(),
            },
            true,
            &origin,
        )?;
        return Ok(Document::Legacy(doc));
    }
    let mut fields = vec![
        ("version", Kind::Number, true),
        ("scope", Kind::String, true),
        (
            "agents",
            Kind::Array(if scope == Scope::User {
                USER_AGENT
            } else {
                AGENT
            }),
            false,
        ),
        ("models", Kind::Object(MODELS), false),
        ("policy", Kind::Object(POLICY), false),
        ("tools", Kind::Object(TOOLS), false),
    ];
    if scope == Scope::User {
        fields.push(("workspaces", Kind::Strings, false));
    }
    if scope == Scope::Workspace {
        fields.push(("members", Kind::Strings, true));
    }
    shape(&value, &fields, &origin)?;
    let actual: Scope = decode(value["scope"].clone(), &origin.at("$.scope"))?;
    if actual != scope {
        return Err(Diagnostic::new(
            "incorrect_scope",
            &origin.at("$.scope"),
            "scope does not match document source",
        ));
    }
    // Optional fields default only when absent; StrictValue has already rejected nulls.
    if scope == Scope::User && value.get("workspaces").is_none() {
        value["workspaces"] = serde_json::json!([]);
    }
    let doc = match scope {
        Scope::User => Document::User(decode(value, &origin)?),
        Scope::Workspace => Document::Workspace(decode(value, &origin)?),
        Scope::Repo => Document::Repo(decode(value, &origin)?),
    };
    let data = doc.data();
    validate_policy(&data.policy, false, &origin.at("$.policy"))?;
    for (key, v) in [
        ("economy", &data.models.economy),
        ("standard", &data.models.standard),
        ("high", &data.models.high),
    ] {
        if v.as_ref().is_some_and(String::is_empty) {
            return Err(Diagnostic::new(
                "invalid_model",
                &origin.at(format!("$.models.{key}")),
                "model hint must be nonempty",
            ));
        }
    }
    let mut ids = std::collections::BTreeMap::new();
    for (i, a) in data.agents.iter().enumerate() {
        let at = origin.at(format!("$.agents[{i}]"));
        if !identifier(&a.id) {
            return Err(Diagnostic::new(
                "invalid_identifier",
                &at.at(format!("{}.id", at.field)),
                "invalid agent ID",
            ));
        }
        if let Some(previous) = ids.insert(&a.id, at.clone()) {
            return Err(Diagnostic::conflict("duplicate_agent", &at, &previous));
        }
        validate_relative_path(&a.source, &at.at(format!("{}.source", at.field)))?;
        if !a.source.ends_with(".md") || a.source.len() <= 3 || a.version.is_empty() {
            return Err(Diagnostic::new(
                "invalid_agent",
                &at,
                "agent requires a .md source and nonempty version",
            ));
        }
    }
    for (name, list) in [("allow", &data.tools.allow), ("deny", &data.tools.deny)] {
        let mut ids = BTreeSet::new();
        for (i, id) in list.iter().flatten().enumerate() {
            if !identifier(id) || !ids.insert(id) {
                return Err(Diagnostic::new(
                    "invalid_capability",
                    &origin.at(format!("$.tools.{name}[{i}]")),
                    "invalid or duplicate capability ID",
                ));
            }
        }
    }
    Ok(doc)
}

fn decode<T: for<'de> Deserialize<'de>>(v: Value, at: &Origin) -> ConfigResult<T> {
    serde_json::from_value(v)
        .map_err(|_| Diagnostic::new("invalid_type", at, "invalid typed configuration"))
}
pub(crate) fn identifier(id: &str) -> bool {
    id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
fn validate_policy(policy: &PolicyDeclaration, legacy: bool, origin: &Origin) -> ConfigResult<()> {
    let mut ids = std::collections::BTreeMap::new();
    for (i, check) in policy.checks.iter().enumerate() {
        let at = origin.at(format!("{}.checks[{i}]", origin.field));
        if check.id.is_empty()
            || check.id.len() > 40
            || check.id.eq_ignore_ascii_case("scope")
            || !check
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            || check.program.trim().is_empty()
            || !(1..=3600).contains(&check.timeout_seconds)
        {
            return Err(Diagnostic::new(
                "invalid_check",
                &at,
                "invalid check definition",
            ));
        }
        if let Some(previous) = ids.insert(check.id.to_ascii_lowercase(), at.clone()) {
            return Err(Diagnostic::conflict("duplicate_check", &at, &previous));
        }
    }
    for (i, path) in policy.protected.iter().enumerate() {
        let at = origin.at(format!("{}.protected[{i}]", origin.field));
        if legacy {
            paths::legacy_path(path, &at)?;
        } else {
            validate_relative_path(path, &at)?;
        }
    }
    Ok(())
}

pub(crate) struct ScopeData {
    pub agents: Vec<AgentDeclaration>,
    pub models: ModelHints,
    pub policy: PolicyDeclaration,
    pub tools: ToolCapabilities,
}
impl Document {
    pub(crate) fn data(&self) -> ScopeData {
        match self {
            Self::User(d) => ScopeData {
                agents: d
                    .agents
                    .iter()
                    .map(|a| AgentDeclaration {
                        id: a.id.clone(),
                        source: a.source.clone(),
                        version: a.version.clone(),
                        replace: false,
                    })
                    .collect(),
                models: d.models.clone(),
                policy: d.policy.clone(),
                tools: d.tools.clone(),
            },
            Self::Workspace(d) => ScopeData {
                agents: d.agents.clone(),
                models: d.models.clone(),
                policy: d.policy.clone(),
                tools: d.tools.clone(),
            },
            Self::Repo(d) => ScopeData {
                agents: d.agents.clone(),
                models: d.models.clone(),
                policy: d.policy.clone(),
                tools: d.tools.clone(),
            },
            Self::Legacy(d) => ScopeData {
                agents: LEGACY_ROLES
                    .iter()
                    .map(|r| AgentDeclaration {
                        id: (*r).into(),
                        source: format!("agents/{r}.md"),
                        version: "legacy-v1".into(),
                        replace: false,
                    })
                    .collect(),
                models: ModelHints::default(),
                policy: PolicyDeclaration {
                    protected: d.protected.clone(),
                    checks: d.checks.clone(),
                },
                tools: ToolCapabilities::default(),
            },
        }
    }
}
