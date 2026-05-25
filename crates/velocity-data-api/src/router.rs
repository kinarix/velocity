//! Data-plane router.
//!
//! Routes are static — handlers extract the schema path from URL params and
//! resolve it against the registry on each request. No router rebuilds on
//! registry change (ADR-006: lock-free read). The `/auth/*` sub-router and the
//! platform control surface live in the `velocity-api` core; search in
//! `velocity-search`.

use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderName, Request, Response, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::Span;
use velocity_core::metrics_middleware;

use crate::handlers;
use crate::state::DataState;

/// 10 MB request body cap — matches the platform-wide limit referenced in
/// CLAUDE.md › Input size limits.
const BODY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

/// The canonical header — both Set and Propagate layers agree on this so a
/// caller-supplied id is honoured and a server-generated one ships back.
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Build the data-plane router. Exposed as both `build` (used by the
/// integration tests) and `build_data_api` (used by `main.rs`); they are
/// identical — a data-API pod serves the same surface everywhere.
pub fn build(state: DataState) -> Router {
    build_data_api(state)
}

/// Data-API router (Phase 12a / ADR-011): per-domain CRUD/query/time-machine/
/// archive. The platform-global audit endpoints (`/api/platform/audit*`) are
/// owned by the platform tier, and `/search` by the search tier, so both
/// answer `PLATFORM_ONLY` here — directing callers to the right service.
pub fn build_data_api(state: DataState) -> Router {
    // This builder deliberately does not install the auth middleware. The
    // binary wires `axum::middleware::from_fn_with_state(auth_state,
    // authenticate)` on top; tests stack their own identity-injection layer
    // instead, so the unauthenticated assertions stay possible.

    Router::new()
        .route("/api", get(index))
        .route("/version", get(version))
        .route("/api/platform/audit", get(platform_only))
        .route("/api/platform/audit/verify", get(platform_only))
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}",
            get(handlers::list).post(handlers::create),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/query",
            axum::routing::post(handlers::query),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/search",
            axum::routing::post(platform_only),
        )
        .route("/api/{org}/search", axum::routing::post(platform_only))
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}",
            get(handlers::get_one).put(handlers::update).delete(handlers::delete_soft),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}/history",
            get(crate::time_machine::history),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}/diff",
            get(crate::time_machine::diff_endpoint),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}/restore",
            axum::routing::post(crate::time_machine::restore),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}/replay",
            get(crate::time_machine::replay),
        )
        .route(
            "/api/{org}/{app}/{domain}/history/snapshot",
            axum::routing::post(crate::time_machine::snapshot),
        )
        // Phase 8 slice 9 — Archive API.
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}/archive",
            get(crate::archive_handlers::get_one),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/archive/query",
            axum::routing::post(crate::archive_handlers::query),
        )
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/{id}/unarchive",
            axum::routing::post(crate::archive_handlers::unarchive),
        )
        // Metrics middleware. Mounted *inside* the auth layer (added by
        // `main.rs`) so requests rejected at auth aren't charged a per-schema
        // label.
        .layer(axum::middleware::from_fn(metrics_middleware::record))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(X_REQUEST_ID.clone(), MakeRequestUuid))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|req: &Request<_>| {
                            let request_id = req
                                .headers()
                                .get(X_REQUEST_ID.clone())
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            tracing::info_span!(
                                "http_request",
                                method = %req.method(),
                                uri = %req.uri(),
                                request_id = request_id,
                                status = tracing::field::Empty,
                                latency_ms = tracing::field::Empty,
                            )
                        })
                        .on_response(|res: &Response<_>, latency: Duration, span: &Span| {
                            span.record("status", res.status().as_u16());
                            span.record("latency_ms", latency.as_millis() as u64);
                            tracing::info!("response");
                        }),
                )
                .layer(PropagateRequestIdLayer::new(X_REQUEST_ID.clone()))
                .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES)),
        )
        .with_state(state)
}

/// Stub for cross-schema / platform-global routes on a data-API pod
/// (Phase 12a). A single-domain pod can't serve these — its registry only
/// holds its own namespace — so it answers `404 PLATFORM_ONLY` directing the
/// caller to the shared platform-API.
async fn platform_only() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "code": "PLATFORM_ONLY",
            "message": "this endpoint is served by the platform-API, not a per-domain data-API"
        })),
    )
}

async fn index(State(state): State<DataState>) -> (StatusCode, Json<serde_json::Value>) {
    let inner = state.registry.snapshot();
    let paths: Vec<&String> = inner.by_path.keys().collect();
    (
        StatusCode::OK,
        Json(json!({
            "service": "velocity-data-api",
            "ready": state.registry.is_ready(),
            "schemas": paths.len(),
            "paths": paths,
        })),
    )
}

/// Build info — the small companion endpoint for `velocity version`.
async fn version(State(state): State<DataState>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "service": "velocity-data-api",
            "version": env!("CARGO_PKG_VERSION"),
            "git_sha": option_env!("VELOCITY_GIT_SHA").unwrap_or("unknown"),
            "ready":   state.registry.is_ready(),
        })),
    )
}
