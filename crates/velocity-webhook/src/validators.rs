//! Validation rules.
//!
//! Each rule takes the parsed CRD object and returns either `Ok(())` or a
//! human-readable rejection reason. Webhook handler glues these together
//! per Kind.

use cel_interpreter::Program;
use serde_json::Value;
use thiserror::Error;
use velocity_types::common::kebab;

const MAX_CEL_LEN: usize = 10 * 1024;
const MAX_CEL_DEPTH: usize = 10;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ValidationFailure(pub String);

pub type ValidationResult = Result<(), ValidationFailure>;

/// `Domain.metadata.namespace` must equal `kebab(org)-kebab(app)`.
/// `org` is taken from the `velocity.sh/org` label, `app` from `spec.app`.
pub fn validate_domain(obj: &Value, namespace: &str) -> ValidationResult {
    let org = obj
        .pointer("/metadata/labels/velocity.sh~1org")
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationFailure("Domain must have label `velocity.sh/org`".into()))?;
    let app = obj
        .pointer("/spec/app")
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationFailure("Domain.spec.app is required".into()))?;

    let expected = format!("{}-{}", kebab(org), kebab(app));
    if namespace != expected {
        return Err(ValidationFailure(format!(
            "Domain namespace `{namespace}` must equal `{expected}` (from labels+spec)",
        )));
    }
    Ok(())
}

/// `Application.metadata.namespace` must equal `{org}-platform`.
pub fn validate_application(obj: &Value, namespace: &str) -> ValidationResult {
    let org = obj
        .pointer("/metadata/labels/velocity.sh~1org")
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationFailure("Application must have label `velocity.sh/org`".into()))?;
    let expected = format!("{}-platform", kebab(org));
    if namespace != expected {
        return Err(ValidationFailure(format!(
            "Application namespace `{namespace}` must equal `{expected}`",
        )));
    }
    Ok(())
}

/// SchemaDefinition validators. Bundle the per-Kind checks behind one
/// entry point so the handler doesn't need to know which rules apply.
///
/// `multi_tenant_mode` enables the ADR-010 cross-org guard: in a shared
/// cluster, any `ref` field that points at a schema in a different org is
/// rejected at admission time. Single-tenant clusters leave it off.
pub fn validate_schema_definition(
    obj: &Value,
    namespace: &str,
    multi_tenant_mode: bool,
) -> ValidationResult {
    let org = obj.pointer("/metadata/labels/velocity.sh~1org").and_then(Value::as_str).ok_or_else(
        || ValidationFailure("SchemaDefinition must have label `velocity.sh/org`".into()),
    )?;
    let app = obj.pointer("/metadata/labels/velocity.sh~1app").and_then(Value::as_str).ok_or_else(
        || ValidationFailure("SchemaDefinition must have label `velocity.sh/app`".into()),
    )?;
    let domain =
        obj.pointer("/metadata/labels/velocity.sh~1domain").and_then(Value::as_str).ok_or_else(
            || ValidationFailure("SchemaDefinition must have label `velocity.sh/domain`".into()),
        )?;

    let expected = format!("{}-{}-{}", kebab(org), kebab(app), kebab(domain));
    if namespace != expected {
        return Err(ValidationFailure(format!(
            "SchemaDefinition namespace `{namespace}` must equal `{expected}`",
        )));
    }

    if let Some(validations) = obj.pointer("/spec/validations").and_then(Value::as_array) {
        for (i, v) in validations.iter().enumerate() {
            check_cel_safety(v, i)?;
        }
    }

    validate_field_refs(obj, org, multi_tenant_mode)?;
    validate_fts_weights(obj)?;

    Ok(())
}

/// Phase 5d — reject `ftsWeight` on fields that won't carry it into the
/// generated tsvector. The DDL builder filters non-searchable / non-string
/// fields out of the `__fts` expression silently, so without this check a
/// user could set `ftsWeight: A` on a numeric column and never notice that
/// it has no effect. Failing at admission tells them now.
fn validate_fts_weights(obj: &Value) -> ValidationResult {
    let Some(fields) = obj.pointer("/spec/fields").and_then(Value::as_array) else {
        return Ok(());
    };
    for (i, f) in fields.iter().enumerate() {
        let Some(weight) = f.pointer("/ftsWeight").and_then(Value::as_str) else {
            continue;
        };
        let name = f.pointer("/name").and_then(Value::as_str).unwrap_or("<unnamed>");
        if !matches!(weight, "A" | "B" | "C" | "D") {
            return Err(ValidationFailure(format!(
                "fields[{i}] (`{name}`): ftsWeight must be one of A, B, C, D \
                 (got `{weight}`)"
            )));
        }
        let searchable = f.pointer("/searchable").and_then(Value::as_bool).unwrap_or(false);
        let kind = f.pointer("/type").and_then(Value::as_str).unwrap_or("");
        if !searchable {
            return Err(ValidationFailure(format!(
                "fields[{i}] (`{name}`): ftsWeight requires `searchable: true` — \
                 the weight only affects the generated `__fts` tsvector"
            )));
        }
        if !matches!(kind, "string" | "enum") {
            return Err(ValidationFailure(format!(
                "fields[{i}] (`{name}`): ftsWeight only applies to string/enum fields \
                 (got `{kind}`); non-text searchable fields are silently dropped from \
                 the tsvector"
            )));
        }
    }
    Ok(())
}

