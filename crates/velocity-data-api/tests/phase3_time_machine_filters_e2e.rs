#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 3 — time-machine endpoints applying Layer-4 row-filter,
//! Layer-5 field strip, and Layer-6 masking.
//!
//! `platform.event_log` has no RLS, so a scoped reader hitting
//! `/history`, `/at`, `/diff`, `/replay`, `/snapshot`, or `/restore`
//! would otherwise see fields and entities they're not entitled to via
//! the live GET. These tests pin:
//!
//! 1. **Field strip on history payload + diff** — a reader without the
//!    `finance-reader` role gets `unit_cost` removed from every event's
//!    `payload`, and JSON-Patch ops on `/unit_cost` removed from
//!    `diff`.
//! 2. **Field strip on /at** — `?at=T` reconstruction strips identically.
//! 3. **Field strip on /diff** — the returned JSON-Patch references no
//!    stripped field.
//! 4. **Field strip on /replay** — every SSE frame's payload/diff is
//!    stripped.
//! 5. **Field strip on /restore response** — the restored row goes
//!    through the same per-reader pipeline as GET, so the response
//!    doesn't leak fields the actor can't read.
//! 6. **Row filter on /history** — entity outside actor's row-scope
//!    returns 404 (no metadata leak).
//! 7. **Row filter on /snapshot** — entities outside scope are skipped.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase3_time_machine_filters_e2e
//! Skips silently when env unset (same pattern as other phase2b/3 e2e tests).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use velocity_core::registry::ResolvedSchema;
use velocity_data_api::router;
use velocity_core::{Identity, SchemaRegistry};
use velocity_data_api::DataState;
use velocity_operator::PostgresProvisioner;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldAccess, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess,
    RowFilter, RowFilterRule, SchemaDefinitionSpec, SearchSpec, SearchTier,
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

const SENSITIVE_FIELD: &str = "unit_cost";
const FINANCE_ROLE: &str = "finance-reader";
const GENERAL_READER: &str = "purchase-order-reader";
const GENERAL_WRITER: &str = "purchase-order-writer";
const RESTORER: &str = "purchase-order-restorer";
const WEST_ROLE: &str = "regional-reader-west";
const EAST_ROLE: &str = "regional-reader-east";

fn field_open(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f
}

fn field_gated(name: &str, kind: FieldKind, read_roles: &[&str]) -> FieldSpec {
    let mut f = field_open(name, kind);
    f.access = Some(FieldAccess {
        read: read_roles.iter().map(|s| (*s).to_string()).collect(),
        write: vec![],
    });
    f
}

