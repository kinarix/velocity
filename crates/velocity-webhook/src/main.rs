use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::http::StatusCode;
use axum::{
    routing::{get, post},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use tracing_subscriber::EnvFilter;
use velocity_webhook::{handler, WebhookConfig};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = WebhookConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        tls_addr = %cfg.tls_addr,
        health_addr = %cfg.health_addr,
        tls = cfg.tls_cert_path.is_some(),
        "velocity-webhook starting",
    );

    let app = Router::new()
        .route("/validate", post(handler::validate))
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }));

    let health_app = Router::new().route("/healthz", get(|| async { (StatusCode::OK, "ok") }));

    let health_addr: SocketAddr = cfg.health_addr.parse().context("parsing health_addr")?;
    let health_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(health_addr).await?;
        tracing::info!(%health_addr, "health server listening (plain HTTP)");
        axum::serve(listener, health_app).await
    });

    let tls_addr: SocketAddr = cfg.tls_addr.parse().context("parsing tls_addr")?;

    match (&cfg.tls_cert_path, &cfg.tls_key_path) {
        (Some(cert), Some(key)) => {
            let tls =
                RustlsConfig::from_pem_file(cert, key).await.context("loading TLS cert/key")?;
            tracing::info!(%tls_addr, %cert, %key, "admission server listening (TLS)");
            axum_server::bind_rustls(tls_addr, tls).serve(app.into_make_service()).await?;
        }
        _ => {
            tracing::warn!(
                "VELOCITY_WEBHOOK_TLS_CERT/KEY not set — falling back to plain HTTP. \
                 Kubernetes admission webhooks REQUIRE TLS in production."
            );
            let listener = tokio::net::TcpListener::bind(tls_addr).await?;
            tracing::info!(%tls_addr, "admission server listening (plain HTTP, DEV ONLY)");
            axum::serve(listener, app).await?;
        }
    }

    let _ = health_handle.await;
    Ok(())
}

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_webhook=debug"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}