/// Walk `spec.fields[]` and check every `kind: ref` entry:
///
/// - The `ref` object must carry `org`, `app`, `domain`, `object`, `version`
///   (no partial pointers).
/// - In multi-tenant mode (ADR-010), `ref.org` must equal the schema's own
///   org — cross-tenant references are a data-leak vector and the webhook
///   is the last admission gate before the operator wires them up.
///
/// We deliberately stop at the static check: confirming the target schema
/// actually exists in the cluster would require live kube reads from the
/// webhook, which we leave to the operator's reconcile-time validation.
fn validate_field_refs(obj: &Value, self_org: &str, multi_tenant_mode: bool) -> ValidationResult {
    let Some(fields) = obj.pointer("/spec/fields").and_then(Value::as_array) else {
        return Ok(());
    };
    let self_org_kebab = kebab(self_org);

    for (i, f) in fields.iter().enumerate() {
        let kind = f.pointer("/type").and_then(Value::as_str).unwrap_or("");
        if kind != "ref" {
            continue;
        }
        let name = f.pointer("/name").and_then(Value::as_str).unwrap_or("<unnamed>");
        let Some(target) = f.pointer("/ref") else {
            return Err(ValidationFailure(format!(
                "fields[{i}] (`{name}`): kind=ref requires a `ref` block"
            )));
        };
        for k in ["org", "app", "domain", "object", "version"] {
            if target.pointer(&format!("/{k}")).and_then(Value::as_str).is_none() {
                return Err(ValidationFailure(format!(
                    "fields[{i}] (`{name}`): ref.{k} is required"
                )));
            }
        }
        if multi_tenant_mode {
            let target_org = target.pointer("/org").and_then(Value::as_str).unwrap_or("");
            if kebab(target_org) != self_org_kebab {
                return Err(ValidationFailure(format!(
                    "fields[{i}] (`{name}`): cross-org ref to `{target_org}` rejected — \
                     multi-tenant clusters do not permit cross-tenant references (ADR-010)"
                )));
            }
        }
    }
    Ok(())
}

/// Phase 0 CEL safety: length cap, depth cap, syntactic parse must succeed.
fn check_cel_safety(rule_obj: &Value, idx: usize) -> ValidationResult {
    let Some(rule_type) = rule_obj.pointer("/type").and_then(Value::as_str) else { return Ok(()) };
    if rule_type != "cel" {
        return Ok(());
    }

    let rule = rule_obj.pointer("/rule").and_then(Value::as_str).unwrap_or("");

    if rule.len() > MAX_CEL_LEN {
        return Err(ValidationFailure(format!(
            "validations[{idx}]: CEL rule exceeds {MAX_CEL_LEN} byte cap (got {} bytes)",
            rule.len()
        )));
    }
    let depth = cel_paren_depth(rule);
    if depth > MAX_CEL_DEPTH {
        return Err(ValidationFailure(format!(
            "validations[{idx}]: CEL rule nesting depth {depth} exceeds cap of {MAX_CEL_DEPTH}",
        )));
    }
    Program::compile(rule)
        .map_err(|e| ValidationFailure(format!("validations[{idx}]: CEL compile error: {e}")))?;
    Ok(())
}

