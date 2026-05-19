//! AdmissionReview handler — routes by `request.kind.kind` to the matching validator.

use std::sync::Arc;

use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use kube::core::DynamicObject;
use serde_json::Value;
use tracing::Instrument;

use crate::strategy_check::{validate_auth_strategy_ref, AuthStrategyExists};
use crate::validators::{self, ValidationFailure};
use crate::WebhookConfig;

/// Shared per-process state. Holds the static config plus the dynamic
/// checker the SchemaDefinition validator uses to verify
/// `spec.auth.strategyRef` exists in the cluster.
///
/// `checker` is a trait object so tests can substitute a deterministic
/// mock — see [`crate::strategy_check::MockStrategyChecker`].
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<WebhookConfig>,
    pub checker: Arc<dyn AuthStrategyExists>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").field("cfg", &self.cfg).field("checker", &"<dyn>").finish()
    }
}

impl AppState {
    pub fn new(cfg: WebhookConfig, checker: Arc<dyn AuthStrategyExists>) -> Self {
        Self { cfg: Arc::new(cfg), checker }
    }
}

/// `POST /validate` handler.
pub async fn validate(
    State(state): State<AppState>,
    Json(review): Json<AdmissionReview<DynamicObject>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let span = tracing::info_span!(
        "admission",
        kind = tracing::field::Empty,
        namespace = tracing::field::Empty,
        name = tracing::field::Empty,
        uid = tracing::field::Empty,
    );

    let req: AdmissionRequest<DynamicObject> =
        review.try_into().map_err(|e: kube::core::admission::ConvertAdmissionReviewError| {
            (StatusCode::BAD_REQUEST, format!("invalid AdmissionReview: {e}"))
        })?;

    span.record("kind", req.kind.kind.as_str());
    span.record("namespace", req.namespace.as_deref().unwrap_or(""));
    span.record("name", req.name.as_str());
    span.record("uid", req.uid.as_str());

    async move {
        let response = match decide(&req, &state).await {
            Ok(()) => {
                tracing::info!("admit");
                AdmissionResponse::from(&req)
            }
            Err(failure) => {
                tracing::warn!(reason = %failure.0, "deny");
                AdmissionResponse::from(&req).deny(failure.0)
            }
        };
        let review: AdmissionReview<DynamicObject> = response.into_review();
        Ok::<_, (StatusCode, String)>(Json(review))
    }
    .instrument(span)
    .await
}

