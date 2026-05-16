//! Public API router.
//!
//! Phase 1 scaffolding ships a single placeholder route — generic CRUD
//! handlers (list/create/get/update/delete) land in the next task and bind
//! at `/api/:org/:app/:domain/:object/:version`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;

pub fn build(state: AppState) -> Router {
    Router::new().route("/api", get(index)).with_state(state)
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
