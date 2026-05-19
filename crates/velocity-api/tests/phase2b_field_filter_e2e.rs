#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 2b Layer-5 read-strip against a real Postgres.
//!
//! The unit tests in `src/field_filter.rs` prove the strip semantics in
//! isolation, and `field_filter_routes.rs` proves the write-reject path
//! at HTTP. What no test pinned before this one is the full round trip:
//! SQL fetches all columns → handler invokes `strip_for_read` row-by-row
//! → response only contains fields the actor's role can see.
//!
//! That sequence is load-bearing — a regression where the handler skips
//! the strip (e.g., refactor of `list()` that forgets to call
//! `schema.field_filter.strip_for_read`) would silently leak protected
//! columns to every reader, with no other test catching it.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase2b_field_filter_e2e
//! Skips silently when env unset (same pattern as `phase2a_e2e.rs`).

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
use velocity_api::registry::ResolvedSchema;
use velocity_api::router;
use velocity_api::{AppState, Identity, SchemaRegistry};
use velocity_operator::PostgresProvisioner;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldAccess, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess,
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

const SENSITIVE_FIELD: &str = "unit_cost";
const FINANCE_ROLE: &str = "finance-reader";
const GENERAL_ROLE: &str = "purchase-order-reader";

fn field_open(name: &str) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
    f
}

fn field_gated(name: &str, read_roles: &[&str]) -> FieldSpec {
    let mut f = field_open(name);
    f.access = Some(FieldAccess {
        read: read_roles.iter().map(|s| (*s).to_string()).collect(),
        write: vec![], // writes wide-open — read-strip is what we're testing
    });
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
            // Layer-1 RBAC must admit both roles before Layer-5 runs.
            roles: vec![
                RoleAccess { role: GENERAL_ROLE.into(), operations: vec!["read".into()] },
                RoleAccess { role: FINANCE_ROLE.into(), operations: vec!["read".into()] },
            ],
            ..AccessSpec::default()
        },
        fields: vec![field_open("po_number"), field_gated(SENSITIVE_FIELD, &[FINANCE_ROLE])],
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

async fn cleanup(admin: &PgPool, pg_schema: &str) {
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
    path: SchemaPath,
    table: String,
}

async fn setup_db(org: &str) -> Option<Harness> {
    let admin_url = admin_url()?;
    let api_url = api_url()?;
    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin_url).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api_url).await.unwrap();
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(org, "supply-chain", "procurement").await.unwrap();
    let path = SchemaPath::new(org, "supply-chain", "procurement", "purchase-order", "v1");
    let plan = velocity_operator::build_ddl(&schema_spec(), &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    let table = format!("{pg_schema}.purchase_order_v1");
    Some(Harness { admin_pool, api_pool, pg_schema, path, table })
}

async fn seed_one_row(h: &Harness) {
    // Admin pool bypasses RLS — used to populate. The reader paths below
    // exercise the actual API and field-strip.
    let sql = format!(
        "INSERT INTO {} (po_number, {SENSITIVE_FIELD}, created_by, updated_by) \
         VALUES ($1, $2, 'seed', 'seed')",
        h.table
    );
    sqlx::query(&sql)
        .bind("PO-001")
        .bind("$42.50")
        .execute(&h.admin_pool)
        .await
        .expect("seed insert");
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

async fn list_as(h: &Harness, identity: Identity) -> (StatusCode, Value) {
    let (schemas, _ready) = SchemaRegistry::new();
    schemas.upsert(ResolvedSchema::from_spec(h.path.clone(), schema_spec()));
    let app_state = AppState::new(Arc::clone(&schemas), h.api_pool.clone());
    let app = router::build(app_state).layer(from_fn(inject_identity(identity)));

    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}",
            h.path.org, h.path.app, h.path.domain, h.path.object, h.path.version
        ))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

#[tokio::test]
async fn list_strips_sensitive_field_for_general_reader() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    seed_one_row(&h).await;

    // General reader → SQL fetched the row; handler ran `strip_for_read`;
    // `unit_cost` should be absent from the response. po_number (no
    // `access` block at all → open) is still present.
    let (status, body) = list_as(&h, ident("alice", &[GENERAL_ROLE])).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    let row = &items[0];
    assert_eq!(row["po_number"], "PO-001");
    assert!(
        row.get(SENSITIVE_FIELD).is_none(),
        "field `{SENSITIVE_FIELD}` must be stripped from the response \
         (entire body for inspection: {body})",
    );

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn list_includes_sensitive_field_for_finance_reader() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    seed_one_row(&h).await;

    // Positive control — without this, the strip test could pass for the
    // wrong reason (e.g. the field was never written to Postgres in the
    // first place, or the handler strips every field unconditionally).
    let (status, body) = list_as(&h, ident("bina", &[FINANCE_ROLE])).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let row = &items[0];
    assert_eq!(row["po_number"], "PO-001");
    assert_eq!(
        row[SENSITIVE_FIELD], "$42.50",
        "finance role must see the sensitive field — strip should be a no-op",
    );

    cleanup(&h.admin_pool, &h.pg_schema).await;
}
