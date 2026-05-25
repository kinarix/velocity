//! Core router: the OIDC `/auth/*` sub-router, shared by every tier's
//! `main.rs`. The platform control surface (index, build info, audit) lives
//! in `velocity-platform-api`; the per-domain data routes in
//! `velocity-data-api`; search in `velocity-search`.

use axum::routing::get;
use axum::Router;

use crate::auth_handlers::{self, AuthHandlersState};

/// Build the `/auth/*` sub-router for the OIDC redirect flow. Carries
/// its own state because the handlers don't touch the schema registry —
/// they only need the auth registry + session store + flow-cookie key.
/// Mounted alongside each tier's router in its `main.rs` via `Router::merge`.
pub fn build_auth(state: AuthHandlersState) -> Router {
    Router::new()
        .route("/auth/login/{namespace}/{name}", get(auth_handlers::login))
        .route("/auth/callback", get(auth_handlers::callback))
        .route("/auth/logout", axum::routing::post(auth_handlers::logout))
        .with_state(state)
}
