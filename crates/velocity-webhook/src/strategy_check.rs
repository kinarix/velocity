//! `AuthStrategy` existence check used by the SchemaDefinition validator.
//!
//! ## Why a trait
//!
//! Tests must run without a kube cluster, so the existence check is gated
//! behind a trait. Production wires [`KubeStrategyChecker`]; tests use
//! [`MockStrategyChecker`].
//!
//! ## Why the webhook does this at all
//!
//! `SchemaDefinition.spec.auth.strategyRef` is dereferenced by the API
//! server's auth middleware on every request. If the referenced
//! `AuthStrategy` does not exist when traffic hits, the API returns
//! `AUTH_STRATEGY_MISSING` (500) — a 5xx that should never happen on a
//! correctly-applied CRD. Catching the typo at admission means the
//! operator never reconciles a schema it can't serve, and the API never
//! returns 500 for the trivially-foreseeable cause.
//!
//! ## What we *don't* check
//!
//! Issuer reachability, JWKS validity, claim mapping correctness. Those
//! belong to the operator's reconciler — the webhook is the namespace+
//! existence gate, and live network reads from an admission webhook are
//! a recipe for cluster-wide stalls when the IdP is slow.

use async_trait::async_trait;
use kube::api::Api;
use kube::Client;
use serde_json::Value;
use velocity_types::crds::AuthStrategy;

use crate::validators::ValidationFailure;

/// Existence check abstraction. Implementations only have to answer
/// "does this `AuthStrategy` exist in this namespace?" — they don't read
/// the spec, don't validate it, and don't care about status.
#[async_trait]
pub trait AuthStrategyExists: Send + Sync {
    async fn exists(&self, namespace: &str, name: &str) -> Result<bool, String>;
}

/// Production checker — issues a `GET` against the kube API. We log but
/// don't classify the kube error type here: anything other than 404 is
/// surfaced as `Err`, the caller decides whether to fail open or closed.
#[derive(Clone)]
pub struct KubeStrategyChecker {
    client: Client,
}

impl std::fmt::Debug for KubeStrategyChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubeStrategyChecker").finish_non_exhaustive()
    }
}

impl KubeStrategyChecker {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl AuthStrategyExists for KubeStrategyChecker {
    async fn exists(&self, namespace: &str, name: &str) -> Result<bool, String> {
        let api: Api<AuthStrategy> = Api::namespaced(self.client.clone(), namespace);
        match api.get_opt(name).await {
            Ok(opt) => Ok(opt.is_some()),
            Err(e) => Err(format!("kube get AuthStrategy {namespace}/{name}: {e}")),
        }
    }
}

/// Test fixture — explicit allow-list of `(namespace, name)` pairs.
#[derive(Debug, Default, Clone)]
pub struct MockStrategyChecker {
    pub present: Vec<(String, String)>,
    pub fail: bool,
}

impl MockStrategyChecker {
    pub fn with(strategies: Vec<(&str, &str)>) -> Self {
        Self {
            present: strategies.into_iter().map(|(ns, n)| (ns.into(), n.into())).collect(),
            fail: false,
        }
    }

    /// Force the checker to return `Err` — used to exercise the
    /// fail-closed branch of the admission gate.
    pub fn failing() -> Self {
        Self { present: Vec::new(), fail: true }
    }
}

#[async_trait]
impl AuthStrategyExists for MockStrategyChecker {
    async fn exists(&self, namespace: &str, name: &str) -> Result<bool, String> {
        if self.fail {
            return Err("mock configured to fail".into());
        }
        Ok(self.present.iter().any(|(ns, n)| ns == namespace && n == name))
    }
}

/// Extract `spec.auth.strategyRef` and check the target exists. Errors
/// mean either: (1) the field is missing/malformed (denied), or (2) the
/// referenced strategy does not exist (denied), or (3) the kube backend
/// could not be queried (denied — fail-closed by design; an admission
/// gate that silently admits when it can't check is no gate at all).
pub async fn validate_auth_strategy_ref(
    obj: &Value,
    self_namespace: &str,
    checker: &dyn AuthStrategyExists,
) -> Result<(), ValidationFailure> {
    // `spec.auth.strategyRef` is required for every SchemaDefinition that
    // wants the API to serve it. Absent => deny with a clear message
    // rather than a surprising 500 at first request.
    let strategy_ref = obj.pointer("/spec/auth/strategyRef").ok_or_else(|| {
        ValidationFailure(
            "SchemaDefinition.spec.auth.strategyRef is required — name and namespace of the \
             AuthStrategy the API should use for this schema"
                .into(),
        )
    })?;

    let name = strategy_ref
        .pointer("/name")
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationFailure("spec.auth.strategyRef.name is required".into()))?;

