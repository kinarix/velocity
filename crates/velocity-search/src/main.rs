//! `velocity-search` — the search tier (Phase 12a / ADR-011).
//!
//! Owns ALL search: per-schema, per-domain, and the per-org cross-domain
//! unified collection (`/api/{org}/search`), plus the CDC outbox→Typesense
//! workers and Typesense collection/alias management. Postgres is reached only
//! for the standalone search-audit write. No CRUD, no admin, no UI.
//!
//! The search/CDC/Typesense code lives in this crate's library (`cdc`,
//! `search_handlers`, `router`, `state`); the shared bootstrap + auth come
//! from the `velocity-api` core.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use velocity_core::auth::authenticate;
use velocity_core::router::build_auth;
use velocity_core::server::{bootstrap_common, init_tracing, install_crypto_provider};
use velocity_core::ApiConfig;
use velocity_search::router::build_search_api;
use velocity_search::state::SearchState;
use velocity_typesense::TypesenseClient;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    install_crypto_provider()?;
    let cfg = ApiConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind_addr,
        "velocity-search starting",
    );

    let common = bootstrap_common(&cfg).await?;
    let mut state = SearchState::new(common.registry.clone(), common.pool.clone());

    // Typesense is mandatory for this tier; without it /search returns
    // SEARCH_NOT_CONFIGURED and CDC cannot run.
    let (cdc_shutdown_tx, cdc_shutdown_rx) = tokio::sync::watch::channel(false);
    let _cdc_handle = match (cfg.typesense_url.as_deref(), cfg.typesense_api_key.as_deref()) {
        (Some(url), Some(key)) => {
            let ts = Arc::new(
                TypesenseClient::new(url, key).context("building typesense client")?,
            );
            state = state.with_typesense(ts.clone());
            let cdc_pool = state.pool.clone();
            let cdc_registry = state.registry.clone();
            Some(tokio::spawn(async move {
                velocity_search::cdc::run(cdc_pool, cdc_registry, ts, cdc_shutdown_rx).await;
            }))
        }
        _ => {
            tracing::warn!(
                "VELOCITY_API_TYPESENSE_URL/KEY unset — Tier-3 disabled; /search returns SEARCH_NOT_CONFIGURED, CDC not running"
            );
            None
        }
    };
    let _cdc_shutdown_tx = cdc_shutdown_tx;

    let app = build_search_api(state)
        .merge(build_auth(common.auth_handlers_state))
        .layer(axum::middleware::from_fn_with_state(common.auth_state, authenticate));

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "search-API listening");
    let serve = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());

    let (informer_handle, auth_informer_handle, health_handle) =
        (common.informer_handle, common.auth_informer_handle, common.health_handle);
    tokio::select! {
        r = serve => match r {
            Ok(()) => tracing::warn!("search-API exited cleanly"),
            Err(e) => tracing::error!(error = %e, "search-API failed"),
        },
        r = health_handle => tracing::error!(?r, "health server exited"),
        r = informer_handle => tracing::error!(?r, "schema informer exited"),
        r = auth_informer_handle => tracing::error!(?r, "auth strategy informer exited"),
    };
    Ok(())
}
