//! axum router + handlers for the warm-reader HTTP surface.
//!
//! Endpoints:
//!   - `POST /v1/warm/events`  — auth-gated, the real workload
//!   - `GET  /healthz`         — liveness, no auth
//!   - `GET  /readyz`          — readiness, no auth
//!
//! The data port and the health port are separate listeners so the
//! probes don't share an auth surface with the read endpoint — the same
//! pattern velocity-operator uses (operator/src/health.rs).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use datafusion::execution::context::SessionContext;
use object_store::ObjectStore;
use subtle::ConstantTimeEq;

use crate::datafusion_reader::{read_events, ReadParams};
use crate::error::WarmReaderError;
use crate::object_layout;
use crate::types::{EventsRequest, EventsResponse};

/// Per-process state passed to every handler. Three pieces:
///   - `session`: shared DataFusion `SessionContext` (registered with
///     the warm storage's `ObjectStore` at startup; reused across
///     requests so we don't re-pay registration cost).
///   - `store`: same `ObjectStore` as a separate handle, used for
///     cheap `HEAD` calls to filter out month-objects that don't
///     exist before they reach DataFusion. DataFusion's
///     `read_parquet` errors on missing paths; the pre-flight HEAD
///     turns those into a clean empty result.
///   - `base_url`: the warm storage URL prefix as a string, used to
///     reconstruct full per-file URLs for `read_parquet`. We can't
///     derive it from the `ObjectStore` because the `PrefixStore`
///     wrapper hides the bucket/prefix.
#[derive(Clone)]
pub struct AppState {
    pub session: Arc<SessionContext>,
    pub store: Arc<dyn ObjectStore>,
    pub base_url: Arc<str>,
    pub service_token: Arc<str>,
    /// Per-request fan-out cap. Set generously (12 months ≈ one warm
    /// year) — the warm-tier retention is years anyway, but the goal
    /// here is to bound per-request cost, not enforce retention.
    pub max_months: u32,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("session", &"<SessionContext>")
            .field("store", &"<ObjectStore>")
            .field("base_url", &self.base_url)
            .field("service_token", &"<redacted>")
            .field("max_months", &self.max_months)
            .finish()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/warm/events", post(events))
        // 1 MiB cap — internal RPC bodies should be much smaller than
        // user-facing API bodies (which cap at 10 MiB per CLAUDE.md).
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

pub fn health_router() -> Router {
    Router::new().route("/healthz", get(|| async { "ok" })).route("/readyz", get(|| async { "ok" }))
}

/// Constant-time service-token check. Bearer-only — the API client
/// shipped in `velocity-api::tiering::warm_reader` uses the same shape.
fn verify_bearer(headers: &HeaderMap, expected: &str) -> Result<(), WarmReaderError> {
    let h = headers.get(AUTHORIZATION).ok_or(WarmReaderError::AuthMissing)?;
    let s = h.to_str().map_err(|_| WarmReaderError::AuthMalformed)?;
    let token = s.strip_prefix("Bearer ").ok_or(WarmReaderError::AuthMalformed)?;
    if token.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(())
    } else {
        Err(WarmReaderError::AuthInvalid)
    }
}

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, WarmReaderError> {
    verify_bearer(&headers, &state.service_token)?;

    let req: EventsRequest = serde_json::from_slice(&body)
        .map_err(|e| WarmReaderError::BadRequest(format!("invalid request body: {e}")))?;

    // Validate path before anything touches the planner. Catches
    // path-traversal attempts and obviously malformed input early.
    object_layout::validate_path(&req.path)
        .map_err(|e| WarmReaderError::BadRequest(format!("{e}")))?;

    if req.limit == 0 {
        return Err(WarmReaderError::BadRequest("limit must be >= 1".into()));
    }

    let out = read_events(ReadParams {
        session: &state.session,
        store: state.store.clone(),
        base_url: &state.base_url,
        path: &req.path,
        entity_id: req.entity_id,
        until: req.until,
        limit: req.limit,
        max_months: state.max_months,
    })
    .await?;

    Ok((
        StatusCode::OK,
        Json(EventsResponse { events: out.events, objects_scanned: out.objects_scanned }),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use object_store::memory::InMemory;
    use tower::util::ServiceExt;

    fn state_with_memory_store() -> AppState {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let session = Arc::new(SessionContext::new());
        // For unit tests against an in-memory store, no URL prefix is
        // meaningful — DataFusion won't be exercised because we don't
        // call /v1/warm/events with valid bodies in these auth-error
        // path tests.
        AppState {
            session,
            store,
            base_url: Arc::from("memory:///"),
            service_token: Arc::from("test-token-32-chars-min-xxxxxxx"),
            max_months: 12,
        }
    }

    #[tokio::test]
    async fn missing_auth_is_401() {
        let app = router(state_with_memory_store());
        let resp = app
            .oneshot(Request::post("/v1/warm/events").body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_is_401_invalid() {
        let app = router(state_with_memory_store());
        let resp = app
            .oneshot(
                Request::post("/v1/warm/events")
                    .header("authorization", "Bearer not-the-right-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], "INVALID_SERVICE_TOKEN");
    }

    #[tokio::test]
    async fn malformed_path_is_400() {
        let app = router(state_with_memory_store());
        let body = serde_json::json!({
            "path": "../etc/passwd",
            "entity_id": "00000000-0000-0000-0000-000000000000",
            "until": "2026-05-18T00:00:00Z",
            "limit": 10,
        });
        let resp = app
            .oneshot(
                Request::post("/v1/warm/events")
                    .header("authorization", "Bearer test-token-32-chars-min-xxxxxxx")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
