//! Public API router.
//!
//! Routes are static — handlers extract the schema path from URL params and
//! resolve it against the registry on each request. No router rebuilds on
//! registry change (ADR-006: lock-free read).

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

use crate::auth_handlers::{self, AuthHandlersState};
use crate::handlers;
use crate::metrics_middleware;
use crate::platform_handlers;
use crate::state::AppState;

/// 10 MB request body cap — matches the platform-wide limit referenced in
/// CLAUDE.md › Input size limits.
const BODY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

/// The canonical header — both Set and Propagate layers agree on this so a
/// caller-supplied id is honoured and a server-generated one ships back.
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn build(state: AppState) -> Router {
    // `build` deliberately does not install the auth middleware. The binary
    // wires `axum::middleware::from_fn_with_state(auth_state, authenticate)`
    // on top of this router; tests stack their own identity-injection
    // layer instead, so the unauthenticated assertions stay possible.
    //
    // The `/auth/*` routes are *not* protected — they kick off and
    // terminate the OIDC redirect flow. The auth middleware already
    // recognises this (its `schema_path_from_uri` returns `None` for
    // anything not under `/api/{...}`), so they pass through untouched
    // even when the middleware is installed.
    Router::new()
        .route("/api", get(index))
        // Phase 6a-2: platform-internal audit endpoints. Mounted under
        // /api/platform so the auth middleware's `schema_path_from_uri`
        // (which requires 5 path segments after /api) naturally skips
        // them — these routes authenticate via the platform service
        // token, not the per-schema strategy.
        .route(
            "/api/platform/audit",
            get(platform_handlers::audit_list),
        )
        .route(
            "/api/platform/audit/verify",
            get(platform_handlers::audit_verify),
        )
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
            axum::routing::post(handlers::search),
        )
        .route(
            "/api/{org}/search",
            axum::routing::post(handlers::cross_search),
        )
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
        // Metrics middleware. Mounted *inside* the auth layer (which
        // is added by `main.rs`) so that requests rejected at auth
        // don't get charged against a per-schema label — by the time
        // we get here, the auth middleware has either admitted the
        // request and inserted an `Identity` extension or short-
        // circuited with a 4xx that this middleware never sees.
        .layer(axum::middleware::from_fn(metrics_middleware::record))
        .layer(
            ServiceBuilder::new()
                // 1. If the caller didn't pass X-Request-ID, generate a UUID
                //    so the rest of the stack has a stable id to log against.
                .layer(SetRequestIdLayer::new(X_REQUEST_ID.clone(), MakeRequestUuid))
                // 2. Per-request tracing span with method/uri/status/latency.
                //    JSON formatter in main.rs renders these as structured
                //    fields — one log line per request.
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
                // 3. Echo the id back so the client can correlate.
                .layer(PropagateRequestIdLayer::new(X_REQUEST_ID.clone()))
                // 4. 10 MB body cap. Applied last so it sees the original
                //    request bytes before any extractor reads them.
                .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES)),
        )
        .with_state(state)
}

/// Build the `/auth/*` sub-router for the OIDC redirect flow. Carries
/// its own state because the handlers don't touch the schema registry —
/// they only need the auth registry + session store + flow-cookie key.
/// Mounted alongside the API router in `main.rs` via `Router::merge`.
pub fn build_auth(state: AuthHandlersState) -> Router {
    Router::new()
        .route(
            "/auth/login/{namespace}/{name}",
            get(auth_handlers::login),
        )
        .route("/auth/callback", get(auth_handlers::callback))
        .route("/auth/logout", axum::routing::post(auth_handlers::logout))
        .with_state(state)
}

async fn index(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let inner = state.registry.snapshot();
    let paths: Vec<&String> = inner.by_path.keys().collect();
    (
        StatusCode::OK,
        Json(json!({
            "service": "velocity-api",
            "ready": state.registry.is_ready(),
            "schemas": paths.len(),
            "paths": paths,
        })),
    )
}