    // `namespace` is optional — if omitted, the strategy lives in the same
    // namespace as the SchemaDefinition. Matches the kube convention for
    // cross-namespace refs (and avoids forcing every CRD to repeat itself).
    let namespace =
        strategy_ref.pointer("/namespace").and_then(Value::as_str).unwrap_or(self_namespace);

    match checker.exists(namespace, name).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(ValidationFailure(format!(
            "AuthStrategy `{namespace}/{name}` not found — apply the strategy CRD before this \
             SchemaDefinition (or fix the strategyRef typo)"
        ))),
        Err(detail) => Err(ValidationFailure(format!(
            "could not verify AuthStrategy `{namespace}/{name}` exists ({detail}) — refusing \
             to admit a SchemaDefinition the API may not be able to serve"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sd(strategy_ref: Value) -> Value {
        json!({
            "metadata": { "namespace": "acme-supply-chain-procurement" },
            "spec": { "auth": { "strategyRef": strategy_ref } }
        })
    }

    #[tokio::test]
    async fn missing_strategy_ref_block_is_rejected() {
        let obj = json!({ "metadata": {}, "spec": { "auth": {} } });
        let mock = MockStrategyChecker::default();
        let err = validate_auth_strategy_ref(&obj, "acme-platform", &mock).await.unwrap_err();
        assert!(err.0.contains("strategyRef is required"));
    }

    #[tokio::test]
    async fn missing_name_is_rejected() {
        let obj = sd(json!({ "namespace": "acme-platform" }));
        let mock = MockStrategyChecker::default();
        let err = validate_auth_strategy_ref(&obj, "acme-platform", &mock).await.unwrap_err();
        assert!(err.0.contains("strategyRef.name"));
    }

    #[tokio::test]
    async fn missing_strategy_in_cluster_is_rejected() {
        let obj = sd(json!({ "name": "default", "namespace": "acme-platform" }));
        let mock = MockStrategyChecker::with(vec![("acme-platform", "other")]);
        let err = validate_auth_strategy_ref(&obj, "acme-platform", &mock).await.unwrap_err();
        assert!(err.0.contains("`acme-platform/default` not found"));
    }

    #[tokio::test]
    async fn present_strategy_is_admitted() {
        let obj = sd(json!({ "name": "default", "namespace": "acme-platform" }));
        let mock = MockStrategyChecker::with(vec![("acme-platform", "default")]);
        assert!(validate_auth_strategy_ref(&obj, "acme-platform", &mock).await.is_ok());
    }

    #[tokio::test]
    async fn namespace_defaults_to_self() {
        // No `namespace:` on the ref — checker should be queried with the
        // SchemaDefinition's own namespace.
        let obj = sd(json!({ "name": "default" }));
        let mock = MockStrategyChecker::with(vec![("acme-supply-chain-procurement", "default")]);
        assert!(validate_auth_strategy_ref(&obj, "acme-supply-chain-procurement", &mock)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn kube_failure_denies_admission() {
        // Fail-closed: if we can't query, we don't admit. A webhook that
        // silently admits on backend failure is a worse safety regression
        // than a transient deploy stall.
        let obj = sd(json!({ "name": "default", "namespace": "acme-platform" }));
        let mock = MockStrategyChecker::failing();
        let err = validate_auth_strategy_ref(&obj, "acme-platform", &mock).await.unwrap_err();
        assert!(err.0.contains("could not verify"));
        assert!(err.0.contains("refusing to admit"));
    }
}
