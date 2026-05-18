//! Claim mapping — turn a verified JWT payload into an [`Identity`].
//!
//! The mapping is declared on the `AuthStrategy` CRD as
//! `config.issuers[].claims`. Each field (`actorId`, `email`, `roles`,
//! `attributes`) can be:
//!
//! * a bare path string: `"$.sub"` — resolved via JSONPath, no transform
//! * an object: `{ path: "$.scope", transform: { type: "scope_to_roles" } }`
//!
//! Transforms supported in Phase 2a:
//!
//! | type           | input         | output                | notes |
//! |----------------|---------------|-----------------------|-------|
//! | `prefix_strip` | string        | string                | strips `prefix:` once |
//! | `scope_to_roles` | string      | array<string>         | space-split OAuth-style scope |
//! | `lookup`       | string        | string                | static `from -> to` map |
//! | `regex_extract`| string        | string                | first capture group of `pattern` |
//! | `static_append`| array<string> | array<string>         | appends `values` |

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use jsonpath_rust::parser::model::JpQuery;
use jsonpath_rust::parser::parse_json_path;
use jsonpath_rust::query::js_path_process;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use velocity_types::crds::auth::ClaimMapping;

use crate::Identity;

/// Parse a JSONPath at compile time. Returns a stored expression we can run
/// against many payloads. `jsonpath-rust` 1.x exposes `parse_json_path` as
/// the pre-parsing entry point and `js_path_process` to run the resulting
/// [`JpQuery`] against a value.
fn compile_path(path: &str) -> Result<JpQuery, ClaimError> {
    parse_json_path(path).map_err(|e| ClaimError::Mapping {
        field: "path",
        reason: format!("JSONPath `{path}` invalid: {e}"),
    })
}

