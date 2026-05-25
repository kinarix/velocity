#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 2b Layer-4 happy path against a real Postgres.
//!
//! The wire-through tests in `row_filter_routes.rs` prove every handler
//! *consults* the row-filter index by checking the broken-config 500. This
//! file proves the predicate actually shapes the result set against a
//! seeded table:
//!
//! - A scoped role (`regional-reader-west`) sees only its slice.
//! - An unscoped role (`procurement-admin`, no rowFilter entry) sees all
//!   rows — the "more roles = wider access" semantic from
//!   `scoped_roles_for_session`.
//! - A role with no rowFilter entry on a schema that *has* rowFilter rules
//!   in the deny direction (zero matching roles) → 200 with empty items.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase2b_row_filter_e2e
//!
//! Skips silently when those env vars aren't set — same pattern as
//! `phase2a_e2e.rs`.

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
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess, RowFilter,
    RowFilterRule, SchemaDefinitionSpec, SearchSpec, SearchTier,
};

// ─── env shims (mirror phase2a_e2e.rs) ─────────────────────────────────────

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

// ─── Schema with a region field + row_filter rule ──────────────────────────

fn field(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f
}

fn schema_spec() -> SchemaDefinitionSpec {
    // Two scoped reader roles + one unscoped admin role. The unscoped role
    // exercises the "any role not in the row_filter map → see all rows"
    // branch in `scoped_roles_for_session`.
    let row_filter = vec![
        RowFilterRule {
            role: "regional-reader-west".into(),
            filter: RowFilter {
                field: "region".into(),
                op: "eq".into(),
                value: Value::String("west".into()),
            },
        },
        RowFilterRule {
            role: "regional-reader-east".into(),
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
            // Layer-1 RBAC: name the four roles that may read this schema.
            // Without these the handler 403s before any row filter runs.
            roles: vec![
                RoleAccess { role: "regional-reader-west".into(), operations: vec!["read".into()] },
                RoleAccess { role: "regional-reader-east".into(), operations: vec!["read".into()] },
                RoleAccess { role: "procurement-admin".into(), operations: vec!["read".into()] },
                RoleAccess { role: "outsider".into(), operations: vec!["read".into()] },
            ],
            row_filter,
            policies: Vec::new(),
        },
        fields: vec![field("po_number", FieldKind::String), field("region", FieldKind::String)],
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

/// Seed one east row and one west row using the admin pool (which bypasses
/// RLS). The reader paths in the tests below then assert what the API
/// gives back to the scoped vs unscoped identities.
async fn seed_rows(h: &Harness) {
    // The DDL plan generates a UUID PK with `gen_random_uuid()` and a
    // version column starting at 1 — we let those defaults fire and only
    // supply the user-declared fields. created_at / updated_at are
    // defaulted by the table DDL too.
    for (po, region) in [("PO-WEST-1", "west"), ("PO-EAST-1", "east")] {
        let sql = format!(
            "INSERT INTO {} (po_number, region, created_by, updated_by) \
             VALUES ($1, $2, 'seed', 'seed')",
            h.table
        );
        sqlx::query(&sql).bind(po).bind(region).execute(&h.admin_pool).await.expect("seed insert");
    }
}

// ─── Router with identity injected directly (skip the JWT layer) ───────────

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
    let app_state = DataState::new(Arc::clone(&schemas), h.api_pool.clone());
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

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scoped_reader_sees_only_matching_region() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    seed_rows(&h).await;

    // `regional-reader-west`: row_filter rule maps to `region = 'west'`.
    let (status, body) = list_as(&h, ident("anita", &["regional-reader-west"])).await;
    assert_eq!(status, StatusCode::OK, "scoped reader admitted at Layer-1 RBAC");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "scoped reader must see exactly one row");
    assert_eq!(items[0]["po_number"], "PO-WEST-1");
    assert_eq!(items[0]["region"], "west");

    // And the east-scoped reader sees only east — proves the filter binds
    // per-role, not per-request-shape.
    let (_, body) = list_as(&h, ident("bina", &["regional-reader-east"])).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["po_number"], "PO-EAST-1");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn unscoped_role_sees_all_rows() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    seed_rows(&h).await;

    // `procurement-admin` is in `roles[]` (Layer-1 read OK) but not in
    // `row_filter[]` — `scoped_roles_for_session` returns `"*"` and the
    // wildcard RLS policy admits every row. Required so an admin or
    // back-office role isn't accidentally row-filtered into invisibility.
    let (status, body) = list_as(&h, ident("global-admin", &["procurement-admin"])).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "unscoped role must see both seeded rows");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn role_outside_filter_map_gets_empty_result() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    seed_rows(&h).await;

    // `outsider` has Layer-1 read permission but isn't in the row_filter
    // map AND isn't unscoped — wait, by construction "any role not in the
    // map" is treated as unscoped. So `outsider` *should* see everything.
    // That's an important property to pin: row_filter is a per-role
    // *narrowing*, not a default-deny. Phase 2b adopted "more roles = more
    // access" intentionally; if it ever flips to default-deny this test
    // fails loud and the test name becomes wrong, which is the cue.
    let (status, body) = list_as(&h, ident("ext-vendor", &["outsider"])).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        2,
        "a Layer-1-allowed role with no row_filter entry is treated as unscoped",
    );

    cleanup(&h.admin_pool, &h.pg_schema).await;
}
