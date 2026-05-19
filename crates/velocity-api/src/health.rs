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

pub fn router(ready_rx: watch::Receiver<bool>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route(
            "/readyz",
            get(move || {
                let rx = ready_rx.clone();
                async move {
                    if *rx.borrow() {
                        (StatusCode::OK, "ready")
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

pub async fn serve(addr: &str, ready_rx: watch::Receiver<bool>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "API health server listening");
    axum::serve(listener, router(ready_rx)).await?;
    Ok(())
}
