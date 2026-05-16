//! Integration tests against a live Postgres.
//!
//! Skipped unless `VELOCITY_OPERATOR_PG_URL` is set. From repo root:
//!
//! ```sh
//! make up-pg db-bootstrap migrate
//! VELOCITY_OPERATOR_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!   cargo test -p velocity-operator --test provisioner_integration -- --nocapture
//! ```

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use velocity_operator::provisioner::PostgresProvisioner;

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_OPERATOR_PG_URL").ok()
}

#[tokio::test]
async fn sync_domain_is_idempotent_and_creates_schema_and_roles() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };

    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    // Use a randomized suffix so concurrent runs don't collide.
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let expected_schema = format!("{}_supply_chain_procurement", org);

    // Clean slate.
    cleanup(&pool, &expected_schema).await;

    let prov = PostgresProvisioner::new(pool.clone());

    // First run — creates everything.
    let p1 = prov.sync_domain(&org, app, domain).await.unwrap();
    assert_eq!(p1.pg_schema, expected_schema);
    assert_eq!(p1.pg_roles.len(), 3);

    // Second run — idempotent, same result, no error.
    let p2 = prov.sync_domain(&org, app, domain).await.unwrap();
    assert_eq!(p1.pg_schema, p2.pg_schema);
    assert_eq!(p1.pg_roles, p2.pg_roles);

    // Verify in pg_catalog.
    let schema_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
            .bind(&expected_schema)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(schema_exists, "schema should exist");

    for role in &p1.pg_roles {
        let row = sqlx::query(
            "SELECT rolbypassrls, rolsuper, rolcanlogin FROM pg_roles WHERE rolname = $1",
        )
        .bind(role)
        .fetch_optional(&pool)
        .await
        .unwrap();
        let row = row.unwrap_or_else(|| panic!("role {role} should exist"));
        let bypass: bool = row.try_get("rolbypassrls").unwrap();
        let superuser: bool = row.try_get("rolsuper").unwrap();
        let canlogin: bool = row.try_get("rolcanlogin").unwrap();
        assert!(!bypass, "{role} must be NOBYPASSRLS");
        assert!(!superuser, "{role} must be NOSUPERUSER");
        assert!(!canlogin, "{role} must be NOLOGIN");
    }

    // velocity_api must have USAGE on the new schema.
    let has_usage: bool =
        sqlx::query_scalar("SELECT has_schema_privilege('velocity_api', $1, 'USAGE')")
            .bind(&expected_schema)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(has_usage, "velocity_api should have USAGE on the new schema");

    cleanup(&pool, &expected_schema).await;
}

#[tokio::test]
async fn sync_domain_rejects_invalid_identifiers() {
    let Some(url) = pg_url() else { return };
    let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let prov = PostgresProvisioner::new(pool);

    // Tries to embed a DDL injection.
    let err = prov.sync_domain("acme", "supply;DROP TABLE x;--", "procurement").await;
    assert!(err.is_err(), "injection-shaped name must be rejected");
}

async fn cleanup(pool: &sqlx::PgPool, schema: &str) {
    let stmt = format!("DROP SCHEMA IF EXISTS {schema} CASCADE");
    let _ = sqlx::query(&stmt).execute(pool).await;
    for suffix in ["_reader", "_writer", "_admin"] {
        let role = format!("{schema}{suffix}");
        let drop_sql = format!("DROP ROLE IF EXISTS {role}");
        let _ = sqlx::query(&drop_sql).execute(pool).await;
    }
}