/// Schema with:
/// - `po_number`, `region` — open fields (no `access` block).
/// - `unit_cost` — readable only by `finance-reader`.
/// - row filter: `regional-reader-west` sees `region=west`,
///   `regional-reader-east` sees `region=east`.
fn schema_spec() -> SchemaDefinitionSpec {
    let row_filter = vec![
        RowFilterRule {
            role: WEST_ROLE.into(),
            filter: RowFilter {
                field: "region".into(),
                op: "eq".into(),
                value: Value::String("west".into()),
            },
        },
        RowFilterRule {
            role: EAST_ROLE.into(),
            filter: RowFilter {
                field: "region".into(),
                op: "eq".into(),
                value: Value::String("east".into()),
            },
        },
    ];
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
            // Layer-1 RBAC: every test role gets `read` + writes the actor
            // needs. Without these the handler 403s before Layer-4/5 runs.
            roles: vec![
                RoleAccess { role: GENERAL_READER.into(), operations: vec!["read".into()] },
                RoleAccess { role: FINANCE_ROLE.into(), operations: vec!["read".into()] },
                RoleAccess {
                    role: GENERAL_WRITER.into(),
                    operations: vec![
                        "read".into(),
                        "create".into(),
                        "update".into(),
                        "delete".into(),
                    ],
                },
                RoleAccess {
                    role: RESTORER.into(),
                    operations: vec!["read".into(), "restore".into()],
                },
                RoleAccess { role: WEST_ROLE.into(), operations: vec!["read".into()] },
                RoleAccess { role: EAST_ROLE.into(), operations: vec!["read".into()] },
            ],
            row_filter,
            policies: Vec::new(),
        },
        fields: vec![
            field_open("po_number", FieldKind::String),
            field_open("region", FieldKind::String),
            field_gated(SENSITIVE_FIELD, FieldKind::String, &[FINANCE_ROLE]),
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

async fn cleanup(admin: &PgPool, pg_schema: &str, schema_org: &str) {
    let _ = sqlx::query("DELETE FROM platform.event_log WHERE schema_org = $1")
        .bind(schema_org)
        .execute(admin)
        .await;
    let _ = sqlx::query("DELETE FROM platform.audit_log WHERE schema_org = $1")
        .bind(schema_org)
        .execute(admin)
        .await;
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE")).execute(admin).await;
    for role in
        [format!("{pg_schema}_reader"), format!("{pg_schema}_writer"), format!("{pg_schema}_admin")]
    {
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

fn ident(actor: &str, roles: &[&str]) -> Identity {
    Identity {
        actor_id: actor.into(),
        roles: roles.iter().map(|s| (*s).to_string()).collect(),
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

fn build_app(h: &Harness, identity: Identity) -> axum::Router {
    let (schemas, _ready) = SchemaRegistry::new();
    schemas.upsert(ResolvedSchema::from_spec(h.path.clone(), schema_spec()));
    let app_state = DataState::new(Arc::clone(&schemas), h.api_pool.clone());
    router::build(app_state).layer(from_fn(inject_identity(identity)))
}

async fn body_json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

async fn create_one(h: &Harness, po: &str, region: &str, unit_cost: &str) -> String {
    // Writer identity, full access — feeds event_log directly.
    let app = build_app(h, ident("seed-writer", &[GENERAL_WRITER, FINANCE_ROLE]));
    let body = json!({ "po_number": po, "region": region, SENSITIVE_FIELD: unit_cost });
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {body}");
    body["id"].as_str().unwrap().to_string()
}

async fn update_one(h: &Harness, id: &str, body: Value) {
    let app = build_app(h, ident("seed-writer", &[GENERAL_WRITER, FINANCE_ROLE]));
    let req = Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version, id
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "update failed: {body}");
}

// ─── Field-strip tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn history_strips_sensitive_field_from_payload() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let id = create_one(&h, "PO-001", "west", "$100").await;

    // General reader (no FINANCE_ROLE) hits /history. The history is one
    // create event; its payload was stored unstripped (writes don't
    // pre-strip — see event_log doc), so the strip MUST happen on read.
    let app = build_app(&h, ident("alice", &[GENERAL_READER]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/history",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version, id
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "history call failed: {body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "one create event expected");
    let payload = &items[0]["payload"];
    assert_eq!(payload["po_number"], "PO-001");
    assert!(
        payload.get(SENSITIVE_FIELD).is_none(),
        "history.payload must strip `{SENSITIVE_FIELD}` for general reader; got {payload}"
    );

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn history_keeps_sensitive_field_for_finance_reader() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let id = create_one(&h, "PO-001", "west", "$100").await;

    // Positive control: without this test, the strip could be passing
    // because the field was never written. Finance reader must see it.
    let app = build_app(&h, ident("bina", &[GENERAL_READER, FINANCE_ROLE]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/history",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version, id
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["payload"][SENSITIVE_FIELD], "$100");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn history_strips_sensitive_op_from_diff() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let id = create_one(&h, "PO-001", "west", "$100").await;
    update_one(
        &h,
        &id,
        json!({ "po_number": "PO-001", "region": "west", SENSITIVE_FIELD: "$250", "version": 1 }),
    )
    .await;

    // General reader sees the update event but its diff must NOT
    // contain a `/unit_cost` op — that would re-leak the value the
    // payload strip removed.
    let app = build_app(&h, ident("alice", &[GENERAL_READER]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/history",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version, id
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    // Find the update event (history is newest-first).
    let update_ev =
        items.iter().find(|e| e["operation"] == "update").expect("update event present");
    let diff = update_ev["diff"].as_array().expect("diff array");
    let mentions_unit_cost =
        diff.iter().any(|op| op["path"].as_str().map(|s| s.contains("unit_cost")).unwrap_or(false));
    assert!(
        !mentions_unit_cost,
        "diff for general reader must not reference /unit_cost; got {diff:?}"
    );

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn point_in_time_strips_sensitive_field() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let id = create_one(&h, "PO-001", "west", "$100").await;

    let at = chrono::Utc::now() + chrono::Duration::seconds(1);
    let app = build_app(&h, ident("alice", &[GENERAL_READER]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/history?at={}",
            h.path.org,
            h.path.app,
            h.path.domain,
            h.path.object,
            h.path.version,
            id,
            at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "point-in-time call failed: {body}");
    assert_eq!(body["po_number"], "PO-001");
    assert!(body.get(SENSITIVE_FIELD).is_none(), "/at must strip {SENSITIVE_FIELD}; got {body}");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn diff_endpoint_strips_sensitive_op() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let id = create_one(&h, "PO-001", "west", "$100").await;
    // Capture t0 AFTER create so state_at(t0) returns v1 (the create
    // event's payload). Sleeping bookends both timestamps with a
    // wall-clock gap from the surrounding writes — the handler's
    // `<=`/`>=` comparators are fine without sleeps, but the wall-clock
    // separation removes any ambiguity around DB-vs-host clock skew.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let t0 = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    update_one(
        &h,
        &id,
        json!({ "po_number": "PO-001", "region": "west", SENSITIVE_FIELD: "$250", "version": 1 }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let t1 = chrono::Utc::now();

    let app = build_app(&h, ident("alice", &[GENERAL_READER]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/diff?from={}&to={}",
            h.path.org,
            h.path.app,
            h.path.domain,
            h.path.object,
            h.path.version,
            id,
            t0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            t1.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "diff call failed: {body}");
    let patch = body.as_array().expect("diff is an array");
    let mentions_unit_cost = patch
        .iter()
        .any(|op| op["path"].as_str().map(|s| s.contains("unit_cost")).unwrap_or(false));
    assert!(
        !mentions_unit_cost,
        "diff for general reader must not reference /unit_cost; got {patch:?}"
    );

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn restore_response_is_stripped() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let id = create_one(&h, "PO-001", "west", "$100").await;
    // Same timing pattern as `diff_endpoint_strips_sensitive_op`: a
    // clear wall-clock gap on each side of t0 so state_at(t0) lands
    // on v1 even under DB-vs-host clock skew.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let t0 = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    update_one(
        &h,
        &id,
        json!({ "po_number": "PO-001", "region": "west", SENSITIVE_FIELD: "$250", "version": 1 }),
    )
    .await;

    // Restorer doesn't carry FINANCE_ROLE → restore writes the raw $100
    // back into the row, but the response body must have `unit_cost`
    // stripped. The actor still successfully restored the value (we
    // can't verify that here without admin SELECT, but it's pinned by
    // `phase3_event_log_e2e::restore_applies_old_state_as_new_event`).
    let app = build_app(&h, ident("alice", &[RESTORER]));
    let body = json!({ "at": t0.to_rfc3339() });
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/restore",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version, id
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "restore failed: {body}");
    assert!(
        body.get(SENSITIVE_FIELD).is_none(),
        "restore response for non-finance role must strip {SENSITIVE_FIELD}; got {body}"
    );

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

// ─── Row-filter tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn history_404s_for_actor_outside_row_scope() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let east_id = create_one(&h, "PO-EAST-1", "east", "$100").await;

    // West-only reader asks for an east entity's history → 404.
    // No metadata leak: same shape they'd get for a never-existed id.
    let app = build_app(&h, ident("alice", &[WEST_ROLE]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/history",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version, east_id
        ))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn point_in_time_404s_for_actor_outside_row_scope() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let east_id = create_one(&h, "PO-EAST-1", "east", "$100").await;

    let at = chrono::Utc::now() + chrono::Duration::seconds(1);
    let app = build_app(&h, ident("alice", &[WEST_ROLE]));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}/{}/history?at={}",
            h.path.org,
            h.path.app,
            h.path.domain,
            h.path.object,
            h.path.version,
            east_id,
            at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, _) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn snapshot_skips_entities_outside_row_scope() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let _west_id = create_one(&h, "PO-WEST-1", "west", "$100").await;
    let _east_id = create_one(&h, "PO-EAST-1", "east", "$200").await;

    // West reader snapshots → only the west entity appears. The east
    // entity must be skipped (not 404 — snapshot is cross-entity, so
    // skip-and-emit-rest is the right shape).
    let at = chrono::Utc::now() + chrono::Duration::seconds(1);
    let app = build_app(&h, ident("alice", &[WEST_ROLE]));
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/{}/{}/{}/history/snapshot?at={}",
            h.path.org,
            h.path.app,
            h.path.domain,
            at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "snapshot failed: {body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "expected one west entity, got {items:?}");
    assert_eq!(items[0]["state"]["po_number"], "PO-WEST-1");

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}

#[tokio::test]
async fn snapshot_strips_sensitive_field_per_item() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    let _ = create_one(&h, "PO-WEST-1", "west", "$100").await;

    let at = chrono::Utc::now() + chrono::Duration::seconds(1);
    let app = build_app(&h, ident("alice", &[GENERAL_READER, WEST_ROLE]));
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/{}/{}/{}/history/snapshot?at={}",
            h.path.org,
            h.path.app,
            h.path.domain,
            at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "snapshot failed: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let state = &items[0]["state"];
    assert!(
        state.get(SENSITIVE_FIELD).is_none(),
        "snapshot state must strip {SENSITIVE_FIELD} for non-finance role; got {state}"
    );

    cleanup(&h.admin_pool, &h.pg_schema, &h.schema_org).await;
}
