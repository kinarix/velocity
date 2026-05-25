#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! HTTP-level integration: exercises the full middleware stack
//! (`SetRequestIdLayer` → `TraceLayer` → `PropagateRequestIdLayer` →
//! body cap → handlers) and the Idempotency-Key replay semantics.
//!
//! Runs against docker-compose Postgres:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test http_idempotency

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use velocity_core::registry::ResolvedSchema;
use velocity_data_api::router;
use velocity_data_api::state::DataState;
use velocity_core::SchemaRegistry;
use velocity_operator::PostgresProvisioner;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

fn admin_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

fn api_url() -> String {
    std::env::var("VELOCITY_API_TEST_API_URL").unwrap_or_else(|_| {
        "postgres://velocity_api:velocity_api_dev@localhost:5434/velocity".into()
    })
}

fn field(name: &str, kind: FieldKind, required: bool) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f.required = required;
    f
}

fn spec(fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: velocity_types::common::NamespacedRef {
                name: "default".into(),
                namespace: "acme-platform".into(),
            },
            overrides: Vec::new(),
        },
        access: AccessSpec::default(),
        fields,
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

async fn cleanup(admin: &PgPool, pg_schema: &str, idem_keys: &[&str]) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE")).execute(admin).await;
    for role in
        [format!("{pg_schema}_reader"), format!("{pg_schema}_writer"), format!("{pg_schema}_admin")]
    {
        let _ = sqlx::query(&format!("DROP ROLE IF EXISTS {role}")).execute(admin).await;
    }
    for k in idem_keys {
        let _ = sqlx::query("DELETE FROM platform.idempotency_keys WHERE key = $1")
            .bind(k)
            .execute(admin)
            .await;
    }
}

async fn read_body(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn idempotency_and_request_id_round_trip() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };

    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api_url()).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    let idem_a = format!("idem-{suffix}-a");
    let idem_b = format!("idem-{suffix}-b");
    cleanup(&admin_pool, &pg_schema, &[&idem_a, &idem_b]).await;

    // Provision Postgres.
    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();
    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let sd_spec = spec(vec![
        field("po_number", FieldKind::String, true),
        field("supplier_code", FieldKind::String, false),
    ]);
    let plan = velocity_operator::build_ddl(&sd_spec, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    // Build router with a registry pre-loaded with the resolved schema.
    let (registry, _ready_rx) = SchemaRegistry::new();
    registry.upsert(ResolvedSchema::from_spec(path.clone(), sd_spec));
    let app_router = router::build(DataState::new(registry, api_pool.clone()));

    let url = format!("/api/{org}/{app}/{domain}/purchase-order/v1");
    let payload_v1 = json!({ "po_number": "PO-100", "supplier_code": "SUP-X" });
    let payload_v2 = json!({ "po_number": "PO-200", "supplier_code": "SUP-Y" });

    // ── (1) First POST with caller-supplied request id + idempotency key.
    let caller_request_id = format!("req-{suffix}");
    let req = Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("x-request-id", &caller_request_id)
        .header("idempotency-key", &idem_a)
        .body(Body::from(serde_json::to_vec(&payload_v1).unwrap()))
        .unwrap();
    let res = app_router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    // x-request-id echoed back unchanged.
    assert_eq!(
        res.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
        Some(caller_request_id.as_str())
    );
    let body1 = read_body(res.into_body()).await;
    assert_eq!(body1["po_number"], "PO-100");
    let first_id = body1["id"].as_str().unwrap().to_string();

    // ── (2) Replay: same key, same body → identical row back, no new insert.
    let req = Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("idempotency-key", &idem_a)
        .body(Body::from(serde_json::to_vec(&payload_v1).unwrap()))
        .unwrap();
    let res = app_router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    // Server-generated request id present even though we didn't send one.
    assert!(res.headers().get("x-request-id").is_some());
    let body2 = read_body(res.into_body()).await;
    assert_eq!(body2["id"].as_str().unwrap(), first_id, "replay must return the cached row");

    // ── (3) Conflict: same key, different body → 409 IDEMPOTENCY_CONFLICT.
    let req = Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("idempotency-key", &idem_a)
        .body(Body::from(serde_json::to_vec(&payload_v2).unwrap()))
        .unwrap();
    let res = app_router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body3 = read_body(res.into_body()).await;
    assert_eq!(body3["error"], "IDEMPOTENCY_CONFLICT");
    assert!(body3["message"].is_string());

    // ── (4) New key, different body → first-time success.
    let req = Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("idempotency-key", &idem_b)
        .body(Body::from(serde_json::to_vec(&payload_v2).unwrap()))
        .unwrap();
    let res = app_router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body4 = read_body(res.into_body()).await;
    assert_eq!(body4["po_number"], "PO-200");
    assert_ne!(body4["id"].as_str().unwrap(), first_id);

    // ── (5) Validation error → BAD_REQUEST shape.
    let req = Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
        .unwrap();
    let res = app_router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let err_body = read_body(res.into_body()).await;
    assert_eq!(err_body["error"], "BAD_REQUEST");
    assert!(err_body["message"].as_str().unwrap().contains("required"));

    cleanup(&admin_pool, &pg_schema, &[&idem_a, &idem_b]).await;
}
