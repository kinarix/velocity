//! `/healthz`, `/readyz`, and `/metrics` for the API server.
//!
//! These run on a separate listener so saturation on the public API doesn't
//! starve probes (and so liveness vs readiness can use the same port for
//! pod-level checks). `/metrics` lives here too so the public API listener
//! never has to surface internal counters.

use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use tokio::sync::watch;

use crate::metrics;

pub fn router(ready_rx: watch::Receiver<bool>, anonymous_auth: bool) -> Router {
    // Phase 12b: when the auth bypass is active, the readiness body carries a
    // banner so anyone probing the pod sees it without scraping metrics.
    let ready_body = if anonymous_auth {
        "ready [ANONYMOUS AUTH — authentication disabled]"
    } else {
        "ready"
    };
    Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route(
            "/readyz",
            get(move || {
                let rx = ready_rx.clone();
                async move {
                    if *rx.borrow() {
                        (StatusCode::OK, ready_body)
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "starting")
                    }
                }
            }),
        )
        .route(
            "/metrics",
            get(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                    metrics::gather(),
                )
                    .into_response()
            }),
        )
}

pub async fn serve(
    addr: &str,
    ready_rx: watch::Receiver<bool>,
    anonymous_auth: bool,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "API health server listening");
    axum::serve(listener, router(ready_rx, anonymous_auth)).await?;
    Ok(())
}
