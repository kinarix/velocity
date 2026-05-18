//! Layer 2 — ABAC (attribute-based access control) via CEL.
//!
//! Spec at `SchemaDefinition.spec.access.policies[]`. Each entry is a
//! CEL predicate that must evaluate to true for the request to proceed:
//!
//! ```yaml
//! access:
//!   policies:
//!     - name: budget-cap
//!       action: create
//!       fields: [total]            # optional — only fire if these fields appear
//!       condition: "self.total <= identity.attributes.budget_limit"
//!       message: "PO total exceeds your budget"
//! ```
//!
//! ## Why a separate compile path
//!
//! [`crate::validate::CompiledRule`] already covers schema-level
//! `validations[]`. Those are data-shape rules ("amount > 0"); ABAC
//! policies are access rules ("this actor may write this row"). They
//! share the CEL evaluator and the 10ms timeout (ADR — CEL safety) but
//! diverge in three ways the type system makes load-bearing:
//!
//! 1. **Context**: policies see `identity` and `request`, not just `self`.
//! 2. **Failure mode**: a violation returns `ApiError::PolicyDenied` (403),
//!    not `BadRequest` (400) — a refused payload is *valid input the actor
//!    isn't allowed to submit*, distinct from "this JSON is malformed".
//! 3. **Scope**: policies are filtered by op (`action`) and optionally by
//!    which fields appeared in the payload (`fields`), so the hot path is
//!    "first applicable policy" rather than "every rule, always".
//!
//! ## Read ops
//!
//! Phase 2b deliberately leaves `action: read` to Layer 4 (row filter).
//! A row-level predicate that runs per result is the wrong tool for
//! "may this actor read row X" — that's a `WHERE` clause, not a
//! handler-entry check. We log if a CRD declares `action: read` so the
//! operator can spot the misconfiguration.

use std::sync::Arc;
use std::time::Duration;

use cel_interpreter::{Context as CelContext, Program};
use serde_json::{json, Value};
use velocity_types::crds::schema::AbacPolicy;

use crate::error::ApiError;
use crate::identity::Identity;
use crate::validate::CEL_MAX_MS;

/// A policy that's been turned into a runnable form at resolve time.
/// `Broken` means the rule failed to compile — fail-closed so the
/// admin notices, not a silent admit.
#[derive(Debug)]
pub enum CompiledPolicy {
    Ok {
        name: String,
        action: PolicyAction,
        /// If non-empty, only evaluate when the write payload mentions at
        /// least one of these field names. The empty set means "always".
        field_filter: Vec<String>,
        program: Arc<Program>,
        message: String,
    },
    Broken {
        name: String,
        action: PolicyAction,
        reason: String,
        message: String,
    },
}

/// Subset of operation strings policies may target. `read` is deliberately
/// absent — see module docs. `Any` is the wildcard used when the CRD
/// declares `action: "*"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAction {
    Create,
    Update,
    Delete,
    /// Wildcard — applies to every write op. Use sparingly; most policies
    /// have a natural single op they belong to.
    Any,
}

impl PolicyAction {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "create" => Self::Create,
            "update" => Self::Update,
            "delete" => Self::Delete,
            "*" | "any" | "write" => Self::Any,
            _ => return None,
        })
    }

    /// Does this policy apply to a request with op `op`? `op` uses the
    /// canonical lowercase strings from [`crate::rbac::op`].
    pub fn matches(&self, op: &str) -> bool {
        match self {
            Self::Any => matches!(op, "create" | "update" | "delete"),
            Self::Create => op == "create",
            Self::Update => op == "update",
            Self::Delete => op == "delete",
        }
    }
}

/// Compile every policy on a schema. Read-action policies are logged and
/// dropped — the CRD is technically valid (CEL parses), but the policy
/// would never fire under Phase 2b's write-only evaluation point.
pub fn compile_policies(policies: &[AbacPolicy]) -> Vec<CompiledPolicy> {
    policies.iter().filter_map(compile_one).collect()
}

