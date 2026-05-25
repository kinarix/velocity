//! Search-tier router (Phase 12a / ADR-011): `velocity-search` owns **all**
//! search — per-schema, per-domain, and the per-org cross-domain unified
//! collection. Mounted under `/search` so a single ingress host can prefix
//! route `/search`→search, `/api`→data, `/api/platform`→platform, `/`→UI
//! without controller-specific rewrites. The auth middleware's
//! `schema_path_from_uri` strips the `/search` prefix, so per-schema read
//! RBAC still applies.

use axum::extract::DefaultBodyLimit;
use axum::http::HeaderName;
use axum::routing::post;
use axum::Router;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

use crate::search_handlers;
use crate::state::SearchState;

/// 10 MB request body cap — matches the platform-wide limit.
const BODY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn build_search_api(state: SearchState) -> Router {
    let inner = Router::new()
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}/search",
            post(search_handlers::search),
        )
        .route("/api/{org}/search", post(search_handlers::cross_search));
    Router::new()
        .nest("/search", inner)
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(X_REQUEST_ID, MakeRequestUuid))
                .layer(PropagateRequestIdLayer::new(X_REQUEST_ID)),
        )
        .with_state(state)
}
