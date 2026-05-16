use anyhow::Result;
use tracing_subscriber::EnvFilter;
use velocity_api::{health, informer, router, startup, ApiConfig, AppState, SchemaRegistry};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // rustls 0.23 requires an explicit crypto provider before any TLS code
    // runs. kube-rs (via rustls-tls) pulls rustls in but doesn't pick a
    // provider for us.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))?;

    let cfg = ApiConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind_addr,
        health = %cfg.health_addr,
        watch_namespace = cfg.watch_namespace.as_deref().unwrap_or("<all>"),
        "velocity-api starting",
    );

    // ADR-007 startup gate.
    let pool = startup::pool_with_checks(&cfg).await?;

    // Kube client + schema registry fed by an informer.
    let client = kube::Client::try_default().await?;
    let (registry, ready_rx) = SchemaRegistry::new();

    let informer_registry = registry.clone();
    let informer_client = client.clone();
    let informer_ns = cfg.watch_namespace.clone();
    let informer_handle = tokio::spawn(async move {
        if let Err(e) = informer::run(informer_registry, informer_client, informer_ns).await {
            tracing::error!(error = %e, "schema informer terminated");
        }
    });

    // Health server (separate listener — readiness gates on the registry).
    let health_addr = cfg.health_addr.clone();
    let health_ready_rx = ready_rx.clone();
    let health_handle =
        tokio::spawn(async move { health::serve(&health_addr, health_ready_rx).await });

    // Public API.
    let state = AppState::new(registry, pool);
    let app = router::build(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "API server listening");

    tokio::select! {
        r = axum::serve(listener, app) => match r {
            Ok(()) => tracing::warn!("API server exited cleanly"),
            Err(e) => tracing::error!(error = %e, "API server failed"),
        },
        r = health_handle => match r {
            Ok(Ok(())) => tracing::warn!("health server exited cleanly"),
            Ok(Err(e)) => tracing::error!(error = %e, "health server failed"),
            Err(e)     => tracing::error!(error = %e, "health server panicked"),
        },
        r = informer_handle => match r {
            Ok(()) => tracing::warn!("informer task exited"),
            Err(e) => tracing::error!(error = %e, "informer task panicked"),
        },
    };

    Ok(())
}

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_api=debug,kube=info"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}