fn compile_one(p: &AbacPolicy) -> Option<CompiledPolicy> {
    let message = p.message.clone().unwrap_or_else(|| format!("policy `{}` denied", p.name));

    let action = match PolicyAction::parse(&p.action) {
        Some(a) => a,
        None if p.action.eq_ignore_ascii_case("read") => {
            tracing::warn!(
                policy = %p.name,
                "ABAC policy with `action: read` is ignored — row-level reads are Layer 4 (row filter), not handler-entry policy",
            );
            return None;
        }
        None => {
            return Some(CompiledPolicy::Broken {
                name: p.name.clone(),
                action: PolicyAction::Any,
                reason: format!("unknown action `{}`", p.action),
                message,
            });
        }
    };

    match Program::compile(&p.condition) {
        Ok(program) => Some(CompiledPolicy::Ok {
            name: p.name.clone(),
            action,
            field_filter: p.fields.clone(),
            program: Arc::new(program),
            message,
        }),
        Err(e) => Some(CompiledPolicy::Broken {
            name: p.name.clone(),
            action,
            reason: format!("CEL compile error: {e}"),
            message,
        }),
    }
}

/// Run every applicable policy for `op`. Stops at the first denial.
///
/// `payload` is the incoming JSON object for writes; pass `Value::Null`
/// for delete (the policy can still reference `identity` / `request`).
pub async fn evaluate_for(
    policies: &Arc<Vec<CompiledPolicy>>,
    op: &str,
    payload: &Value,
    identity: &Identity,
) -> Result<(), ApiError> {
    if policies.is_empty() {
        return Ok(());
    }

    let identity_ctx = identity_context(identity);
    let request_ctx = json!({ "action": op });

    for policy in policies.iter() {
        match policy {
            CompiledPolicy::Broken { name, message, reason, .. } => {
                // Schema is in an unsafe state. Fail-closed — refusing a
                // request the operator can't reason about is the safer
                // default than admitting it.
                tracing::error!(
                    policy = %name,
                    reason = %reason,
                    "fail-closed: ABAC policy is broken",
                );
                return Err(ApiError::Internal(format!(
                    "ABAC policy `{name}` is broken: {message}"
                )));
            }
            CompiledPolicy::Ok { name, action, field_filter, program, message } => {
                if !action.matches(op) {
                    continue;
                }
                if !field_filter_applies(field_filter, payload) {
                    continue;
                }

                let program = Arc::clone(program);
                let payload_owned = payload.clone();
                let identity_owned = identity_ctx.clone();
                let request_owned = request_ctx.clone();
                let policy_name = name.clone();

                let outcome = tokio::time::timeout(
                    Duration::from_millis(CEL_MAX_MS),
                    tokio::task::spawn_blocking(move || {
                        let mut ctx = CelContext::default();
                        ctx.add_variable("self", payload_owned)
                            .map_err(|e| format!("CEL context: {e}"))?;
                        ctx.add_variable("identity", identity_owned)
                            .map_err(|e| format!("CEL context: {e}"))?;
                        ctx.add_variable("request", request_owned)
                            .map_err(|e| format!("CEL context: {e}"))?;
                        program.execute(&ctx).map_err(|e| format!("{e}"))
                    }),
                )
                .await;

                match outcome {
                    Err(_) => {
                        tracing::warn!(
                            policy = %policy_name,
                            actor = %identity.actor_id,
                            "ABAC policy timed out — denying",
                        );
                        return Err(ApiError::PolicyDenied(format!(
                            "policy `{policy_name}` timed out (>{CEL_MAX_MS}ms)"
                        )));
                    }
                    Ok(Err(join_err)) => {
                        return Err(ApiError::Internal(format!(
                            "ABAC CEL evaluator panicked: {join_err}"
                        )));
                    }
                    Ok(Ok(Err(eval_err))) => {
                        // Evaluator hit a runtime error (e.g. type mismatch
                        // because the payload was missing a field the policy
                        // referenced). Fail-closed.
                        tracing::warn!(
                            policy = %policy_name,
                            actor = %identity.actor_id,
                            error = %eval_err,
                            "ABAC policy raised — denying",
                        );
                        return Err(ApiError::PolicyDenied(message.clone()));
                    }
                    Ok(Ok(Ok(v))) => {
                        if !truthy(&v) {
                            tracing::info!(
                                policy = %policy_name,
                                actor = %identity.actor_id,
                                op = %op,
                                "ABAC policy denied",
                            );
                            return Err(ApiError::PolicyDenied(message.clone()));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Build the `identity` variable passed to CEL. We expose only the fields
/// a policy is supposed to read — keeping the CRD-author's surface small
/// stops a future Identity field (e.g. raw bearer token bytes) from
/// silently leaking into policy expressions.
fn identity_context(identity: &Identity) -> Value {
    json!({
        "actor_id": identity.actor_id,
        "roles": identity.roles,
        "attributes": identity.attributes,
        "email": identity.email,
        "issuer": identity.issuer,
        "strategy": identity.strategy,
    })
}

/// Field-filter semantics: if the policy declared `fields: [a, b]`, only
/// fire when the payload contains at least one of them. Useful for
/// write-time policies that only care about specific columns (e.g. a
/// "you can only set `status` to certain values" rule shouldn't fire
/// when the update doesn't touch `status`).
fn field_filter_applies(filter: &[String], payload: &Value) -> bool {
    if filter.is_empty() {
        return true;
    }
    let Some(obj) = payload.as_object() else {
        // No payload shape to inspect (e.g. delete) — be safe and run.
        return true;
    };
    filter.iter().any(|f| obj.contains_key(f))
}

fn truthy(v: &cel_interpreter::Value) -> bool {
    use cel_interpreter::Value as V;
    match v {
        V::Bool(b) => *b,
        V::Null => false,
        V::Int(i) => *i != 0,
        V::UInt(u) => *u != 0,
        V::Float(f) => *f != 0.0,
        V::String(s) => !s.is_empty(),
        V::List(l) => !l.is_empty(),
        V::Map(m) => !m.map.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn policy(name: &str, action: &str, condition: &str) -> AbacPolicy {
        AbacPolicy {
            name: name.into(),
            action: action.into(),
            fields: Vec::new(),
            condition: condition.into(),
            message: None,
        }
    }

    fn ident(actor: &str, roles: &[&str], attrs: &[(&str, &str)]) -> Identity {
        let attributes: HashMap<String, String> =
            attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        Identity {
            actor_id: actor.into(),
            roles: roles.iter().map(|s| (*s).into()).collect(),
            attributes,
            strategy: "acme-platform/default".into(),
            ..Identity::default()
        }
    }

    #[test]
    fn action_parse_canonical_and_wildcard() {
        assert_eq!(PolicyAction::parse("create"), Some(PolicyAction::Create));
        assert_eq!(PolicyAction::parse("UPDATE"), Some(PolicyAction::Update));
        assert_eq!(PolicyAction::parse("*"), Some(PolicyAction::Any));
        assert_eq!(PolicyAction::parse("write"), Some(PolicyAction::Any));
        assert_eq!(PolicyAction::parse("garbage"), None);
    }

    #[test]
    fn read_action_is_dropped_with_warning() {
        // Layer 4 covers reads via row filter — a policy targeting read is
        // a config smell, drop it and the warning log nudges the author.
        let policies = compile_policies(&[policy("p1", "read", "true")]);
        assert!(policies.is_empty());
    }

    #[test]
    fn unknown_action_compiles_to_broken() {
        let policies = compile_policies(&[policy("p1", "explode", "true")]);
        assert_eq!(policies.len(), 1);
        assert!(matches!(&policies[0], CompiledPolicy::Broken { .. }));
    }

    #[test]
    fn cel_compile_failure_is_broken() {
        let policies = compile_policies(&[policy("p1", "create", "this is not CEL )))")]);
        assert!(matches!(&policies[0], CompiledPolicy::Broken { .. }));
    }

    #[tokio::test]
    async fn empty_policy_list_admits() {
        let policies = Arc::new(Vec::new());
        let id = ident("alice", &[], &[]);
        assert!(evaluate_for(&policies, "create", &json!({}), &id).await.is_ok());
    }

    #[tokio::test]
    async fn true_policy_admits() {
        let policies = Arc::new(compile_policies(&[policy("p1", "create", "self.total > 0")]));
        let id = ident("alice", &[], &[]);
        let payload = json!({ "total": 5 });
        assert!(evaluate_for(&policies, "create", &payload, &id).await.is_ok());
    }

    #[tokio::test]
    async fn false_policy_denies_with_message() {
        let mut p = policy("budget-cap", "create", "self.total <= 100");
        p.message = Some("over budget".into());
        let policies = Arc::new(compile_policies(&[p]));
        let id = ident("alice", &[], &[]);
        let payload = json!({ "total": 500 });
        let err = evaluate_for(&policies, "create", &payload, &id).await.unwrap_err();
        assert!(matches!(err, ApiError::PolicyDenied(_)));
        assert_eq!(err.to_string(), "access denied: over budget");
    }

    #[tokio::test]
    async fn policy_only_applies_to_matching_action() {
        let policies = Arc::new(compile_policies(&[policy("p1", "delete", "false")]));
        let id = ident("alice", &[], &[]);
        // Same false condition would deny a delete; we're doing a create,
        // so it doesn't fire. Important — without action gating, every
        // write would pay the cost of evaluating every policy.
        assert!(evaluate_for(&policies, "create", &json!({}), &id).await.is_ok());
        let err = evaluate_for(&policies, "delete", &Value::Null, &id).await.unwrap_err();
        assert!(matches!(err, ApiError::PolicyDenied(_)));
    }

    #[tokio::test]
    async fn wildcard_action_fires_on_every_write() {
        let policies = Arc::new(compile_policies(&[policy("p1", "*", "false")]));
        let id = ident("alice", &[], &[]);
        for op in ["create", "update", "delete"] {
            let err = evaluate_for(&policies, op, &json!({}), &id).await.unwrap_err();
            assert!(matches!(err, ApiError::PolicyDenied(_)));
        }
    }

    #[tokio::test]
    async fn identity_attributes_are_visible_to_cel() {
        let policies = Arc::new(compile_policies(&[policy(
            "tenant-match",
            "create",
            "self.tenant == identity.attributes.tenant_id",
        )]));
        let id = ident("alice", &[], &[("tenant_id", "t-42")]);

        let ok = json!({ "tenant": "t-42" });
        assert!(evaluate_for(&policies, "create", &ok, &id).await.is_ok());

        let mismatch = json!({ "tenant": "t-99" });
        let err = evaluate_for(&policies, "create", &mismatch, &id).await.unwrap_err();
        assert!(matches!(err, ApiError::PolicyDenied(_)));
    }

    #[tokio::test]
    async fn field_filter_skips_policy_when_no_listed_field_present() {
        let mut p = policy("status-rule", "update", "self.status == 'approved'");
        p.fields = vec!["status".into()];
        let policies = Arc::new(compile_policies(&[p]));
        let id = ident("alice", &[], &[]);

        // Payload doesn't touch `status` — policy should not fire, even
        // though it would otherwise deny.
        let untouched = json!({ "notes": "hello" });
        assert!(evaluate_for(&policies, "update", &untouched, &id).await.is_ok());

        // Payload touches `status` and sets it to a denied value — fires.
        let denied = json!({ "status": "draft" });
        let err = evaluate_for(&policies, "update", &denied, &id).await.unwrap_err();
        assert!(matches!(err, ApiError::PolicyDenied(_)));
    }

    #[tokio::test]
    async fn broken_policy_returns_internal_error_not_admit() {
        // Schema-author error must be loud, not silent. The handler should
        // see Internal (500) so logs/alerts fire; admitting the request
        // would defeat the point of having a policy.
        let policies = Arc::new(compile_policies(&[policy("p1", "create", "garbage )))")]));
        let id = ident("alice", &[], &[]);
        let err = evaluate_for(&policies, "create", &json!({}), &id).await.unwrap_err();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn runtime_eval_error_denies() {
        // Policy references self.amount; payload has no amount. CEL raises
        // a runtime error → fail-closed deny.
        let policies = Arc::new(compile_policies(&[policy("p1", "create", "self.amount > 0")]));
        let id = ident("alice", &[], &[]);
        let err = evaluate_for(&policies, "create", &json!({}), &id).await.unwrap_err();
        assert!(matches!(err, ApiError::PolicyDenied(_)));
    }
}
