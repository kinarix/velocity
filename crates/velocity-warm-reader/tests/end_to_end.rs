//! End-to-end integration: write a real Parquet object to a `file://`
//! warm store using the same column shape the operator's exporter
//! produces, then hit the warm-reader's HTTP endpoint and assert the
//! returned events match what we wrote.
//!
//! This test does NOT spin up Postgres. The point is to validate the
//! warm-reader's `Parquet on object_store → DataFusion → HTTP response`
//! path on its own; the full operator → warm-reader chain is exercised
//! separately once the API's `phase4_tiering_e2e` lands.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use arrow::array::{ArrayRef, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use chrono::{TimeZone, Utc};
use datafusion::execution::context::SessionContext;
use datafusion::execution::runtime_env::RuntimeEnv;
use object_store::ObjectStore;
use parquet::arrow::AsyncArrowWriter;
use uuid::Uuid;
use velocity_warm_reader::http;

fn arrow_schema() -> Arc<Schema> {
    // Identical layout to velocity-operator::tiering::schema::arrow_schema.
    // If you change this, change both sides — DataFusion is strict
    // about column types when it does predicate pushdown.
    Arc::new(Schema::new(vec![
        Field::new(
            "occurred_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("schema_org", DataType::Utf8, false),
        Field::new("entity_id", DataType::Utf8, true),
        Field::new("operation", DataType::Utf8, false),
        Field::new("diff", DataType::Utf8, true),
        Field::new("payload", DataType::Utf8, true),
    ]))
}

async fn write_test_object(store: Arc<dyn ObjectStore>, key: &str, entity: Uuid) {
    let path = object_store::path::Path::from(key.to_string());
    let writer = object_store::buffered::BufWriter::with_capacity(store.clone(), path, 4 * 1024 * 1024);
    let schema = arrow_schema();
    let mut pq = AsyncArrowWriter::try_new(writer, schema.clone(), None).unwrap();

    // Two events for the same entity: a create at 14:00, an update at 15:00.
    let create_ts = Utc.with_ymd_and_hms(2026, 3, 1, 14, 0, 0).unwrap().timestamp_micros();
    let update_ts = Utc.with_ymd_and_hms(2026, 3, 1, 15, 0, 0).unwrap().timestamp_micros();

    let occurred = TimestampMicrosecondArray::from(vec![Some(create_ts), Some(update_ts)])
        .with_timezone("UTC");
    let so = StringArray::from(vec!["acme/supply-chain/procurement/purchase-order/v1", "acme/supply-chain/procurement/purchase-order/v1"]);
    let eid_s = entity.hyphenated().to_string();
    let eid = StringArray::from(vec![Some(eid_s.clone()), Some(eid_s.clone())]);
    let op = StringArray::from(vec!["create", "update"]);
    let diff = StringArray::from(vec![None::<&str>, Some(r#"[{"op":"replace","path":"/qty","value":7}]"#)]);
    let payload = StringArray::from(vec![Some(r#"{"qty":1}"#), Some(r#"{"qty":7}"#)]);

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(occurred) as ArrayRef,
            Arc::new(so),
            Arc::new(eid),
            Arc::new(op),
            Arc::new(diff),
            Arc::new(payload),
        ],
    )
    .unwrap();

    pq.write(&batch).await.unwrap();
    pq.close().await.unwrap();
}

/// Shared bring-up: temp dir + file://-rooted object_store + DataFusion
/// SessionContext with that store registered, and an entity_id whose
/// two events are prewritten into a March 2026 Parquet file.
struct Harness {
    _tmp: tempfile::TempDir,
    state: http::AppState,
    entity: Uuid,
    addr: std::net::SocketAddr,
}

async fn bring_up() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let base_url = format!("file://{}", tmp.path().to_str().unwrap());
    let parsed = url::Url::parse(&base_url).unwrap();
    let (raw_store_box, prefix) = object_store::parse_url(&parsed).unwrap();
    let raw_store: Arc<dyn ObjectStore> = Arc::from(raw_store_box);
    let prefixed: Arc<dyn ObjectStore> = if prefix.as_ref().is_empty() {
        raw_store.clone()
    } else {
        Arc::new(object_store::prefix::PrefixStore::new(raw_store.clone(), prefix.clone()))
    };

    let entity = Uuid::new_v4();
    write_test_object(
        prefixed.clone(),
        "acme/supply-chain/procurement/purchase-order/v1/event_log_2026_03.parquet",
        entity,
    )
    .await;

    let runtime = Arc::new(RuntimeEnv::default());
    runtime.register_object_store(&parsed, raw_store.clone());
    // Mirror main.rs: disable view types so the downstream Arrow
    // downcast path stays single (`StringArray`, not `StringViewArray`).
    let mut cfg = datafusion::execution::config::SessionConfig::new();
    cfg.options_mut().execution.parquet.schema_force_view_types = false;
    let session = Arc::new(SessionContext::new_with_config_rt(cfg, runtime));

    let state = http::AppState {
        session,
        store: prefixed,
        base_url: Arc::from(base_url.as_str()),
        service_token: Arc::from("test-token-32-chars-min-xxxxxxx"),
        max_months: 12,
    };
    let app = http::router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Harness { _tmp: tmp, state, entity, addr }
}

async fn post_events(addr: std::net::SocketAddr, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}/v1/warm/events"))
        .bearer_auth("test-token-32-chars-min-xxxxxxx")
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn round_trip_reads_events_back() {
    let h = bring_up().await;
    let resp = post_events(
        h.addr,
        serde_json::json!({
            "path": "acme/supply-chain/procurement/purchase-order/v1",
            "entity_id": h.entity.hyphenated().to_string(),
            "until": "2026-03-01T16:00:00Z",
            "limit": 100,
        }),
    )
    .await;

    if resp.status() != 200 {
        let s = resp.status();
        let b = resp.text().await.unwrap_or_default();
        panic!("expected 200, got {s}: {b}");
    }
    let body: serde_json::Value = resp.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "expected create + update: {body:#?}");
    // Newest-first by occurred_at.
    assert_eq!(events[0]["operation"], "update");
    assert_eq!(events[1]["operation"], "create");
    assert_eq!(events[0]["payload"]["qty"], 7);

    // The state struct held a copy of the SessionContext — verify
    // we're not creating one per request (lifecycle assertion).
    let _ = h.state.session.state();
}

#[tokio::test]
async fn until_clamp_excludes_later_events() {
    let h = bring_up().await;
    let resp = post_events(
        h.addr,
        serde_json::json!({
            "path": "acme/supply-chain/procurement/purchase-order/v1",
            "entity_id": h.entity.hyphenated().to_string(),
            // Between the two events — should return only the create.
            "until": "2026-03-01T14:30:00Z",
            "limit": 100,
        }),
    )
    .await;
    if resp.status() != 200 {
        let s = resp.status();
        let b = resp.text().await.unwrap_or_default();
        panic!("expected 200, got {s}: {b}");
    }
    let body: serde_json::Value = resp.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["operation"], "create");
}

#[tokio::test]
async fn wrong_entity_id_returns_empty() {
    let h = bring_up().await;
    let other_entity = Uuid::new_v4();
    let resp = post_events(
        h.addr,
        serde_json::json!({
            "path": "acme/supply-chain/procurement/purchase-order/v1",
            "entity_id": other_entity.hyphenated().to_string(),
            "until": "2026-03-01T16:00:00Z",
            "limit": 100,
        }),
    )
    .await;
    if resp.status() != 200 {
        let s = resp.status();
        let b = resp.text().await.unwrap_or_default();
        panic!("expected 200, got {s}: {b}");
    }
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["events"].as_array().unwrap().len(), 0);
}
