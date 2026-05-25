#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 4 acceptance: time-machine reads with `at` in the warm window
//! route through the tier router to a real `velocity-warm-reader`
//! process (in-process, ephemeral port), which reads a real Parquet
//! object from a `file://`-backed `object_store`. End-to-end the API
//! returns the entity's reconstructed state at `at` even though no
//! event for the entity exists in Postgres.
//!
//! Why this test is the right shape for Phase 4 acceptance:
//!   - The exporter side is exercised by writing a Parquet object with
//!     the same column shape `velocity-operator::tiering` produces.
//!     Drift between the two sides is caught by
//!     `velocity-warm-reader::object_layout::object_key_layout_matches_*`
//!     and `velocity-operator::tiering::object_store_url::month_key_*`
//!     — this test pins the integration.
//!   - The schema has an empty `rowFilter`, so the per-entity hot-tier
//!     gate (`ensure_entity_visible`) short-circuits without touching
//!     event_log. The point of Phase 4 is the warm-tier path; the
//!     row-filter limitation when an entity has aged out of hot is a
//!     documented fail-closed (see time_machine.rs module doc).
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase4_warm_tier_e2e
//! Skips silently when the env var is unset.

use std::sync::Arc;

use arrow::array::{ArrayRef, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::Response;
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use datafusion::execution::context::SessionContext;
use datafusion::execution::runtime_env::RuntimeEnv;
use http_body_util::BodyExt;
use object_store::ObjectStore;
use parquet::arrow::AsyncArrowWriter;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;
use velocity_core::registry::ResolvedSchema;
use velocity_data_api::router;
use velocity_data_api::tiering::{TierWindows, TieredEventReader, WarmEventReader};
use velocity_core::{Identity, SchemaRegistry};
use velocity_data_api::DataState;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess,
    SchemaDefinitionSpec, SearchSpec, SearchTier,
};
use velocity_warm_reader::http as warm_http;

fn admin_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

fn field(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f
}

/// Schema with empty rowFilter / fieldFilter / masking so the
/// time-machine pipeline runs without per-entity hot-tier checks.
/// Tier-specific behaviour is isolated; Layer 4–6 gating is covered by
/// `phase3_time_machine_filters_e2e`.
fn schema_spec() -> SchemaDefinitionSpec {
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: NamespacedRef {
                name: "default".into(),
                namespace: "phase4-platform".into(),
            },
            overrides: Vec::new(),
        },
        access: AccessSpec {
            roles: vec![RoleAccess {
                role: "purchase-order-reader".into(),
                operations: vec!["read".into()],
            }],
            ..AccessSpec::default()
        },
        fields: vec![
            field("po_number", FieldKind::String),
            field("supplier_code", FieldKind::String),
        ],
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

