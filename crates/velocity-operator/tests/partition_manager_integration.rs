//! Integration tests for the event-log partition manager (Phase 3.8).
//!
//! Skipped unless `VELOCITY_OPERATOR_PG_URL` is set. From repo root:
//!
//! ```sh
//! make up-pg db-bootstrap migrate
//! VELOCITY_OPERATOR_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!   cargo test -p velocity-operator --test partition_manager_integration
//! ```

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use chrono::{Datelike, Months, Utc};
use sqlx::postgres::PgPoolOptions;
use velocity_operator::partition_manager;

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_OPERATOR_PG_URL").ok()
}

async fn partition_exists(pool: &sqlx::PgPool, name: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'platform' AND c.relname = $1)",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn tick_ensures_current_and_next_month_partitions_exist() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    // Tick once. The base migration already creates current + next, so
    // the first tick after a clean migrate should be a no-op (empty
    // returned vec) — its purpose is to PROVE that the manager
    // converges, not that it always does work.
    let _ = partition_manager::tick(&pool).await.unwrap();

    // Whatever was on disk before, current month and next month MUST
    // exist after a successful tick.
    let today = Utc::now().date_naive();
    let current = format!("event_log_{:04}_{:02}", today.year(), today.month());
    let next_date = today.with_day(1).unwrap().checked_add_months(Months::new(1)).unwrap();
    let next = format!("event_log_{:04}_{:02}", next_date.year(), next_date.month());

    assert!(partition_exists(&pool, &current).await, "current-month partition {current} missing");
    assert!(partition_exists(&pool, &next).await, "next-month partition {next} missing");
}

#[tokio::test]
async fn tick_is_idempotent() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    // First tick may create partitions (if a previous test ran in a
    // fresh month) or do nothing. Second tick must do nothing —
    // partitions are already present.
    let _ = partition_manager::tick(&pool).await.unwrap();
    let second = partition_manager::tick(&pool).await.unwrap();

    assert!(second.is_empty(), "second tick must not create anything; created: {second:?}");
}

#[tokio::test]
async fn tick_creates_missing_next_month_partition() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    // Force the worst case: detach + drop next-month's partition, then
    // tick. The manager must put it back. We use DROP TABLE — the
    // bootstrap migration uses the same path so this matches what would
    // happen if an operator hand-deleted one.
    let today = Utc::now().date_naive();
    let next_date = today.with_day(1).unwrap().checked_add_months(Months::new(1)).unwrap();
    let next_name = format!("event_log_{:04}_{:02}", next_date.year(), next_date.month());

    // First ensure it exists, then drop, then tick.
    partition_manager::tick(&pool).await.unwrap();
    sqlx::query(&format!("DROP TABLE IF EXISTS platform.{next_name}"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !partition_exists(&pool, &next_name).await,
        "test precondition: partition must be dropped"
    );

    let created = partition_manager::tick(&pool).await.unwrap();
    assert!(
        created.iter().any(|n| n == &next_name),
        "tick must report the recreated partition; got: {created:?}"
    );
    assert!(partition_exists(&pool, &next_name).await, "tick must recreate {next_name}");
}