#[derive(Debug, Error)]
pub enum ClaimError {
    #[error("claim mapping for `{field}`: {reason}")]
    Mapping { field: &'static str, reason: String },
    #[error("required claim `{0}` not found in token")]
    Missing(&'static str),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Transform {
    PrefixStrip { prefix: String },
    ScopeToRoles,
    Lookup { from: HashMap<String, String> },
    RegexExtract { pattern: String },
    StaticAppend { values: Vec<String> },
}

#[derive(Debug, Clone)]
struct CompiledTransform {
    inner: Transform,
    regex: Option<regex::Regex>,
}

impl CompiledTransform {
    fn compile(t: Transform) -> Result<Self, ClaimError> {
        let regex = match &t {
            Transform::RegexExtract { pattern } => Some(regex::Regex::new(pattern).map_err(|e| {
                ClaimError::Mapping {
                    field: "transform",
                    reason: format!("regex_extract pattern `{pattern}` invalid: {e}"),
                }
            })?),
            _ => None,
        };
        Ok(Self { inner: t, regex })
    }

    fn apply(&self, value: Value) -> Result<Value, ClaimError> {
        match &self.inner {
            Transform::PrefixStrip { prefix } => {
                let s = expect_string(value, "prefix_strip")?;
                let stripped = s.strip_prefix(prefix.as_str()).unwrap_or(&s).to_string();
                Ok(Value::String(stripped))
            }
            Transform::ScopeToRoles => {
                let s = expect_string(value, "scope_to_roles")?;
                let roles: Vec<Value> = s
                    .split_whitespace()
                    .map(|p| Value::String(p.to_string()))
                    .collect();
                Ok(Value::Array(roles))
            }
            Transform::Lookup { from } => {
                let s = expect_string(value, "lookup")?;
                let out = from.get(&s).cloned().unwrap_or(s);
                Ok(Value::String(out))
            }
            Transform::RegexExtract { pattern } => {
                let s = expect_string(value, "regex_extract")?;
                let re = self.regex.as_ref().ok_or_else(|| ClaimError::Mapping {
                    field: "regex_extract",
                    reason: "regex was not compiled".into(),
                })?;
                let captures = re.captures(&s).ok_or_else(|| ClaimError::Mapping {
                    field: "regex_extract",
                    reason: format!("pattern `{pattern}` did not match"),
                })?;
                // Prefer the first capture group; fall back to the full match.
                let group = captures.get(1).or_else(|| captures.get(0));
                let out = group.map(|m| m.as_str()).unwrap_or("").to_string();
                Ok(Value::String(out))
            }
            Transform::StaticAppend { values } => {
                let mut arr = match value {
                    Value::Array(a) => a,
                    Value::Null => Vec::new(),
                    other => vec![other],
                };
                arr.extend(values.iter().map(|v| Value::String(v.clone())));
                Ok(Value::Array(arr))
            }
        }
    }
}

fn expect_string(v: Value, t: &'static str) -> Result<String, ClaimError> {
    match v {
        Value::String(s) => Ok(s),
        other => Err(ClaimError::Mapping {
            field: t,
            reason: format!("expected string, got {}", type_of(&other)),
        }),
    }
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[derive(Debug)]
struct Rule {
    path: JpQuery,
    /// Original path string — kept only for error messages and test
    /// inspection. Currently unused in the hot path; retained because a
    /// future failure-mode log will want to cite the raw path.
    #[allow(dead_code)]
    raw_path: String,
    transform: Option<CompiledTransform>,
}

impl Rule {
    /// Parse one mapping value. Accepts a bare string `"$.sub"` or an
    /// object `{ "path": "...", "transform": { ... } }`.
    fn parse(field: &'static str, raw: &Value) -> Result<Self, ClaimError> {
        match raw {
            Value::String(s) => {
                Ok(Self { path: compile_path(s)?, raw_path: s.clone(), transform: None })
            }
            Value::Object(map) => {
                let path_str =
                    map.get("path").and_then(Value::as_str).ok_or_else(|| ClaimError::Mapping {
                        field,
                        reason: "expected string or object with `path`".into(),
                    })?;
                let path = compile_path(path_str)?;
                let transform = match map.get("transform") {
                    None | Some(Value::Null) => None,
                    Some(v) => {
                        let t: Transform =
                            serde_json::from_value(v.clone()).map_err(|e| ClaimError::Mapping {
                                field,
                                reason: format!("invalid transform: {e}"),
                            })?;
                        Some(CompiledTransform::compile(t)?)
                    }
                };
                Ok(Self { path, raw_path: path_str.to_string(), transform })
            }
            other => Err(ClaimError::Mapping {
                field,
                reason: format!("expected string or object, got {}", type_of(other)),
            }),
        }
    }

    /// Resolve the JSONPath against the claims payload and apply the
    /// transform (if any). Returns `None` when the path matches nothing —
    /// callers decide whether that's a hard miss or an optional field.
    ///
    /// `jsonpath-rust` 1.x returns a `Vec<QueryRef>` whose `.val()` borrows
    /// from the input. We unwrap the common single-hit case back to the
    /// underlying value so transforms see the natural shape.
    fn resolve(&self, claims: &Value) -> Result<Option<Value>, ClaimError> {
        let refs = js_path_process(&self.path, claims).map_err(|e| ClaimError::Mapping {
            field: "path",
            reason: format!("JSONPath `{}` failed: {e}", self.raw_path),
        })?;
        if refs.is_empty() {
            return Ok(None);
        }
        let mut values: Vec<Value> = refs.into_iter().map(|r| r.val().clone()).collect();
        let value = if values.len() == 1 { values.remove(0) } else { Value::Array(values) };
        let value = match &self.transform {
            Some(t) => t.apply(value)?,
            None => value,
        };
        Ok(Some(value))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn path_str(&self) -> &str {
        &self.raw_path
    }
}

/// Pre-compiled mapping ready to run on every request. Compile once per
/// `AuthStrategy` reconcile, then share via `Arc`. Not `Clone` — the
/// `JsonPath` parse tree is shared by reference.
#[derive(Debug)]
pub struct CompiledClaimMapping {
    actor_id: Rule,
    email: Option<Rule>,
    roles: Option<Rule>,
    attributes: BTreeMap<String, Rule>,
}

impl CompiledClaimMapping {
    /// Compile the CRD mapping. `actorId` defaults to `"$.sub"` when absent
    /// — every JWT carries `sub`, and not requiring callers to spell it out
    /// removes the most common config papercut.
    pub fn from_crd(mapping: &ClaimMapping) -> Result<Arc<Self>, ClaimError> {
        let actor_id = match &mapping.actor_id {
            Some(v) => Rule::parse("actor_id", v)?,
            None => Rule { path: compile_path("$.sub")?, raw_path: "$.sub".into(), transform: None },
        };
        let email = match &mapping.email {
            Some(v) => Some(Rule::parse("email", v)?),
            None => None,
        };
        let roles = match &mapping.roles {
            Some(v) => Some(Rule::parse("roles", v)?),
            None => None,
        };
        let mut attributes = BTreeMap::new();
        for (name, v) in &mapping.attributes {
            // Leak the name into a 'static slot — only happens at compile
            // time and the strategy registry holds the Arc for the
            // lifetime of the process.
            let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
            attributes.insert(name.clone(), Rule::parse(leaked, v)?);
        }
        Ok(Arc::new(Self { actor_id, email, roles, attributes }))
    }

    /// Run the mapping. The strategy/issuer fields are injected by the
    /// caller — only the [`crate::Identity`] knows about strategy provenance.
    pub fn apply(
        &self,
        claims: &Value,
        strategy: &str,
        issuer: &str,
    ) -> Result<Identity, ClaimError> {
        let actor_value = self.actor_id.resolve(claims)?.ok_or(ClaimError::Missing("actor_id"))?;
        let actor_id = match actor_value {
            Value::String(s) => s,
            other => other.to_string().trim_matches('"').to_string(),
        };

        let email = match self.email.as_ref().and_then(|r| r.resolve(claims).transpose()) {
            Some(Ok(Value::String(s))) => Some(s),
            Some(Ok(_)) => None,
            Some(Err(e)) => return Err(e),
            None => None,
        };

        let roles = match self.roles.as_ref().and_then(|r| r.resolve(claims).transpose()) {
            None => Vec::new(),
            Some(Ok(Value::Array(arr))) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(Ok(Value::String(s))) => vec![s],
            Some(Ok(_)) => Vec::new(),
            Some(Err(e)) => return Err(e),
        };

        let mut attributes = HashMap::new();
        for (name, rule) in &self.attributes {
            if let Some(v) = rule.resolve(claims)? {
                let as_str = match v {
                    Value::String(s) => s,
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    other => other.to_string(),
                };
                attributes.insert(name.clone(), as_str);
            }
        }

        Ok(Identity {
            actor_id,
            email,
            roles,
            attributes,
            strategy: strategy.to_string(),
            issuer: issuer.to_string(),
            // JWT path — Layer-1 will run role-based RBAC, not scope check.
            api_key_scopes: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mapping_from(v: serde_json::Value) -> ClaimMapping {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn default_actor_id_is_sub() {
        let m = mapping_from(json!({}));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let id = c.apply(&json!({ "sub": "alice" }), "s", "i").unwrap();
        assert_eq!(id.actor_id, "alice");
    }

    #[test]
    fn missing_actor_claim_errors() {
        let m = mapping_from(json!({}));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let err = c.apply(&json!({}), "s", "i").unwrap_err();
        assert!(matches!(err, ClaimError::Missing("actor_id")));
    }

    #[test]
    fn prefix_strip_transform() {
        let m = mapping_from(json!({
            "actorId": { "path": "$.sub", "transform": { "type": "prefix_strip", "prefix": "user:" } }
        }));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let id = c.apply(&json!({ "sub": "user:bob" }), "s", "i").unwrap();
        assert_eq!(id.actor_id, "bob");
    }

    #[test]
    fn scope_to_roles_transform_splits_space() {
        let m = mapping_from(json!({
            "roles": { "path": "$.scope", "transform": { "type": "scope_to_roles" } }
        }));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let id = c
            .apply(&json!({ "sub": "alice", "scope": "read:po write:po admin" }), "s", "i")
            .unwrap();
        assert_eq!(id.roles, vec!["read:po", "write:po", "admin"]);
    }

    #[test]
    fn regex_extract_first_group() {
        let m = mapping_from(json!({
            "actorId": {
                "path": "$.sub",
                "transform": { "type": "regex_extract", "pattern": "^user-(.+)$" }
            }
        }));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let id = c.apply(&json!({ "sub": "user-42" }), "s", "i").unwrap();
        assert_eq!(id.actor_id, "42");
    }

    #[test]
    fn lookup_with_passthrough_default() {
        let m = mapping_from(json!({
            "actorId": {
                "path": "$.sub",
                "transform": { "type": "lookup", "from": { "old": "new" } }
            }
        }));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let mapped = c.apply(&json!({ "sub": "old" }), "s", "i").unwrap();
        assert_eq!(mapped.actor_id, "new");
        let untouched = c.apply(&json!({ "sub": "stranger" }), "s", "i").unwrap();
        assert_eq!(untouched.actor_id, "stranger");
    }

    #[test]
    fn static_append_adds_to_array() {
        let m = mapping_from(json!({
            "roles": {
                "path": "$.scope",
                "transform": { "type": "scope_to_roles" }
            }
        }));
        let _ = CompiledClaimMapping::from_crd(&m).unwrap();

        // Composition isn't supported in Phase 2a — static_append used alone
        // on a roles claim that's already an array.
        let m2 = mapping_from(json!({
            "roles": {
                "path": "$.roles",
                "transform": { "type": "static_append", "values": ["default-reader"] }
            }
        }));
        let c2 = CompiledClaimMapping::from_crd(&m2).unwrap();
        let id =
            c2.apply(&json!({ "sub": "alice", "roles": ["admin"] }), "s", "i").unwrap();
        assert_eq!(id.roles, vec!["admin", "default-reader"]);
    }

    #[test]
    fn attributes_are_resolved_per_key() {
        let m = mapping_from(json!({
            "attributes": {
                "region":  "$.region",
                "tenant":  { "path": "$.tenant" }
            }
        }));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let id = c
            .apply(
                &json!({ "sub": "alice", "region": "west", "tenant": "acme" }),
                "s",
                "i",
            )
            .unwrap();
        assert_eq!(id.attributes.get("region").map(String::as_str), Some("west"));
        assert_eq!(id.attributes.get("tenant").map(String::as_str), Some("acme"));
    }

    #[test]
    fn missing_optional_claim_does_not_fail() {
        let m = mapping_from(json!({ "email": "$.email" }));
        let c = CompiledClaimMapping::from_crd(&m).unwrap();
        let id = c.apply(&json!({ "sub": "alice" }), "s", "i").unwrap();
        assert!(id.email.is_none());
    }
}
