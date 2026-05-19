use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use velocity_warm_reader::{build_app_state, http, WarmReaderConfig};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = WarmReaderConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind_addr = %cfg.bind_addr,
        health_addr = %cfg.health_addr,
        storage_url = %cfg.storage_url,
        "velocity-warm-reader starting",
    );

    let state = build_app_state(&cfg)?;

    let data_router = http::router(state);
    let health_router = http::health_router();

    let data_listener = tokio::net::TcpListener::bind(cfg.bind_addr)
        .await
        .with_context(|| format!("failed to bind data socket {}", cfg.bind_addr))?;
    let health_listener = tokio::net::TcpListener::bind(cfg.health_addr)
        .await
        .with_context(|| format!("failed to bind health socket {}", cfg.health_addr))?;

    tracing::info!("listeners up; serving warm-tier reads");

    let data_fut = async move { axum::serve(data_listener, data_router).await };
    let health_fut = async move { axum::serve(health_listener, health_router).await };

    tokio::select! {
        r = data_fut    => r.context("data listener exited")?,
        r = health_fut  => r.context("health listener exited")?,
    }
    Ok(())
}

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_warm_reader=debug"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}
