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

/// SchemaDefinition: namespace == `{org}-{app}-{domain}`; CEL rules pass basic safety.
pub fn validate_schema_definition(obj: &Value, namespace: &str) -> ValidationResult {
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
        let err = validate_schema_definition(&obj, "acme-supply-chain-procurement").unwrap_err();
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
        let err = validate_schema_definition(&obj, "acme-supply-chain-procurement").unwrap_err();
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
        let err = validate_schema_definition(&obj, "acme-supply-chain-procurement").unwrap_err();
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
        assert!(validate_schema_definition(&obj, "acme-supply-chain-procurement").is_ok());
    }
}
