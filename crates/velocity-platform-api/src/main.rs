#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! `velocity-platform-api` — admin/UI backend (Phase 12a/12c, ADR-011).
//!
//! Owns: the admin CRD read/write endpoints (`/api/platform/objects/*`, with
//! the validating webhook in the write path), platform audit, build/registry
//! info, and the embedded UI. It serves **no** per-schema CRUD/query/search —
//! those are `velocity-data-api` and `velocity-search`. Always-on; the front
//! door for declaring desired state (the operator then materialises it).

mod platform_objects;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use platform_objects::AdminState;
use velocity_core::auth::authenticate;
use velocity_core::router::build_auth;
use velocity_core::server::{bootstrap_common, init_tracing, install_crypto_provider};
use velocity_core::ApiConfig;
use velocity_platform_api::router::build_platform_api;
use velocity_platform_api::state::PlatformState;
use velocity_platform_api::static_files;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    install_crypto_provider()?;
    let cfg = ApiConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind_addr,
        "velocity-platform-api starting",
    );

    let common = bootstrap_common(&cfg).await?;

    // Audit endpoints need the platform token + pool; nothing else data-plane.
    let mut state = PlatformState::new(common.registry.clone(), common.pool.clone());
    let admin_token = cfg.platform_audit_token.clone().map(Arc::new);
    if let Some(token) = admin_token.clone() {
        state = state.with_platform_audit_token(token);
    } else {
        tracing::warn!(
            "VELOCITY_API_PLATFORM_AUDIT_TOKEN unset — /api/platform/* (audit + admin objects) will be closed"
        );
    }

    // Clone sessions before auth_state is moved into the middleware layer below.
    let sessions = common.auth_state.sessions.clone();
    let admin = platform_objects::router(AdminState {
        kube: common.client.clone(),
        token: admin_token,
        sessions,
    });

    let app = build_platform_api(state)
        .merge(admin)
        .merge(build_auth(common.auth_handlers_state))
        .layer(axum::middleware::from_fn_with_state(common.auth_state, authenticate))
        .fallback_service(static_files::router());

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "platform-API listening");
    let serve = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());

    let (informer_handle, auth_informer_handle, health_handle) =
        (common.informer_handle, common.auth_informer_handle, common.health_handle);
    tokio::select! {
        r = serve => match r {
            Ok(()) => tracing::warn!("platform-API exited cleanly"),
            Err(e) => tracing::error!(error = %e, "platform-API failed"),
        },
        r = health_handle => tracing::error!(?r, "health server exited"),
        r = informer_handle => tracing::error!(?r, "schema informer exited"),
        r = auth_informer_handle => tracing::error!(?r, "auth informer exited"),
    };
    Ok(())
}
