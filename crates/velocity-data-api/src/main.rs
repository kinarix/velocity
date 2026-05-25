//! `velocity-data-api` — the data plane (Phase 12a / ADR-011).
//!
//! Serves CRUD, query DSL (Tier-1 filters + Tier-2 Postgres FTS), time-machine,
//! and archive for SchemaDefinitions in its registry. **Postgres only** — no
//! Typesense client, no CDC, no admin/CRD-write, no UI. Search lives in
//! `velocity-search`; admin/UI in `velocity-platform-api`.
//!
//! Deployed two ways, same binary, differing only by `VELOCITY_API_NAMESPACE` /
//! `VELOCITY_API_LABEL_SELECTOR`:
//!   * **app-scope** — label-selector on `{org}/{app}`, watching all matching
//!     domain namespaces; materialised by the operator per Application.
//!   * **domain-scope** — scoped to one `{org}-{app}-{domain}` namespace,
//!     materialised by the operator for `deployment.scope: domain` domains.

use std::net::SocketAddr;

use anyhow::Result;
use velocity_core::auth::authenticate;
use velocity_core::router::build_auth;
use velocity_core::server::{bootstrap_common, init_tracing, install_crypto_provider};
use velocity_core::ApiConfig;
use velocity_data_api::state::DataState;
use velocity_data_api::{router, startup};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    install_crypto_provider()?;
    let cfg = ApiConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind_addr,
        watch_namespace = cfg.watch_namespace.as_deref().unwrap_or("<all>"),
        "velocity-data-api starting",
    );

    let common = bootstrap_common(&cfg).await?;

    // Postgres-backed data state: registry + pool + tiered time-machine reader
    // + (optional) query cursor signer. No Typesense, no platform audit token.
    let (tiered_reader, cold_jobs) = startup::build_tiered_reader(&cfg, common.pool.clone());
    let mut state = DataState::new(common.registry.clone(), common.pool.clone())
        .with_tiering(tiered_reader, cold_jobs);
    if let Some(key) = cfg.cursor_signing_key.clone() {
        match velocity_core::CursorSigner::new(key) {
            Ok(s) => state = state.with_cursor_signer(std::sync::Arc::new(s)),
            Err(e) => anyhow::bail!("cursor signing key: {e}"),
        }
    } else {
        tracing::warn!(
            "VELOCITY_API_CURSOR_SIGNING_KEY unset — cursor-bearing /query requests will 400"
        );
    }

    let app = router::build_data_api(state)
        .merge(build_auth(common.auth_handlers_state))
        .layer(axum::middleware::from_fn_with_state(common.auth_state, authenticate));

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "data-API listening");
    let serve = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());

    let (informer_handle, auth_informer_handle, health_handle) =
        (common.informer_handle, common.auth_informer_handle, common.health_handle);
    tokio::select! {
        r = serve => match r {
            Ok(()) => tracing::warn!("data-API exited cleanly"),
            Err(e) => tracing::error!(error = %e, "data-API failed"),
        },
        r = health_handle => tracing::error!(?r, "health server exited"),
        r = informer_handle => tracing::error!(?r, "schema informer exited"),
        r = auth_informer_handle => tracing::error!(?r, "auth informer exited"),
    };
    Ok(())
}
