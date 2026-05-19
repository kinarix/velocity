//! Library-side wire-up for the webhook binary.
//!
//! Builds the two axum routers (`/validate` + `/healthz`) from a
//! `WebhookConfig` plus an injected `AuthStrategyExists` checker.
//! Pure construction — no socket binds, no rustls install, no env
//! reads. Tests drive these with the in-process `MockStrategyChecker`.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;

use crate::handler;
use crate::strategy_check::AuthStrategyExists;
use crate::WebhookConfig;

/// Maximum admission body the webhook will accept. The apiserver
/// itself bounds AdmissionReview at ~10 MiB; mirroring the cap here
/// stops an attacker who reaches us directly (bypassing the
/// apiserver) from exhausting memory.
const ADMISSION_BODY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

/// Build the admission router (`POST /validate` + `/healthz`).
pub fn build_admission_router(cfg: WebhookConfig, checker: Arc<dyn AuthStrategyExists>) -> Router {
    let state = handler::AppState::new(cfg, checker);
    Router::new()
        .route("/validate", post(handler::validate))
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .layer(DefaultBodyLimit::max(ADMISSION_BODY_LIMIT_BYTES))
        .with_state(state)
}

/// Build the standalone health router served on a separate port so
/// probes survive a saturated admission listener.
pub fn build_health_router() -> Router {
    Router::new().route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy_check::MockStrategyChecker;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn cfg() -> WebhookConfig {
        WebhookConfig {
            tls_addr: "0.0.0.0:8443".into(),
            health_addr: "0.0.0.0:8080".into(),
            tls_cert_path: None,
            tls_key_path: None,
            pretty_logs: false,
            multi_tenant_mode: false,
        }
    }

    #[tokio::test]
    async fn admission_router_serves_health_endpoint() {
        let app = build_admission_router(cfg(), Arc::new(MockStrategyChecker::default()));
        let resp = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admission_router_routes_validate_method_allow() {
        // Wrong-method against /validate returns 405, confirming the
        // route is registered as POST and not, say, GET.
        let app = build_admission_router(cfg(), Arc::new(MockStrategyChecker::default()));
        let resp = app
            .oneshot(Request::builder().uri("/validate").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn admission_router_enforces_body_limit() {
        let app = build_admission_router(cfg(), Arc::new(MockStrategyChecker::default()));
        let big = vec![b'x'; ADMISSION_BODY_LIMIT_BYTES + 1];
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/validate")
                    .header("content-type", "application/json")
                    .header("content-length", big.len().to_string())
                    .body(Body::from(big))
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum returns 413 Payload Too Large when DefaultBodyLimit trips.
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn health_router_serves_healthz() {
        let app = build_health_router();
        let resp = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_router_does_not_serve_validate() {
        // The health router is intentionally narrow — only /healthz.
        // If a deploy accidentally points the admission webhook URL
        // at the health port, callers get 404 and not a silent admit.
        let app = build_health_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/validate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
