#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 2b Layer-7 acceptance: "Postgres-direct query as `velocity_api`
//! → RLS enforced."
//!
//! This bypasses the API entirely — connects to Postgres as the
//! `velocity_api` role (the non-superuser, `NOBYPASSRLS=true` role per
//! ADR-007), sets `app.scoped_roles` directly, and asserts that
//! `SELECT * FROM …` returns exactly what the API would return for the
//! same identity.
//!
//! Why this matters: the API server is one of several things that talks
//! to this database (also the audit reader, ad-hoc operator queries,
//! analytics dumps, and any future read replica). If Layer-7 RLS isn't
//! actually enforced on the connection role, a future tool that skips
//! the `scoped_roles_for_session` helper would silently leak every row to
//! every consumer. The API-layer tests can't catch that — only a
//! direct-DB test can.
//!
//! The three branches mirror `scoped_roles_for_session`:
//! - `app.scoped_roles = '*'`     → wildcard policy admits all rows
//! - `app.scoped_roles = 'role-x'` → per-role policy filters via predicate
//! - `app.scoped_roles = ''`       → zero matching policies → empty result
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase2b_rls_direct
//! Skips when env unset (admin URL alone isn't enough — we need a
//! non-superuser pool too).

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use velocity_operator::PostgresProvisioner;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, RowFilter, RowFilterRule,
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
            row_filter: vec![
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
            ],
            ..AccessSpec::default()
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

    for (po, region) in [("PO-WEST-1", "west"), ("PO-EAST-1", "east")] {
        let sql = format!(
            "INSERT INTO {pg_schema}.purchase_order_v1 \
             (po_number, region, created_by, updated_by) \
             VALUES ($1, $2, 'seed', 'seed')",
        );
        sqlx::query(&sql).bind(po).bind(region).execute(&admin_pool).await.expect("seed insert");
    }

    let table = format!("{pg_schema}.purchase_order_v1");
    Some(Harness { admin_pool, api_pool, pg_schema, table })
}

/// `BYPASSRLS=false` is the invariant the operator startup check enforces
/// (`velocity_operator/src/startup.rs`). We assert it here too — if a
/// future migration ever flips this attribute (e.g., someone runs
/// `ALTER ROLE velocity_api BYPASSRLS` to debug something and forgets to
/// revert) then every RLS-protected table becomes wide-open. This test
/// belongs in the path-of-truth, not just the boot sequence.
async fn assert_api_role_does_not_bypass_rls(api_pool: &PgPool) {
    let bypass: bool =
        sqlx::query_scalar("SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(api_pool)
            .await
            .expect("query current_user rolbypassrls");
    assert!(!bypass, "velocity_api role has BYPASSRLS=true — Layer-7 cannot be enforced",);
}

/// Read every row visible under the supplied `app.scoped_roles` setting.
/// Uses `SET LOCAL` inside a transaction so the setting doesn't leak to
/// other connections in the pool — same discipline the API uses in
/// `with_session_context`.
async fn rows_under_scope(api_pool: &PgPool, table: &str, scope: &str) -> Vec<(String, String)> {
    let mut tx = api_pool.begin().await.expect("begin");
    // `SET LOCAL` takes a literal, not a parameter — we go through
    // sqlx::query with the value pre-escaped by quote_literal.
    let escaped = scope.replace('\'', "''");
    sqlx::query(&format!("SET LOCAL app.scoped_roles = '{escaped}'"))
        .execute(&mut *tx)
        .await
        .expect("set scoped_roles");
    let rows = sqlx::query(&format!("SELECT po_number, region FROM {table} ORDER BY po_number"))
        .fetch_all(&mut *tx)
        .await
        .expect("select");
    let out: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| (r.get::<String, _>("po_number"), r.get::<String, _>("region")))
        .collect();
    tx.commit().await.expect("commit");
    out
}

#[tokio::test]
async fn rls_admits_only_west_for_west_scoped_role() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    assert_api_role_does_not_bypass_rls(&h.api_pool).await;

    let rows = rows_under_scope(&h.api_pool, &h.table, "regional-reader-west").await;
    assert_eq!(rows.len(), 1, "RLS must hide east rows from west-scoped role");
    assert_eq!(rows[0], ("PO-WEST-1".into(), "west".into()));

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn rls_admits_all_rows_for_wildcard_scope() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    assert_api_role_does_not_bypass_rls(&h.api_pool).await;

    // `*` is the sentinel the API emits when the actor carries at least
    // one role outside the row_filter map. The wildcard policy admits
    // every row.
    let rows = rows_under_scope(&h.api_pool, &h.table, "*").await;
    assert_eq!(rows.len(), 2, "wildcard scope must admit both rows");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn rls_denies_all_rows_for_empty_scope() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    assert_api_role_does_not_bypass_rls(&h.api_pool).await;

    // Empty string is the *deny* sentinel — the API emits this when the
    // schema declares row filters but the actor has zero matching roles
    // (the "zero-role identity" branch in scoped_roles_for_session). RLS
    // must match the SQL-fragment behavior of `(false)`.
    let rows = rows_under_scope(&h.api_pool, &h.table, "").await;
    assert!(rows.is_empty(), "empty scope must produce zero rows under RLS");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn rls_denies_all_rows_when_setting_absent() {
    // Belt-and-braces: if a caller forgets to issue SET LOCAL at all,
    // `current_setting('app.scoped_roles', true)` returns NULL. Every
    // policy's USING clause compares NULL = '*' (NULL) or
    // NULL = ANY(string_to_array(NULL, ',')) (NULL) — neither admits, so
    // the row is hidden. Without this, "forgot the prelude" would fail
    // open instead of closed, which is the opposite of the contract.
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };
    assert_api_role_does_not_bypass_rls(&h.api_pool).await;

    let mut tx = h.api_pool.begin().await.unwrap();
    // No SET LOCAL — just SELECT.
    let rows = sqlx::query(&format!("SELECT po_number FROM {} ORDER BY po_number", h.table))
        .fetch_all(&mut *tx)
        .await
        .expect("select");
    tx.commit().await.unwrap();
    assert!(rows.is_empty(), "missing scoped_roles setting must fail closed");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}