/// Cheap proxy for AST depth — counts the maximum nesting of `(` and `[`. Not
/// perfect but good enough as a webhook-time guardrail; the runtime evaluator
/// imposes the real cap via `tokio::time::timeout` (ADR — CEL safety).
fn cel_paren_depth(rule: &str) -> usize {
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    for c in rule.chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth
                }
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    max_depth
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn domain_namespace_must_match() {
        let obj = json!({
            "metadata": { "labels": { "velocity.sh/org": "acme" }, "name": "procurement" },
            "spec": { "app": "supply-chain" }
        });
        assert!(validate_domain(&obj, "acme-supply-chain").is_ok());
        let err = validate_domain(&obj, "acme-supply").unwrap_err();
        assert!(err.0.contains("acme-supply-chain"));
    }

    #[test]
    fn domain_kebab_normalises_org_and_app() {
        let obj = json!({
            "metadata": { "labels": { "velocity.sh/org": "Acme_Corp" } },
            "spec": { "app": "Supply.Chain" }
        });
        assert!(validate_domain(&obj, "acme-corp-supply-chain").is_ok());
    }

    #[test]
    fn schema_definition_label_set_required() {
        let obj = json!({
            "metadata": { "labels": { "velocity.sh/org": "acme" } },
            "spec": {}
        });
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("velocity.sh/app"));
    }

    #[test]
    fn cel_safety_rejects_overlong_rule() {
        let obj = json!({
            "metadata": {
                "labels": {
                    "velocity.sh/org": "acme",
                    "velocity.sh/app": "supply-chain",
                    "velocity.sh/domain": "procurement",
                }
            },
            "spec": { "validations": [
                { "type": "cel", "rule": "x".repeat(20_000) }
            ]}
        });
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("byte cap"));
    }

    #[test]
    fn cel_safety_rejects_deep_nesting() {
        let mut deep = String::new();
        for _ in 0..15 {
            deep.push('(');
        }
        deep.push('1');
        for _ in 0..15 {
            deep.push(')');
        }
        let obj = json!({
            "metadata": {
                "labels": {
                    "velocity.sh/org": "acme",
                    "velocity.sh/app": "supply-chain",
                    "velocity.sh/domain": "procurement",
                }
            },
            "spec": { "validations": [
                { "type": "cel", "rule": deep }
            ]}
        });
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("nesting depth"));
    }

    #[test]
    fn cel_safety_accepts_simple_rule() {
        let obj = json!({
            "metadata": {
                "labels": {
                    "velocity.sh/org": "acme",
                    "velocity.sh/app": "supply-chain",
                    "velocity.sh/domain": "procurement",
                }
            },
            "spec": { "validations": [
                { "type": "cel", "rule": "self.amount > 0", "maxExecutionMs": 10 }
            ]}
        });
        assert!(validate_schema_definition(&obj, "acme-supply-chain-procurement", false).is_ok());
    }

    fn sd_with_field(field: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "metadata": {
                "labels": {
                    "velocity.sh/org": "acme",
                    "velocity.sh/app": "supply-chain",
                    "velocity.sh/domain": "procurement",
                }
            },
            "spec": { "fields": [field] }
        })
    }

    #[test]
    fn ref_missing_target_rejected() {
        let obj = sd_with_field(serde_json::json!({ "name": "supplier", "type": "ref" }));
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("requires a `ref` block"));
    }

    #[test]
    fn ref_missing_subfield_rejected() {
        let obj = sd_with_field(serde_json::json!({
            "name": "supplier",
            "type": "ref",
            "ref": { "org": "acme", "app": "supply-chain", "domain": "procurement" }
        }));
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("ref.object"));
    }

    #[test]
    fn cross_org_ref_allowed_in_single_tenant_mode() {
        let obj = sd_with_field(serde_json::json!({
            "name": "supplier",
            "type": "ref",
            "ref": {
                "org": "globex",
                "app": "supply-chain",
                "domain": "procurement",
                "object": "supplier",
                "version": "v1"
            }
        }));
        assert!(validate_schema_definition(&obj, "acme-supply-chain-procurement", false).is_ok());
    }

    #[test]
    fn cross_org_ref_rejected_in_multi_tenant_mode() {
        let obj = sd_with_field(serde_json::json!({
            "name": "supplier",
            "type": "ref",
            "ref": {
                "org": "globex",
                "app": "supply-chain",
                "domain": "procurement",
                "object": "supplier",
                "version": "v1"
            }
        }));
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", true).unwrap_err();
        assert!(err.0.contains("cross-org"));
        assert!(err.0.contains("ADR-010"));
    }

    #[test]
    fn same_org_ref_allowed_in_multi_tenant_mode() {
        let obj = sd_with_field(serde_json::json!({
            "name": "supplier",
            "type": "ref",
            "ref": {
                "org": "acme",
                "app": "logistics",
                "domain": "shipping",
                "object": "supplier",
                "version": "v1"
            }
        }));
        assert!(validate_schema_definition(&obj, "acme-supply-chain-procurement", true).is_ok());
    }

    fn schema_with_field(field: serde_json::Value) -> serde_json::Value {
        json!({
            "metadata": {
                "labels": {
                    "velocity.sh/org": "acme",
                    "velocity.sh/app": "supply-chain",
                    "velocity.sh/domain": "procurement",
                }
            },
            "spec": { "fields": [field] }
        })
    }

    #[test]
    fn fts_weight_accepted_on_searchable_string() {
        let obj = schema_with_field(json!({
            "name": "title",
            "type": "string",
            "searchable": true,
            "ftsWeight": "A"
        }));
        assert!(validate_schema_definition(&obj, "acme-supply-chain-procurement", false).is_ok());
    }

    #[test]
    fn fts_weight_rejected_on_non_searchable_field() {
        let obj = schema_with_field(json!({
            "name": "title",
            "type": "string",
            "ftsWeight": "A"
        }));
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("searchable: true"));
    }

    #[test]
    fn fts_weight_rejected_on_non_text_field() {
        let obj = schema_with_field(json!({
            "name": "amount",
            "type": "number",
            "searchable": true,
            "ftsWeight": "B"
        }));
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("string/enum"));
    }

    #[test]
    fn fts_weight_rejected_when_invalid() {
        let obj = schema_with_field(json!({
            "name": "title",
            "type": "string",
            "searchable": true,
            "ftsWeight": "Z"
        }));
        let err =
            validate_schema_definition(&obj, "acme-supply-chain-procurement", false).unwrap_err();
        assert!(err.0.contains("A, B, C, D"));
    }
}
