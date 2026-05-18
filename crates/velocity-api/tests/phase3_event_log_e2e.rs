#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 3.1 acceptance: every CRUD mutation produces exactly one
//! `platform.event_log` row inside the same transaction. Drives the API
//! handler chain against real Postgres and reads back from the event_log
//! table directly — proving the row is visible the instant the mutation
//! commits, not via a separate publish/queue.
//!
//! Sequence per test: create → update → delete on the same entity, then
//! query event_log filtered by entity_id. Expect 3 rows in order with the
//! correct {operation, payload presence, diff presence, source, actor}.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase3_event_log_e2e
//! Skips silently when env unset.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tower::ServiceExt;
use velocity_api::registry::ResolvedSchema;
use velocity_api::router;
use velocity_api::{AppState, Identity, SchemaRegistry};
use velocity_operator::PostgresProvisioner;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess,
    SchemaDefinitionSpec, SearchSpec, SearchTier,
};

fn admin_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

fn api_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_API_URL")
        .ok()
        .or_else(|| Some("postgres://velocity_api:velocity_api_dev@localhost:5434/velocity".into()))
}

fn field(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f
}

fn schema_spec() -> SchemaDefinitionSpec {
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: NamespacedRef {
                name: "default".into(),
                namespace: "acme-platform".into(),
            },
            overrides: Vec::new(),
        },
        access: AccessSpec {
            roles: vec![RoleAccess {
                role: "purchase-order-writer".into(),
                operations: vec![
                    "create".into(),
                    "read".into(),
                    "update".into(),
                    "delete".into(),
                    "restore".into(),
                ],
            }],
            ..AccessSpec::default()
        },
        fields: vec![field("po_number", FieldKind::String), field("supplier_code", FieldKind::String)],
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

async fn cleanup(admin: &PgPool, pg_schema: &str, schema_org: &str) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE"))
        .execute(admin)
        .await;
    // event_log persists across schema drops (it's a platform table) — clear
    // rows for this schema path so test runs don't leak into each other.
    let _ = sqlx::query("DELETE FROM platform.event_log WHERE schema_org = $1")
        .bind(schema_org)
        .execute(admin)
        .await;
    for role in [
        format!("{pg_schema}_reader"),
        format!("{pg_schema}_writer"),
        format!("{pg_schema}_admin"),
    ] {
        let _ = sqlx::query(&format!("DROP ROLE IF EXISTS {role}")).execute(admin).await;
    }
}

struct Harness {
    admin_pool: PgPool,
    api_pool: PgPool,
    pg_schema: String,
    schema_org: String,
    path: SchemaPath,
}

async fn setup_db(org: &str) -> Option<Harness> {
    let admin_url = admin_url()?;
    let api_url = api_url()?;
    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin_url).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api_url).await.unwrap();
    let pg_schema = format!("{org}_supply_chain_procurement");
    let schema_org = format!("{org}/supply-chain/procurement/purchase-order/v1");
    cleanup(&admin_pool, &pg_schema, &schema_org).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(org, "supply-chain", "procurement").await.unwrap();
    let path = SchemaPath::new(org, "supply-chain", "procurement", "purchase-order", "v1");
    let plan = velocity_operator::build_ddl(&schema_spec(), &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    Some(Harness { admin_pool, api_pool, pg_schema, schema_org, path })
}

fn ident() -> Identity {
    Identity {
        actor_id: "ravi".into(),
        roles: vec!["purchase-order-writer".into()],
        strategy: "acme-platform/default".into(),
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

fn build_app(h: &Harness) -> axum::Router {
    let (schemas, _ready) = SchemaRegistry::new();
    schemas.upsert(ResolvedSchema::from_spec(h.path.clone(), schema_spec()));
    let app_state = AppState::new(Arc::clone(&schemas), h.api_pool.clone());
    router::build(app_state).layer(from_fn(inject_identity(ident())))
}

async fn body_json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            // Body is non-JSON (likely a plain-text 4xx/5xx from a layer
            // below the handler — body limit, auth, etc). Wrap so the
            // calling test can still assert on the status while the
            // string body shows up in panic output.
            json!({ "_raw_body": String::from_utf8_lossy(&bytes).to_string() })
        })
    };
    (status, v)
}

fn collection_uri(h: &Harness) -> String {
    format!(
        "/api/{}/{}/{}/{}/{}",
        h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version
    )
}

fn item_uri(h: &Harness, id: &str) -> String {
    format!("{}/{}", collection_uri(h), id)
}

