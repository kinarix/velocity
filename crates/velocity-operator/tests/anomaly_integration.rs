//! Phase 6c — DB-backed tests for the anomaly scanner.
//!
//! Seeds rows directly into `platform.audit_log` via the
//! `platform.audit_insert` SP, drives `sweep_once`, and asserts the
//! resulting `anomaly_alerts` rows + dedupe behaviour.
//!
//! Skipped unless `VELOCITY_OPERATOR_PG_URL` is set. From repo root:
//!
//! ```sh
//! make up-pg db-bootstrap migrate
//! VELOCITY_OPERATOR_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!   cargo test -p velocity-operator --test anomaly_integration
//! ```

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use std::sync::OnceLock;

use chrono::{DateTime, TimeZone, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;
use velocity_operator::anomaly::{self, BULK_READER_THRESHOLD, REPEATED_DENIALS_THRESHOLD};

/// The sweep scans `platform.audit_log` cluster-wide and the dedupe
/// unique index spans the whole table — parallel tests would step on
/// each other. Serialise.
fn suite_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_OPERATOR_PG_URL").ok()
}

async fn connect() -> Option<PgPool> {
    let url = pg_url()?;
    Some(PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap())
}

/// Reset DB state to a known-empty starting point. We hold the
/// suite-wide lock, so wiping `audit_log` + `anomaly_alerts` whole is
/// safe — and necessary, because the sweep scans the entire table and
/// would otherwise pick up unrelated rows from other tests / dev work.
///
/// The audit chain is reset alongside the log wipe; tests below don't
/// exercise `audit_verify_window`, so chain integrity is not relevant.
async fn reset(pool: &PgPool, _marker: &str) {
    sqlx::query("DELETE FROM platform.anomaly_alerts").execute(pool).await.unwrap();
    sqlx::query("DELETE FROM platform.audit_log").execute(pool).await.unwrap();
    sqlx::query("UPDATE platform.audit_chain_state SET last_hash = NULL WHERE id = 1")
        .execute(pool)
        .await
        .unwrap();

    // Pin the watermark before any test row's `occurred_at` so the next
    // sweep takes the composite-cursor branch (not the "first sweep =
    // last 5 min" backstop) and reliably picks up our backdated seeds.
    // Production gets the 5-min backstop so a fresh operator doesn't
    // backfill years of history; here we want determinism.
    sqlx::query(
        "UPDATE platform.anomaly_scan_state \
         SET last_scanned_occurred_at = TIMESTAMPTZ '2020-01-01 00:00:00Z', \
             last_scanned_id = '00000000-0000-0000-0000-000000000000', \
             last_scanned_at = NULL \
         WHERE id = 1",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Insert one audit row at a chosen wall-clock time via the SP.
/// Bypasses the chain serialisation lock contention by accepting the
/// SP's `now()` for hash inputs — we patch occurred_at after the fact
/// so we can drive after-hours scenarios deterministically.
async fn seed(
    pool: &PgPool,
    actor: &str,
    action: &str,
    outcome: &str,
    schema_org: Option<&str>,
    occurred_at: DateTime<Utc>,
) {
    // The SP returns the new row id. We then UPDATE occurred_at to the
    // requested timestamp — the chain hash is computed over the SP's
    // wall clock, but the anomaly scanner reads occurred_at independently,
    // so backdating is safe for the scanner's window logic. (Audit
    // verification would notice; tests below don't exercise verify.)
    let id: uuid::Uuid =
        sqlx::query_scalar("SELECT platform.audit_insert($1, $2, $3, $4, NULL, '{}'::jsonb)")
            .bind(actor)
            .bind(action)
            .bind(outcome)
            .bind(schema_org)
            .fetch_one(pool)
            .await
            .unwrap();

    sqlx::query("UPDATE platform.audit_log SET occurred_at = $1 WHERE id = $2")
        .bind(occurred_at)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn alerts_for(pool: &PgPool, marker: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT rule, actor FROM platform.anomaly_alerts \
         WHERE actor LIKE $1 || '%' ORDER BY rule, actor",
    )
    .bind(marker)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn bulk_reader_alert_lands_in_db() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    let marker = "test_bulk_";
    reset(&pool, marker).await;

    // Mid-day UTC so we don't accidentally trip after_hours too.
    let mid_day = Utc.with_ymd_and_hms(2026, 5, 19, 14, 0, 0).single().unwrap();
    for _ in 0..BULK_READER_THRESHOLD {
        seed(&pool, &format!("{marker}alice"), "read", "success", None, mid_day).await;
    }

    let n = anomaly::sweep_once(&pool, None).await.unwrap();
    assert_eq!(n, 1, "exactly one bulk_reader alert inserted");

    let alerts = alerts_for(&pool, marker).await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].0, "bulk_reader");
    assert_eq!(alerts[0].1.as_deref(), Some("test_bulk_alice"));

    reset(&pool, marker).await;
}