async fn decide(
    req: &AdmissionRequest<DynamicObject>,
    state: &AppState,
) -> Result<(), ValidationFailure> {
    let Some(obj) = req.object.as_ref() else {
        // DELETE has no object — admit (nothing to validate).
        return Ok(());
    };
    let ns = req.namespace.as_deref().unwrap_or("");
    let value: Value = serde_json::to_value(obj)
        .map_err(|e| ValidationFailure(format!("failed to serialize object: {e}")))?;

    match req.kind.kind.as_str() {
        "Domain" => validators::validate_domain(&value, ns),
        "Application" => validators::validate_application(&value, ns),
        "SchemaDefinition" => {
            // Order matters: cheap sync checks first (label/namespace,
            // field shape, CEL), then the async kube lookup. A
            // SchemaDefinition with a malformed namespace shouldn't burn
            // an apiserver round-trip just to be rejected.
            validators::validate_schema_definition(&value, ns, state.cfg.multi_tenant_mode)?;
            validate_auth_strategy_ref(&value, ns, state.checker.as_ref()).await
        }
        // Organisation lives in `platform` — no namespace rule yet.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        app_with(
            WebhookConfig {
                tls_addr: String::new(),
                health_addr: String::new(),
                tls_cert_path: None,
                tls_key_path: None,
                pretty_logs: false,
                multi_tenant_mode: false,
            },
            // Tests in this module only drive Domain/Application paths, so
            // an empty allow-list is fine. The strategy_check.rs and
            // handler_strategy.rs tests cover SchemaDefinition cases.
            Arc::new(crate::strategy_check::MockStrategyChecker::default()),
        )
    }

    fn app_with(
        cfg: WebhookConfig,
        checker: Arc<dyn crate::strategy_check::AuthStrategyExists>,
    ) -> axum::Router {
        let state = AppState::new(cfg, checker);
        axum::Router::new().route("/validate", axum::routing::post(validate)).with_state(state)
    }

    fn review(kind: &str, namespace: &str, obj: Value) -> Value {
        json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-1",
                "kind": { "group": "velocity.sh", "version": "v1", "kind": kind },
                "resource": { "group": "velocity.sh", "version": "v1", "resource": format!("{}s", kind.to_lowercase()) },
                "name": "test",
                "namespace": namespace,
                "operation": "CREATE",
                "userInfo": { "username": "tester" },
                "object": obj,
                "dryRun": false,
            }
        })
    }

    async fn run(body: Value) -> Value {
        let app = app();
        let req = Request::builder()
            .method("POST")
            .uri("/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn allows_valid_domain() {
        let body = review(
            "Domain",
            "acme-supply-chain",
            json!({
                "apiVersion": "velocity.sh/v1",
                "kind": "Domain",
                "metadata": {
                    "name": "procurement",
                    "labels": { "velocity.sh/org": "acme" },
                },
                "spec": { "app": "supply-chain", "displayName": "Procurement",
                          "access": { "defaultRole": "r", "adminRole": "a" } }
            }),
        );
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], true);
    }

    /// Drive a SchemaDefinition admission through the real handler with a
    /// `MockStrategyChecker` so we cover the end-to-end wire-up: the
    /// validators run, then the async existence check runs, then the
    /// AdmissionReview comes back denied when the strategy is missing.
    async fn run_sd(body: Value, allow: Vec<(&str, &str)>) -> Value {
        let app = app_with(
            WebhookConfig {
                tls_addr: String::new(),
                health_addr: String::new(),
                tls_cert_path: None,
                tls_key_path: None,
                pretty_logs: false,
                multi_tenant_mode: false,
            },
            Arc::new(crate::strategy_check::MockStrategyChecker::with(allow)),
        );
        let req = Request::builder()
            .method("POST")
            .uri("/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn sd_review(strategy_ref: Value) -> Value {
        review(
            "SchemaDefinition",
            "acme-supply-chain-procurement",
            json!({
                "metadata": {
                    "namespace": "acme-supply-chain-procurement",
                    "labels": {
                        "velocity.sh/org": "acme",
                        "velocity.sh/app": "supply-chain",
                        "velocity.sh/domain": "procurement",
                    },
                },
                "spec": { "auth": { "strategyRef": strategy_ref } }
            }),
        )
    }

    #[tokio::test]
    async fn schemadefinition_denied_when_strategy_missing() {
        let body = sd_review(json!({ "name": "default", "namespace": "acme-platform" }));
        let resp = run_sd(body, vec![]).await;
        assert_eq!(resp["response"]["allowed"], false);
        assert!(resp["response"]["status"]["message"]
            .as_str()
            .unwrap()
            .contains("AuthStrategy `acme-platform/default` not found"));
    }

    #[tokio::test]
    async fn schemadefinition_admitted_when_strategy_present() {
        let body = sd_review(json!({ "name": "default", "namespace": "acme-platform" }));
        let resp = run_sd(body, vec![("acme-platform", "default")]).await;
        assert_eq!(resp["response"]["allowed"], true);
    }

    #[tokio::test]
    async fn appstate_debug_redacts_checker_trait_object() {
        let state = AppState::new(
            WebhookConfig {
                tls_addr: String::new(),
                health_addr: String::new(),
                tls_cert_path: None,
                tls_key_path: None,
                pretty_logs: false,
                multi_tenant_mode: false,
            },
            Arc::new(crate::strategy_check::MockStrategyChecker::default()),
        );
        let dbg = format!("{state:?}");
        assert!(dbg.contains("AppState"));
        assert!(dbg.contains("<dyn>"), "trait object should be redacted: {dbg}");
    }

    #[tokio::test]
    async fn admission_review_without_object_admits() {
        // DELETE has no `object` — the decide() short-circuits to Ok(()).
        let body = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-2",
                "kind": { "group": "velocity.sh", "version": "v1", "kind": "Domain" },
                "resource": { "group": "velocity.sh", "version": "v1", "resource": "domains" },
                "name": "test",
                "namespace": "ns",
                "operation": "DELETE",
                "userInfo": { "username": "tester" },
                "dryRun": false,
            }
        });
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], true);
    }

    #[tokio::test]
    async fn unknown_kind_is_admitted() {
        // Triggers the `_ => Ok(())` fall-through.
        let body = review("Organisation", "platform", json!({ "metadata": { "name": "x" } }));
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], true);
    }

    #[tokio::test]
    async fn application_kind_runs_app_validator() {
        // Triggers the Application arm. Use an invalid payload so the
        // validator returns a deny — that is sufficient to prove the
        // dispatch arm was taken (otherwise we'd hit the catch-all).
        let body = review(
            "Application",
            "wrong-namespace",
            json!({
                "metadata": { "labels": { "velocity.sh/org": "acme" } },
                "spec": {}
            }),
        );
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], false);
    }

    #[tokio::test]
    async fn malformed_admission_review_returns_bad_request() {
        // No `request` field — TryInto<AdmissionRequest> fails, mapped
        // to a 400. Covers the error arm on line 55-57.
        let body = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
        });
        let app = app();
        let req = Request::builder()
            .method("POST")
            .uri("/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn denies_namespace_mismatch() {
        let body = review(
            "Domain",
            "acme-supply",
            json!({
                "metadata": { "labels": { "velocity.sh/org": "acme" } },
                "spec": { "app": "supply-chain" }
            }),
        );
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], false);
        let msg = resp["response"]["status"]["message"].as_str().unwrap();
        assert!(msg.contains("acme-supply-chain"));
    }
}