#[derive(Debug)]
struct StoredEvent {
    operation: String,
    source: String,
    actor: String,
    has_payload: bool,
    has_diff: bool,
}

async fn events_for(admin: &PgPool, entity_id: &str) -> Vec<StoredEvent> {
    let rows = sqlx::query(
        "SELECT operation, source, actor, payload, diff \
         FROM platform.event_log \
         WHERE entity_id = $1::uuid \
         ORDER BY occurred_at, operation",
    )
    .bind(entity_id)
    .fetch_all(admin)
    .await
    .expect("query event_log");
    rows.into_iter()
        .map(|r| StoredEvent {
            operation: r.get("operation"),
            source: r.get("source"),
            actor: r.get("actor"),
            has_payload: r.get::<Option<Value>, _>("payload").is_some(),
            has_diff: r.get::<Option<Value>, _>("diff").is_some(),
        })
        .collect()
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_update_delete_emits_three_event_log_rows() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // CREATE
    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "TATA001" }).to_string(),
        ))
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "create body: {body}");
    let id = body["id"].as_str().expect("returned id").to_string();

    // UPDATE — change supplier_code, bump version
    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "TATA002", "version": 1 }).to_string(),
        ))
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "update body: {body}");

    // DELETE (soft)
    let delete_req = Request::builder()
        .method("DELETE")
        .uri(item_uri(&h, &id))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(delete_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Assert: exactly three event_log rows in order.
    let events = events_for(&h.admin_pool, &id).await;
    assert_eq!(events.len(), 3, "expected 3 events, got {events:?}");

    // [0] create — payload present, diff absent (create has no "before")
    assert_eq!(events[0].operation, "create");
    assert_eq!(events[0].source, "api");
    assert_eq!(events[0].actor, "ravi");
    assert!(events[0].has_payload, "create event must carry the new row payload");
    assert!(!events[0].has_diff, "create event must NOT carry a diff");

    // [1] update — both payload and diff present (diff is the field-level
    // change set; payload is the post-update row, mirroring what the
    // handler returned to the client).
    assert_eq!(events[1].operation, "update");
    assert_eq!(events[1].source, "api");
    assert!(events[1].has_payload);
    assert!(events[1].has_diff, "update event must carry a JSON-Patch diff");

    // [2] delete — neither payload nor diff. Prior state is reconstructable
    // from the preceding create/update rows; storing it again here would
    // duplicate data without informational value.
    assert_eq!(events[2].operation, "delete");
    assert_eq!(events[2].source, "api");
    assert!(!events[2].has_payload, "delete must be a tombstone");
    assert!(!events[2].has_diff);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn get_history_returns_paginated_events_newest_first() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "TATA001" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();

    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "TATA002", "version": 1 }).to_string(),
        ))
        .unwrap();
    let _ = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;

    // GET /history — expects two events newest-first.
    let history_req = Request::builder()
        .method("GET")
        .uri(format!("{}/history", item_uri(&h, &id)))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(history_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "history body: {body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "expected 2 events");
    // Newest-first ordering — update before create.
    assert_eq!(items[0]["operation"], "update");
    assert_eq!(items[1]["operation"], "create");
    assert_eq!(items[0]["actor"], "ravi");
    // Update event carries diff; create does not.
    assert!(items[0]["diff"].is_array(), "update event must carry a JSON-Patch diff array");
    assert!(items[1]["diff"].is_null(), "create event must NOT carry a diff");

    // Pagination — ?limit=1 should return only the newest event.
    let limit_req = Request::builder()
        .method("GET")
        .uri(format!("{}/history?limit=1", item_uri(&h, &id)))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(limit_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "?limit=1 must return one event");
    assert_eq!(items[0]["operation"], "update");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn history_at_t_returns_state_at_that_point() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // Drive create, update; sleep a tick between so the occurred_at
    // timestamps are distinct. now() resolution is microseconds on
    // Postgres but two writes inside the same millisecond *can* land on
    // the same value — the sleeps make the ordering deterministic so
    // the ?at= test reads cleanly.
    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "ACME-V1" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();
    let t_after_create = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "ACME-V2", "version": 1 }).to_string(),
        ))
        .unwrap();
    let _ = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;
    let t_after_update = chrono::Utc::now();

    // ?at = (just after create, before update) — should see V1.
    let at_v1 = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/history?at={}",
            item_uri(&h, &id),
            &t_after_create.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(at_v1).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "at_v1 body: {body}");
    assert_eq!(body["supplier_code"], "ACME-V1", "state at t_after_create must be V1");

    // ?at = (after the update) — should see V2.
    let at_v2 = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/history?at={}",
            item_uri(&h, &id),
            &t_after_update.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(at_v2).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["supplier_code"], "ACME-V2", "state at t_after_update must be V2");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn history_at_t_before_creation_returns_404() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // Capture T BEFORE the create — querying at this point must 404
    // because no event for this entity exists yet.
    let t_before_create = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "po_number": "PO-001" }).to_string()))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();

    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/history?at={}",
            item_uri(&h, &id),
            &t_before_create.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn history_at_t_after_deletion_returns_404() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "po_number": "PO-001" }).to_string()))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(item_uri(&h, &id))
        .body(Body::empty())
        .unwrap();
    let _ = body_json(build_app(&h).oneshot(delete_req).await.unwrap()).await;
    let t_after_delete = chrono::Utc::now();

    // The entity existed but is now tombstoned. ?at after the delete must
    // 404 (the delete event is the latest at-or-before T and 'delete' is
    // the tombstone marker). Querying at T BEFORE the delete is exercised
    // by the V1/V2 test above.
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/history?at={}",
            item_uri(&h, &id),
            &t_after_delete.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn diff_endpoint_returns_json_patch_between_two_timestamps() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "V1" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();
    let t_v1 = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "V2", "version": 1 }).to_string(),
        ))
        .unwrap();
    let _ = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;
    let t_v2 = chrono::Utc::now();

    // Diff from t_v1 → t_v2 should contain a /supplier_code replace V1→V2.
    let diff_req = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/diff?from={}&to={}",
            item_uri(&h, &id),
            &t_v1.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            &t_v2.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(diff_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "diff body: {body}");
    let ops = body.as_array().expect("diff is a JSON-Patch array");
    let supplier_op = ops
        .iter()
        .find(|op| op["path"] == "/supplier_code")
        .expect("diff must include /supplier_code");
    assert_eq!(supplier_op["op"], "replace");
    assert_eq!(supplier_op["value"], "V2");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn diff_endpoint_rejects_inverted_range() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "po_number": "PO-001" }).to_string()))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();

    let now = chrono::Utc::now();
    let earlier = now - chrono::Duration::seconds(60);
    let diff_req = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/diff?from={}&to={}",
            item_uri(&h, &id),
            &now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            &earlier.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(diff_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn restore_applies_old_state_as_new_event() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    // The harness only applies the base migration; tests must also apply
    // any later migrations they depend on. Phase 3.5 adds the `reason`
    // column.
    let _ = sqlx::query("ALTER TABLE platform.event_log ADD COLUMN IF NOT EXISTS reason TEXT")
        .execute(&h.admin_pool)
        .await;

    // Create v1, update to v2.
    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "V1" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();
    let t_v1 = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "V2", "version": 1 }).to_string(),
        ))
        .unwrap();
    let _ = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;

    // Restore to t_v1 — should apply V1 as a new update event.
    let restore_req = Request::builder()
        .method("POST")
        .uri(format!("{}/restore", item_uri(&h, &id)))
        .header("content-type", "application/json")
        .header("x-reason", "rolling back per INC-123")
        .body(Body::from(
            json!({ "at": t_v1.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }).to_string(),
        ))
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(restore_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "restore body: {body}");
    assert_eq!(body["supplier_code"], "V1", "restored row must carry the V1 value");
    // Version bumped — create=1, update=2, restore=3.
    assert_eq!(body["version"], 3, "restore must bump version, not reset it");

    // event_log: now four events (create, update, restore, …). The
    // restore row carries source=restore + reason from the X-Reason
    // header.
    let row = sqlx::query(
        "SELECT source, reason, diff FROM platform.event_log \
         WHERE entity_id = $1::uuid AND operation = 'restore'",
    )
    .bind(&id)
    .fetch_one(&h.admin_pool)
    .await
    .expect("restore event_log row");
    let source: String = row.get("source");
    let reason: Option<String> = row.get("reason");
    let diff: Value = row.get("diff");
    assert_eq!(source, "restore");
    assert_eq!(reason.as_deref(), Some("rolling back per INC-123"));
    // Diff captures the V2→V1 transition on /supplier_code.
    let ops = diff.as_array().expect("diff is array");
    let supplier_op = ops
        .iter()
        .find(|op| op["path"] == "/supplier_code")
        .expect("/supplier_code in diff");
    assert_eq!(supplier_op["value"], "V1");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn restore_returns_409_when_target_matches_current() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let _ = sqlx::query("ALTER TABLE platform.event_log ADD COLUMN IF NOT EXISTS reason TEXT")
        .execute(&h.admin_pool)
        .await;

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "ONLY" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();
    let t_after_create = chrono::Utc::now();

    // Restore to the post-create state — same as current. Must 409 with
    // RESTORE_NO_OP rather than silently writing a redundant event.
    let restore_req = Request::builder()
        .method("POST")
        .uri(format!("{}/restore", item_uri(&h, &id)))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "at": t_after_create.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }).to_string(),
        ))
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(restore_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "RESTORE_NO_OP");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn restore_rejects_future_timestamp() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let _ = sqlx::query("ALTER TABLE platform.event_log ADD COLUMN IF NOT EXISTS reason TEXT")
        .execute(&h.admin_pool)
        .await;

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "po_number": "PO-001" }).to_string()))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();

    let future = chrono::Utc::now() + chrono::Duration::days(1);
    let restore_req = Request::builder()
        .method("POST")
        .uri(format!("{}/restore", item_uri(&h, &id)))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "at": future.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "reason": "from the future" }).to_string(),
        ))
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(restore_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn replay_streams_all_events_oldest_first() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "V1" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();

    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "V2", "version": 1 }).to_string(),
        ))
        .unwrap();
    let _ = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;

    // Drive replay; capture the raw body (SSE wire format: text/event-stream).
    let replay_req = Request::builder()
        .method("GET")
        .uri(format!("{}/replay", item_uri(&h, &id)))
        .body(Body::empty())
        .unwrap();
    let res = build_app(&h).oneshot(replay_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
    );
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("UTF-8 SSE");

    // Expect two "event:" lines — one create, one update — oldest-first.
    let event_lines: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("event: "))
        .collect();
    assert_eq!(event_lines, vec!["event: create", "event: update"]);
    // And a data: line per event, both parseable as our HistoryEvent.
    let data_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("data: ")).collect();
    assert_eq!(data_lines.len(), 2);
    for line in &data_lines {
        let payload = line.strip_prefix("data: ").unwrap();
        let v: Value = serde_json::from_str(payload).expect("data line is JSON");
        assert!(v.get("occurred_at").is_some(), "data must include occurred_at");
        assert!(v.get("operation").is_some(), "data must include operation");
    }

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn update_event_diff_reflects_changed_field() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let create_req = Request::builder()
        .method("POST")
        .uri(collection_uri(&h))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "po_number": "PO-001", "supplier_code": "TATA001" }).to_string(),
        ))
        .unwrap();
    let (_, body) = body_json(build_app(&h).oneshot(create_req).await.unwrap()).await;
    let id = body["id"].as_str().unwrap().to_string();

    let update_req = Request::builder()
        .method("PUT")
        .uri(item_uri(&h, &id))
        .header("content-type", "application/json")
        .header("if-match", "1")
        .body(Body::from(
            // Only supplier_code changes — po_number kept identical.
            json!({ "po_number": "PO-001", "supplier_code": "TATA999", "version": 1 }).to_string(),
        ))
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(update_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);

    // Pull the diff column for the update event and assert the patch only
    // mentions `supplier_code`. Lots of fields change implicitly on every
    // update (updated_at, version) — those MUST be in the diff too because
    // they ARE changes — but we assert the user-visible business field is
    // present with the new value.
    let row = sqlx::query(
        "SELECT diff FROM platform.event_log \
         WHERE entity_id = $1::uuid AND operation = 'update'",
    )
    .bind(&id)
    .fetch_one(&h.admin_pool)
    .await
    .expect("fetch update event");
    let diff: Value = row.get("diff");
    let ops = diff.as_array().expect("diff is array");
    let supplier_op = ops
        .iter()
        .find(|op| op["path"] == "/supplier_code")
        .expect("diff must include /supplier_code");
    assert_eq!(supplier_op["op"], "replace");
    assert_eq!(supplier_op["value"], "TATA999");

    // And po_number, which didn't change, must NOT appear in the diff.
    let unchanged = ops.iter().any(|op| op["path"] == "/po_number");
    assert!(!unchanged, "unchanged field /po_number must be absent from the patch");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

