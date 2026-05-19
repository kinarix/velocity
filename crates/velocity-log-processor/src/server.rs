//! Axum receiver for inbound log batches.
//!
//! One endpoint:
//!
//! - `POST /v1/logs` — body is `{ "records": [ <json>, … ] }`,
//!   bearer-authed. Each record is enriched, evaluated against the
//!   current policy bundle, and dispatched to all destinations on
//!   Keep. Drop/Sampled outcomes increment metrics but never send.
//!
//! The bundle + destinations live behind `ArcSwap` so the policy
//! reloader can atomically swap them without taking a lock or
//! blocking inbound requests.
//!
//! Plus the usual `/healthz` and `/readyz` (always 200 once the first
//! bundle has loaded — pre-load returns 503).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tower_http::limit::RequestBodyLimitLayer;

use crate::destination::{Destination, DestinationOutcome};
use crate::policy::LogPolicyBundle;
use crate::rules::{evaluate, Decision, LogRecord};

/// Snapshot of "what to apply right now". The reloader produces a new
/// snapshot when the policy file changes; the server atomically swaps
/// it in.
#[derive(Debug)]
pub struct PolicySnapshot {
    pub bundle: LogPolicyBundle,
    pub destinations: Vec<Arc<dyn Destination>>,
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub current: Arc<arc_swap::ArcSwap<PolicySnapshot>>,
    pub token: Arc<String>,
    pub ready: Arc<AtomicBool>,
    pub stats: Arc<Stats>,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub received: AtomicU64,
    pub kept: AtomicU64,
    pub dropped: AtomicU64,
    pub sampled: AtomicU64,
    pub unauthorized: AtomicU64,
    pub destination_sent: AtomicU64,
    pub destination_failed: AtomicU64,
    pub destination_skipped: AtomicU64,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/logs", post(ingest))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        // 4 MiB — internal hop from the collector, not a public API.
        // Keeps a runaway pod from blowing the processor's memory.
        .layer(RequestBodyLimitLayer::new(4 * 1024 * 1024))
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %addr, "log-processor listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.ready.load(Ordering::Relaxed) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no policy loaded yet")
    }
}

#[derive(Debug, Deserialize)]
pub struct IngestBody {
    #[serde(default)]
    pub records: Vec<Value>,
}

async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IngestBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !verify_token(&headers, &state.token) {
        state.stats.unauthorized.fetch_add(1, Ordering::Relaxed);
        return Err((StatusCode::UNAUTHORIZED, "invalid bearer token".into()));
    }

    state.stats.received.fetch_add(body.records.len() as u64, Ordering::Relaxed);

    let snapshot = state.current.load();
    for mut value in body.records {
        crate::enrich::enrich(&mut value);
        let mut record = LogRecord::new(value);
        let decision = evaluate(&snapshot.bundle.filters, &mut record);
        match decision {
            Decision::Keep => {
                state.stats.kept.fetch_add(1, Ordering::Relaxed);
                dispatch(&snapshot.destinations, &record.payload, &state.stats).await;
            }
            Decision::Drop => {
                state.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Decision::Sampled => {
                state.stats.sampled.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(StatusCode::ACCEPTED)
}

/// Sequential per-destination dispatch. v1 doesn't fan out — keeps
/// failure mode obvious (a slow destination backpressures the
/// processor, which backpressures the collector). Concurrent fan-out
/// with per-destination retry is a v2 optimisation if we measure
/// destination latency to be a bottleneck.
async fn dispatch(dests: &[Arc<dyn Destination>], record: &Value, stats: &Stats) {
    for d in dests {
        match d.send(record).await {
            DestinationOutcome::Sent => {
                stats.destination_sent.fetch_add(1, Ordering::Relaxed);
            }
            DestinationOutcome::Skipped(why) => {
                stats.destination_skipped.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(destination = %d.name(), reason = %why, "destination skipped record");
            }
            DestinationOutcome::Failed(why) => {
                stats.destination_failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(destination = %d.name(), error = %why, "destination dispatch failed");
            }
        }
    }
}

fn verify_token(headers: &HeaderMap, expected: &str) -> bool {
    let Some(h) = headers.get(axum::http::header::AUTHORIZATION) else { return false };
    let Ok(s) = h.to_str() else { return false };
    let Some(token) = s.strip_prefix("Bearer ") else { return false };
    token.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::destination::build_all;
    use crate::policy::{LogPolicyBundle, LogRoutingDestSpec};
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::BTreeMap;
    use tower::util::ServiceExt;

    fn state_with(bundle: LogPolicyBundle, dests: Vec<LogRoutingDestSpec>) -> AppState {
        let snap = PolicySnapshot { bundle, destinations: build_all(&dests) };
        AppState {
            current: Arc::new(arc_swap::ArcSwap::from(Arc::new(snap))),
            token: Arc::new("test_token_at_least_16_chars".into()),
            ready: Arc::new(AtomicBool::new(true)),
            stats: Arc::new(Stats::default()),
        }
    }

    async fn post(app: Router, body: &str, token: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = req.body(Body::from(body.to_string())).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let state = state_with(LogPolicyBundle::default(), vec![]);
        let app = router(state.clone());
        let (st, _) = post(app, r#"{"records":[]}"#, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(state.stats.unauthorized.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let state = state_with(LogPolicyBundle::default(), vec![]);
        let app = router(state.clone());
        let (st, _) = post(app, r#"{"records":[]}"#, Some("wrong-token-1234")).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_valid_token_and_counts_received() {
        let state = state_with(
            LogPolicyBundle::default(),
            vec![LogRoutingDestSpec {
                name: "c".into(),
                kind: "stdout".into(),
                config: BTreeMap::new(),
            }],
        );
        let app = router(state.clone());
        let (st, _) = post(
            app,
            r#"{"records":[{"msg":"hi"},{"msg":"bye"}]}"#,
            Some("test_token_at_least_16_chars"),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED);
        assert_eq!(state.stats.received.load(Ordering::Relaxed), 2);
        assert_eq!(state.stats.kept.load(Ordering::Relaxed), 2);
        assert_eq!(state.stats.destination_sent.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn readyz_503_until_ready() {
        let state = state_with(LogPolicyBundle::default(), vec![]);
        state.ready.store(false, Ordering::Relaxed);
        let app = router(state);
        let req = Request::builder().uri("/readyz").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
