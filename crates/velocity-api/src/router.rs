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

use crate::handlers;
use crate::state::AppState;

/// 10 MB request body cap — matches the platform-wide limit referenced in
/// CLAUDE.md › Input size limits.
const BODY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

/// The canonical header — both Set and Propagate layers agree on this so a
/// caller-supplied id is honoured and a server-generated one ships back.
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/api", get(index))
        .route(
            "/api/:org/:app/:domain/:object/:version",
            get(handlers::list).post(handlers::create),
        )
        .route(
            "/api/:org/:app/:domain/:object/:version/:id",
            get(handlers::get_one).put(handlers::update).delete(handlers::delete_soft),
        )
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
