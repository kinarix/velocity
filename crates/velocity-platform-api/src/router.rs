//! Platform-API router (Phase 12a / ADR-011): the admin/UI tier's *shared*
//! routes — registry index, build info, and the platform audit endpoints.
//! It serves **no** per-schema CRUD/query/search (those are the data-API and
//! search tiers). The admin CRD read/write endpoints (`platform_objects`) are
//! merged on top of this in `main.rs`.

use axum::extract::State;
use axum::http::{HeaderName, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

use crate::platform_handlers;
use crate::state::PlatformState;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn build_platform_api(state: PlatformState) -> Router {
    Router::new()
        .route("/api", get(index))
        .route("/version", get(version))
        .route("/api/platform/audit", get(platform_handlers::audit_list))
        .route("/api/platform/audit/verify", get(platform_handlers::audit_verify))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(X_REQUEST_ID, MakeRequestUuid))
                .layer(PropagateRequestIdLayer::new(X_REQUEST_ID)),
        )
        .with_state(state)
}

async fn index(State(state): State<PlatformState>) -> (StatusCode, Json<serde_json::Value>) {
    let inner = state.registry.snapshot();
    let paths: Vec<&String> = inner.by_path.keys().collect();
    (
        StatusCode::OK,
        Json(json!({
            "service": "velocity-platform-api",
            "ready": state.registry.is_ready(),
            "schemas": paths.len(),
            "paths": paths,
        })),
    )
}

/// Build info — the small companion endpoint for `velocity version`.
async fn version(State(state): State<PlatformState>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "service": "velocity-platform-api",
            "version": env!("CARGO_PKG_VERSION"),
            "git_sha": option_env!("VELOCITY_GIT_SHA").unwrap_or("unknown"),
            "ready":   state.registry.is_ready(),
        })),
    )
}