/// Arrow column shape that mirrors the operator's tiering exporter.
/// Drift is caught by `velocity_operator::tiering::schema::tests` and
/// `velocity_warm_reader::object_layout::object_key_layout_*`.
fn arrow_schema() -> Arc<Schema> {
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

/// Write a single Parquet object with two events for the entity:
/// a `create` and a later `update`. The path + month in the key MUST
/// match what `object_key_for_month(schema_org, year, month)` computes;
/// we hard-code it here so a regression in either side surfaces as a
/// 404 from the warm path.
async fn write_warm_object(
    store: Arc<dyn ObjectStore>,
    key: &str,
    schema_org: &str,
    entity: Uuid,
    create_ts: DateTime<Utc>,
    update_ts: DateTime<Utc>,
) {
    let path = object_store::path::Path::from(key.to_string());
    let writer =
        object_store::buffered::BufWriter::with_capacity(store.clone(), path, 4 * 1024 * 1024);
    let schema = arrow_schema();
    let mut pq = AsyncArrowWriter::try_new(writer, schema.clone(), None).unwrap();

    let occurred = TimestampMicrosecondArray::from(vec![
        Some(create_ts.timestamp_micros()),
        Some(update_ts.timestamp_micros()),
    ])
    .with_timezone("UTC");
    let so = StringArray::from(vec![schema_org, schema_org]);
    let eid_s = entity.hyphenated().to_string();
    let eid = StringArray::from(vec![Some(eid_s.clone()), Some(eid_s.clone())]);
    let op = StringArray::from(vec!["create", "update"]);
    let diff = StringArray::from(vec![
        None::<&str>,
        Some(r#"[{"op":"replace","path":"/supplier_code","value":"TATA002"}]"#),
    ]);
    let payload = StringArray::from(vec![
        Some(r#"{"po_number":"PO-WARM-001","supplier_code":"TATA001"}"#),
        Some(r#"{"po_number":"PO-WARM-001","supplier_code":"TATA002"}"#),
    ]);

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

/// Bring up an in-process `velocity-warm-reader` server on an ephemeral
/// port backed by `file://<tempdir>`. Returns the URL + bearer token so
/// the API's `WarmEventReader` can connect.
struct WarmHarness {
    _tmp: tempfile::TempDir,
    addr: std::net::SocketAddr,
    bearer: String,
    schema_org: String,
    entity: Uuid,
    /// Timestamps the parquet was written with — tests can target one of
    /// these via `?at=` to assert the warm path returned the right row.
    create_at: DateTime<Utc>,
    update_at: DateTime<Utc>,
}

async fn bring_up_warm() -> WarmHarness {
    // Use a fixed schema_org so the parquet column value, the
    // request body's `path`, and the object-store key all agree.
    // Five segments == the registry_key shape velocity-api emits.
    let schema_org = "phase4/supply-chain/procurement/purchase-order/v1".to_string();

    // Pick wall-clock-old timestamps that will route to warm under
    // both the default windows and our test-tuned windows. Default:
    // hot=90 days, warm=5 years. 120 days ago is comfortably warm.
    let now = Utc::now();
    let create_at = now - Duration::days(125);
    let update_at = now - Duration::days(120);
    let year = update_at.format("%Y").to_string().parse::<i32>().unwrap();
    let month = update_at.format("%m").to_string().parse::<u32>().unwrap();
    let object_key = format!("{schema_org}/event_log_{year:04}_{month:02}.parquet");

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
    write_warm_object(prefixed.clone(), &object_key, &schema_org, entity, create_at, update_at)
        .await;

    let runtime = Arc::new(RuntimeEnv::default());
    runtime.register_object_store(&parsed, raw_store.clone());
    let mut cfg = datafusion::execution::config::SessionConfig::new();
    cfg.options_mut().execution.parquet.schema_force_view_types = false;
    let session = Arc::new(SessionContext::new_with_config_rt(cfg, runtime));

    let bearer = "phase4-warm-token-xxxxxxxxxxxxxxx".to_string();
    let state = warm_http::AppState {
        session,
        store: prefixed,
        base_url: Arc::from(base_url.as_str()),
        service_token: Arc::from(bearer.as_str()),
        max_months: 12,
    };
    let app = warm_http::router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    WarmHarness { _tmp: tmp, addr, bearer, schema_org, entity, create_at, update_at }
}

/// Build velocity-api state with the warm reader pointed at the
/// fixture server. Windows are tuned so any past timestamp routes to
/// warm (`hot_days = 0`) and the fixture timestamps stay inside warm
/// (`warm_years = 100`) regardless of how long this test takes to run
/// on a slow CI box.
fn build_app_state(pool: PgPool, schemas: Arc<SchemaRegistry>, warm: &WarmHarness) -> DataState {
    let warm_reader = WarmEventReader::new(
        format!("http://{}", warm.addr),
        warm.bearer.clone(),
        std::time::Duration::from_secs(5),
    )
    .expect("build warm reader client");
    let hot: Arc<dyn velocity_data_api::tiering::EventReader> =
        Arc::new(velocity_data_api::tiering::PostgresEventReader::new(pool.clone()));
    let tiered = Arc::new(
        TieredEventReader::new(hot, Some(Arc::new(warm_reader)))
            .with_windows(TierWindows { hot_days: 0, warm_years: 100 }),
    );
    let cold_jobs = velocity_data_api::tiering::cold_stub::ColdJobStore::new();
    DataState::new(schemas, pool).with_tiering(tiered, cold_jobs)
}

fn ident() -> Identity {
    Identity {
        actor_id: "phase4-reader".into(),
        roles: vec!["purchase-order-reader".into()],
        strategy: "phase4-platform/default".into(),
        ..Identity::default()
    }
}

fn inject_identity(
    id: Identity,
) -> impl Clone + Fn(Request<Body>, Next) -> futures::future::BoxFuture<'static, Response> {
    move |mut req: Request<Body>, next: Next| {
        let id = id.clone();
        Box::pin(async move {
            req.extensions_mut().insert(id);
            next.run(req).await
        })
    }
}

async fn body_json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "_raw_body": String::from_utf8_lossy(&bytes).to_string() }))
    };
    (status, v)
}

fn schema_path(schema_org: &str) -> SchemaPath {
    let parts: Vec<&str> = schema_org.split('/').collect();
    assert_eq!(parts.len(), 5, "schema_org must be 5-segment");
    SchemaPath::new(parts[0], parts[1], parts[2], parts[3], parts[4])
}

fn history_uri(schema_org: &str, entity_id: &str, at: DateTime<Utc>) -> String {
    let parts: Vec<&str> = schema_org.split('/').collect();
    let at_iso = at.to_rfc3339_opts(SecondsFormat::Millis, true);
    format!(
        "/api/{}/{}/{}/{}/{}/{entity_id}/history?at={at_iso}",
        parts[0], parts[1], parts[2], parts[3], parts[4]
    )
}

