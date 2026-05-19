//! End-to-end: spin up a stand-in processor (axum) on a random port,
//! point a `Collector` at a tempdir, write a pod log line, and verify
//! the processor receives the enriched JSON.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use velocity_log_collector::{Collector, CollectorConfig};

const UUID: &str = "12345678-1234-1234-1234-123456789abc";
const TOKEN: &str = "collector-test-token-1234567";

#[derive(Clone)]
struct Captured {
    bodies: Arc<Mutex<Vec<Value>>>,
    token_ok: Arc<Mutex<bool>>,
}

async fn handle(
    State(state): State<Captured>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"))
    {
        *state.token_ok.lock().await = true;
    }
    state.bodies.lock().await.push(body);
    StatusCode::ACCEPTED
}

async fn spawn_processor() -> (String, Captured) {
    let captured = Captured {
        bodies: Arc::new(Mutex::new(Vec::new())),
        token_ok: Arc::new(Mutex::new(false)),
    };
    let app = Router::new().route("/v1/logs", post(handle)).with_state(captured.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1/logs"), captured)
}

#[tokio::test]
async fn collector_ships_json_line_with_bearer_to_processor() {
    let (endpoint, captured) = spawn_processor().await;
    let root = tempfile::tempdir().unwrap();
    let pod_dir = root.path().join(format!("acme_api-1_{UUID}"));
    let container = pod_dir.join("main");
    tokio::fs::create_dir_all(&container).await.unwrap();
    let log_path = container.join("0.log");
    tokio::fs::write(&log_path, b"").await.unwrap();

    let cfg = CollectorConfig {
        log_root: root.path().to_path_buf(),
        processor_endpoint: endpoint,
        ingest_token: TOKEN.into(),
        scan_interval: Duration::from_millis(20),
        flush_interval: Duration::from_millis(20),
        max_batch_records: 100,
        max_batch_age: Duration::from_millis(50),
    };

    let c = Collector::new(cfg).unwrap();
    let runner = tokio::spawn(async move { c.run().await });

    // Give the collector one scan cycle, then write the log line.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut f = tokio::fs::OpenOptions::new().append(true).open(&log_path).await.unwrap();
    f.write_all(br#"{"level":"INFO","msg":"hello"}"#).await.unwrap();
    f.write_all(b"\n").await.unwrap();
    drop(f);

    // Poll for receipt.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if !captured.bodies.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    runner.abort();

    let bodies = captured.bodies.lock().await;
    assert!(!bodies.is_empty(), "processor should have received at least one batch");
    assert!(*captured.token_ok.lock().await, "bearer token should match expected");

    let has_hello =
        bodies.iter().flat_map(|b| b["records"].as_array().cloned().unwrap_or_default()).any(|r| {
            r.get("msg") == Some(&serde_json::json!("hello"))
                && r.get("level") == Some(&serde_json::json!("INFO"))
                && r["kubernetes"]["pod"] == "api-1"
                && r["kubernetes"]["container"] == "main"
                && r["kubernetes"]["namespace"] == "acme"
        });
    assert!(has_hello, "expected enriched 'hello' record in: {bodies:?}");
}
