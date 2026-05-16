//! Public API router.
//!
//! Routes are static — handlers extract the schema path from URL params and
//! resolve it against the registry on each request. No router rebuilds on
//! registry change (ADR-006: lock-free read).

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::handlers;
use crate::state::AppState;

/// 10 MB request body cap — matches the platform-wide limit referenced in
/// CLAUDE.md › Input size limits.
const BODY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

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
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
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

