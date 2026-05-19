use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use kube::Client;
use tracing_subscriber::EnvFilter;
use velocity_webhook::{
    build_admission_router, build_health_router, KubeStrategyChecker, WebhookConfig,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // rustls 0.23 requires an explicit crypto provider before any TLS code
    // runs. axum-server pulls in rustls but does not pick a provider for us,
    // so install aws-lc-rs (the higher-perf default) here.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))?;

    let cfg = WebhookConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        tls_addr = %cfg.tls_addr,
        health_addr = %cfg.health_addr,
        tls = cfg.tls_cert_path.is_some(),
        multi_tenant_mode = cfg.multi_tenant_mode,
        "velocity-webhook starting",
    );

    let kube = Client::try_default().await.context("building kube client")?;
    let checker = Arc::new(KubeStrategyChecker::new(kube));

    let app = build_admission_router(cfg.clone(), checker);
    let health_app = build_health_router();

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

    // The admission server has exited (clean shutdown or fatal error).
    // The health server is independent; drain it and surface any error
    // that killed it so an operator can see why the probe stopped
    // responding before the process exits.
    match health_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "health server exited with error"),
        Err(e) if e.is_cancelled() => {}
        Err(e) => tracing::error!(error = %e, "health server task panicked"),
    }
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