#[tokio::test]
async fn after_hours_write_alerts_and_dedupes_on_second_sweep() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    let marker = "test_after_";
    reset(&pool, marker).await;

    // 03:00 UTC on a weekday — squarely after-hours.
    let early = Utc.with_ymd_and_hms(2026, 5, 19, 3, 0, 0).single().unwrap();
    seed(&pool, &format!("{marker}ops"), "update", "success", Some("o/a/d/x/v1"), early).await;

    let n1 = anomaly::sweep_once(&pool, None).await.unwrap();
    assert_eq!(n1, 1, "after_hours alert from first sweep");

    // Second seed + sweep within the same hour window — dedupe must
    // drop the new detection because the unique index buckets by
    // date_trunc('hour', detected_at). The watermark also skips the
    // first row, but a fresh seed within the same hour exercises the
    // dedupe path.
    let early_2 = Utc.with_ymd_and_hms(2026, 5, 19, 3, 5, 0).single().unwrap();
    seed(&pool, &format!("{marker}ops"), "delete", "success", Some("o/a/d/x/v1"), early_2).await;

    let n2 = anomaly::sweep_once(&pool, None).await.unwrap();
    assert_eq!(n2, 0, "second after_hours detection deduped by hourly unique index");

    let alerts = alerts_for(&pool, marker).await;
    assert_eq!(alerts.len(), 1, "still exactly one after_hours row in DB");
    assert_eq!(alerts[0].0, "after_hours");

    reset(&pool, marker).await;
}

#[tokio::test]
async fn repeated_denials_alert_lands_in_db() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    let marker = "test_denial_";
    reset(&pool, marker).await;

    let mid_day = Utc.with_ymd_and_hms(2026, 5, 19, 14, 30, 0).single().unwrap();
    for _ in 0..REPEATED_DENIALS_THRESHOLD {
        seed(&pool, &format!("{marker}evil"), "create", "denied", None, mid_day).await;
    }

    let n = anomaly::sweep_once(&pool, None).await.unwrap();
    assert_eq!(n, 1);

    let alerts = alerts_for(&pool, marker).await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].0, "repeated_denials");
    assert_eq!(alerts[0].1.as_deref(), Some("test_denial_evil"));

    reset(&pool, marker).await;
}

#[tokio::test]
async fn watermark_advances_so_next_sweep_skips_processed_window() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    let marker = "test_wm_";
    reset(&pool, marker).await;

    // First sweep: seed enough reads to trip bulk_reader, sweep.
    let mid_day = Utc.with_ymd_and_hms(2026, 5, 19, 14, 0, 0).single().unwrap();
    for _ in 0..BULK_READER_THRESHOLD {
        seed(&pool, &format!("{marker}a"), "read", "success", None, mid_day).await;
    }
    let n1 = anomaly::sweep_once(&pool, None).await.unwrap();
    assert_eq!(n1, 1);

    // Second sweep with NO new rows — watermark must skip the prior
    // window entirely, so no new detections, no DB writes.
    let n2 = anomaly::sweep_once(&pool, None).await.unwrap();
    assert_eq!(n2, 0, "watermark should prevent re-evaluation of prior window");

    reset(&pool, marker).await;
}
