#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! ADR-007 fail-stop gate — the API must refuse to start when the configured
//! Postgres role has BYPASSRLS or SUPERUSER. Without this, row-level security
//! quietly becomes a no-op and every multi-tenant guarantee evaporates.
//!
//!     VELOCITY_API_TEST_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!     cargo test -p velocity-api --test startup_role_gate

use sqlx::postgres::PgPoolOptions;

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

/// Connecting as the superuser must trip the gate. `postgres` has SUPERUSER
/// (and therefore an implicit RLS bypass) — `verify_role_no_bypass` should
/// refuse it.
#[tokio::test]
async fn rejects_superuser_role() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(1).connect(&admin).await.unwrap();
    let err = velocity_core::startup::verify_role_no_bypass(&pool)
        .await
        .expect_err("superuser must be rejected by the ADR-007 gate");
    let msg = format!("{err:#}");
    assert!(msg.contains("ADR-007"), "error must cite ADR-007: {msg}");
    assert!(
        msg.contains("superuser=true") || msg.contains("bypassrls=true"),
        "error must name the offending flag: {msg}"
    );
}

/// The provisioned `velocity_api` role must pass the gate. If this regresses,
/// the operator's role bootstrap has drifted from ADR-007 and every CRUD
/// test downstream would also start failing — keep this assertion narrow.
#[tokio::test]
async fn accepts_velocity_api_role() {
    if admin_url().is_none() {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    }
    let pool = PgPoolOptions::new().max_connections(1).connect(&api_url()).await.unwrap();
    velocity_core::startup::verify_role_no_bypass(&pool)
        .await
        .expect("velocity_api must satisfy NOBYPASSRLS + NOSUPERUSER");
}