// ─── Phase 3.7 — Cross-entity snapshot ─────────────────────────────────────

fn snapshot_uri(h: &Harness, at: &str) -> String {
    // The snapshot route is scoped at the domain level, NOT object/version.
    // Encode `at` once via the same chrono RFC3339 form the handler accepts.
    format!(
        "/api/{}/{}/{}/history/snapshot?at={}",
        h.path.org, h.path.app, h.path.domain, at
    )
}

async fn create_entity(h: &Harness, po: &str, supplier: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri(collection_uri(h))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "po_number": po, "supplier_code": supplier }).to_string()))
        .unwrap();
    let (status, body) = body_json(build_app(h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {body}");
    body["id"].as_str().expect("id in create response").to_string()
}

#[tokio::test]
async fn snapshot_returns_all_live_entities_under_domain() {
    let _ = tracing_subscriber::fmt::try_init();
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // Three live entities under the same schema (the harness only registers
    // one for now — multi-object enumeration is covered by the registry
    // unit tests; here we're verifying the SQL path returns one row per
    // live entity, not one row per event).
    let _id1 = create_entity(&h, "PO-A1", "TATA001").await;
    let _id2 = create_entity(&h, "PO-A2", "TATA002").await;
    let _id3 = create_entity(&h, "PO-A3", "TATA003").await;

    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let req = Request::builder()
        .method("POST")
        .uri(snapshot_uri(&h, &at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "snapshot body: {body}");
    assert_eq!(body["count"], 3, "expected 3 live entities, body: {body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3);
    // Every item must carry schema + entity_id + reconstructed state.
    let pos: Vec<&str> = items
        .iter()
        .map(|i| i["state"]["po_number"].as_str().unwrap())
        .collect();
    for expected in ["PO-A1", "PO-A2", "PO-A3"] {
        assert!(pos.contains(&expected), "snapshot missing {expected}: {body}");
    }
    let schema = items[0]["schema"].as_str().unwrap();
    assert_eq!(schema, h.schema_org, "schema label must match registry key");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn snapshot_excludes_deleted_entities() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // Two entities — delete one. Snapshot must omit the tombstoned entity
    // entirely; a snapshot that surfaced deletes would lie to consumers
    // about what state the platform "remembers" right now.
    let keep_id = create_entity(&h, "PO-KEEP", "TATA001").await;
    let drop_id = create_entity(&h, "PO-DROP", "TATA002").await;

    // Delete second entity (soft delete via DELETE handler).
    let del_req = Request::builder()
        .method("DELETE")
        .uri(item_uri(&h, &drop_id))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(build_app(&h).oneshot(del_req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let req = Request::builder()
        .method("POST")
        .uri(snapshot_uri(&h, &at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 1, "snapshot must skip deleted entity; body: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["entity_id"], keep_id);
    assert_eq!(items[0]["state"]["po_number"], "PO-KEEP");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn snapshot_at_historical_t_excludes_later_entities() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // Create entity #1, capture T between creates, then create entity #2.
    // Snapshot at T must return ONLY entity #1; entity #2 didn't exist yet.
    // This is the "rewind the platform" use case that motivates the
    // endpoint over a simple SELECT * FROM table.
    let id1 = create_entity(&h, "PO-T1", "TATA001").await;
    // Sleep ~50ms so occurred_at strictly advances. event_log uses
    // `now()` per row; without a gap the two writes can land in the same
    // microsecond on fast hardware and the test gets flaky.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let snapshot_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _id2 = create_entity(&h, "PO-T2", "TATA002").await;

    let req = Request::builder()
        .method("POST")
        .uri(snapshot_uri(&h, &snapshot_at))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 1, "historical snapshot must exclude later entities; body: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["entity_id"], id1);
    assert_eq!(items[0]["state"]["po_number"], "PO-T1");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn snapshot_returns_404_when_no_schemas_registered_under_domain() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    // Snapshot a domain the registry knows nothing about. The schema
    // registered by the harness is under `supply-chain/procurement`;
    // querying `supply-chain/nope` should 404, not return an empty list.
    // An empty list would mask the configuration error — a caller might
    // assume "no entities" when the real answer is "you spelled the
    // domain wrong".
    let bogus_uri = format!(
        "/api/{}/supply-chain/nope/history/snapshot?at={}",
        h.path.org,
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    let req = Request::builder().method("POST").uri(&bogus_uri).body(Body::empty()).unwrap();
    let (status, body) = body_json(build_app(&h).oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["error"], "SCHEMA_NOT_FOUND");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}