#[tokio::test]
async fn warm_at_query_returns_reconstructed_state() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&admin).await.unwrap();

    let warm = bring_up_warm().await;
    let (schemas, _ready) = SchemaRegistry::new();
    let path = schema_path(&warm.schema_org);
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));
    let app_state = build_app_state(pool, Arc::clone(&schemas), &warm);
    let app = router::build(app_state).layer(from_fn(inject_identity(ident())));

    // Target the post-update timestamp — point_in_time should return
    // the latest event payload at or before `at`, which is the update.
    let at = warm.update_at + Duration::seconds(1);
    let req = Request::builder()
        .method("GET")
        .uri(history_uri(&warm.schema_org, &warm.entity.hyphenated().to_string(), at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["po_number"], "PO-WARM-001");
    assert_eq!(body["supplier_code"], "TATA002", "expected latest update");
}

#[tokio::test]
async fn warm_at_between_events_returns_create_state() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&admin).await.unwrap();

    let warm = bring_up_warm().await;
    let (schemas, _ready) = SchemaRegistry::new();
    let path = schema_path(&warm.schema_org);
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));
    let app_state = build_app_state(pool, Arc::clone(&schemas), &warm);
    let app = router::build(app_state).layer(from_fn(inject_identity(ident())));

    // Halfway between create and update — should reconstruct the create state.
    let at = warm.create_at + (warm.update_at - warm.create_at) / 2;
    let req = Request::builder()
        .method("GET")
        .uri(history_uri(&warm.schema_org, &warm.entity.hyphenated().to_string(), at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["supplier_code"], "TATA001", "expected pre-update state");
}

#[tokio::test]
async fn warm_at_before_any_event_is_not_found() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&admin).await.unwrap();

    let warm = bring_up_warm().await;
    let (schemas, _ready) = SchemaRegistry::new();
    let path = schema_path(&warm.schema_org);
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));
    let app_state = build_app_state(pool, Arc::clone(&schemas), &warm);
    let app = router::build(app_state).layer(from_fn(inject_identity(ident())));

    // Well before the entity's first event.
    let at = warm.create_at - Duration::days(1);
    let req = Request::builder()
        .method("GET")
        .uri(history_uri(&warm.schema_org, &warm.entity.hyphenated().to_string(), at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

#[tokio::test]
async fn cold_at_returns_202_with_job_id() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&admin).await.unwrap();

    let warm = bring_up_warm().await;
    let (schemas, _ready) = SchemaRegistry::new();
    let path = schema_path(&warm.schema_org);
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));
    // Build state with windows that put any past `at` into Cold so the
    // 202 path fires before any reader is touched.
    let warm_reader = WarmEventReader::new(
        format!("http://{}", warm.addr),
        warm.bearer.clone(),
        std::time::Duration::from_secs(5),
    )
    .expect("build warm reader client");
    let hot: Arc<dyn velocity_data_api::tiering::EventReader> =
        Arc::new(velocity_data_api::tiering::PostgresEventReader::new(pool.clone()));
    let tiered = Arc::new(
        TieredEventReader::new(hot, Some(Arc::new(warm_reader)))
            .with_windows(TierWindows { hot_days: 0, warm_years: 0 }),
    );
    let cold_jobs = velocity_data_api::tiering::cold_stub::ColdJobStore::new();
    let app_state = DataState::new(Arc::clone(&schemas), pool).with_tiering(tiered, cold_jobs);
    let app = router::build(app_state).layer(from_fn(inject_identity(ident())));

    // Use a date that falls into Cold under (hot=0, warm=0).
    let at = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    let req = Request::builder()
        .method("GET")
        .uri(history_uri(&warm.schema_org, &warm.entity.hyphenated().to_string(), at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
    assert_eq!(body["code"], "TIME_MACHINE_COLD_RETRIEVAL_ACCEPTED");
    assert!(body["job_id"].is_string(), "expected job_id in response");
}

#[tokio::test]
async fn warm_unreachable_surfaces_as_503() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&admin).await.unwrap();

    // Build a tiered reader pointing at a port that is NOT listening.
    // Asserts the ADR-003 fail-closed default: a warm-tier dependency
    // failure surfaces as 503, never as silent empty results.
    let (schemas, _ready) = SchemaRegistry::new();
    let schema_org = "phase4/supply-chain/procurement/purchase-order/v1";
    let path = schema_path(schema_org);
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));

    let warm_reader = WarmEventReader::new(
        "http://127.0.0.1:1", // reserved discard port; nothing listens
        "irrelevant",
        std::time::Duration::from_secs(1),
    )
    .unwrap();
    let hot: Arc<dyn velocity_data_api::tiering::EventReader> =
        Arc::new(velocity_data_api::tiering::PostgresEventReader::new(pool.clone()));
    let tiered = Arc::new(
        TieredEventReader::new(hot, Some(Arc::new(warm_reader)))
            .with_windows(TierWindows { hot_days: 0, warm_years: 100 }),
    );
    let cold_jobs = velocity_data_api::tiering::cold_stub::ColdJobStore::new();
    let app_state = DataState::new(Arc::clone(&schemas), pool).with_tiering(tiered, cold_jobs);
    let app = router::build(app_state).layer(from_fn(inject_identity(ident())));

    let at = Utc::now() - Duration::days(120);
    let entity = Uuid::new_v4();
    let req = Request::builder()
        .method("GET")
        .uri(history_uri(schema_org, &entity.hyphenated().to_string(), at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
}
