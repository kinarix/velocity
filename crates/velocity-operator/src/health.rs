//! `/healthz` and `/readyz` for the operator.

use axum::http::StatusCode;
use axum::{routing::get, Router};
use tokio::sync::watch;

/// Build the health router. `/readyz` returns 200 only once `ready_rx`
/// has been flipped to `true` (after the first informer sync).
pub fn router(ready_rx: watch::Receiver<bool>) -> Router {
    Router::new().route("/healthz", get(|| async { (StatusCode::OK, "ok") })).route(
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
}

/// Bind + serve the health router on `addr`. Logs and exits on bind failure.
pub async fn serve(addr: &str, ready_rx: watch::Receiver<bool>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "health server listening");
    axum::serve(listener, router(ready_rx)).await?;
    Ok(())
}
