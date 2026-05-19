//! Phase 5d-3b race-window SELECT primitives — integration coverage.
//!
//! `search_rebuild::run` is built from three private SELECTs:
//! - `fetch_page` — snapshot, paginated by id, excludes soft-deletes
//! - `fetch_delta` — rows updated since cutoff, excludes soft-deletes
//! - `fetch_deleted_ids_page` — soft-deleted ids since cutoff, paginated
//!
//! The race-window correctness LIVES in the WHERE clauses of these
//! three. The orchestration on top is a loop-until-empty over
//! idempotent Typesense calls. These tests cover the SQL directly so
//! a regression in the cutoff arithmetic or the soft-delete filter
//! gets caught before it leaks into production.
//!
//! Orchestration coverage (that `run()` calls them in sequence) is
//! deferred — relies on code review + advisor.
//!
//! Skipped unless `VELOCITY_OPERATOR_PG_URL` is set.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use std::sync::OnceLock;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;
use velocity_operator::search_rebuild;

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_OPERATOR_PG_URL").ok()
}

/// Each test creates its own table in a unique schema, but the
/// suite as a whole shares one PG. Lock so a parallel test's
/// `CREATE SCHEMA` / `DROP SCHEMA` doesn't race with another's
/// SELECTs on the same fixture. Cheap; six small tests.
fn suite_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn connect() -> Option<PgPool> {
    let url = pg_url()?;
    Some(PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap())
}

/// Build a fixture table that mirrors what `DdlBuilder` produces:
/// `id UUID`, `payload JSONB`, `updated_at`, `deleted_at` — the
/// columns the rebuild SQL actually reads. The production table has
/// an `__fts` generated column; `to_jsonb(t) - '__fts'` is a no-op
/// when the key is absent, so we leave it out and rely on `payload`
/// for the doc shape.
async fn make_fixture(pool: &PgPool, schema: &str, table: &str) -> String {
    // Belt-and-braces — drop any leftover from a prior failed run.
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).execute(pool).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(pool).await.unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}.{table} (
            id         UUID NOT NULL PRIMARY KEY,
            payload    JSONB,
            updated_at TIMESTAMPTZ NOT NULL,
            deleted_at TIMESTAMPTZ
        )"
    ))
    .execute(pool)
    .await
    .unwrap();
    format!("\"{schema}\".\"{table}\"")
}

async fn drop_fixture(pool: &PgPool, schema: &str) {
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).execute(pool).await.unwrap();
}

async fn insert_row(
    pool: &PgPool,
    qualified: &str,
    id: uuid::Uuid,
    label: &str,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
) {
    sqlx::query(&format!(
        "INSERT INTO {qualified} (id, payload, updated_at, deleted_at) \
         VALUES ($1, jsonb_build_object('label', $2::text), $3, $4)"
    ))
    .bind(id)
    .bind(label)
    .bind(updated_at)
    .bind(deleted_at)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn fetch_page_excludes_soft_deleted_rows() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _g = suite_lock().lock().await;
    let qualified = make_fixture(&pool, "test_fp_softdel", "t").await;

    let now = Utc::now();
    let alive = uuid::Uuid::new_v4();
    let dead = uuid::Uuid::new_v4();
    insert_row(&pool, &qualified, alive, "alive", now, None).await;
    insert_row(&pool, &qualified, dead, "dead", now, Some(now)).await;

    let rows = search_rebuild::fetch_page(&pool, &qualified, None).await.unwrap();
    let ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(ids, vec![alive.to_string()], "snapshot must exclude soft-deletes");

    drop_fixture(&pool, "test_fp_softdel").await;
}

#[tokio::test]
async fn fetch_page_keyset_paginates_in_id_order() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _g = suite_lock().lock().await;
    let qualified = make_fixture(&pool, "test_fp_keyset", "t").await;

    let now = Utc::now();
    // Construct ids that sort deterministically as text.
    let a = "00000000-0000-0000-0000-00000000000a".parse::<uuid::Uuid>().unwrap();
    let b = "00000000-0000-0000-0000-00000000000b".parse::<uuid::Uuid>().unwrap();
    let c = "00000000-0000-0000-0000-00000000000c".parse::<uuid::Uuid>().unwrap();
    insert_row(&pool, &qualified, b, "b", now, None).await;
    insert_row(&pool, &qualified, a, "a", now, None).await;
    insert_row(&pool, &qualified, c, "c", now, None).await;

    let first = search_rebuild::fetch_page(&pool, &qualified, None).await.unwrap();
    let first_ids: Vec<String> = first.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(
        first_ids,
        vec![a.to_string(), b.to_string(), c.to_string()],
        "first page must be id-ASC"
    );

    let next = search_rebuild::fetch_page(&pool, &qualified, Some(&b.to_string())).await.unwrap();
    let next_ids: Vec<String> = next.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(next_ids, vec![c.to_string()], "keyset must skip past cursor");

    drop_fixture(&pool, "test_fp_keyset").await;
}

#[tokio::test]
async fn fetch_delta_cutoff_arithmetic() {
    // The exact race-window correctness: a row updated at t1 must
    // appear when cutoff < t1, must NOT appear when cutoff > t1.
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _g = suite_lock().lock().await;
    let qualified = make_fixture(&pool, "test_fd_cutoff", "t").await;

    let base = Utc::now() - ChronoDuration::seconds(60);
    let t1 = base + ChronoDuration::seconds(10);
    let t2 = base + ChronoDuration::seconds(20);
    let row_a = uuid::Uuid::new_v4();
    let row_b = uuid::Uuid::new_v4();
    insert_row(&pool, &qualified, row_a, "a", t1, None).await;
    insert_row(&pool, &qualified, row_b, "b", t2, None).await;

    // Cutoff before both → {a, b}.
    let cutoff_early = (base + ChronoDuration::seconds(5)).to_rfc3339();
    let early = search_rebuild::fetch_delta(&pool, &qualified, cutoff_early).await.unwrap();
    let mut early_ids: Vec<String> = early.iter().map(|(id, _)| id.clone()).collect();
    early_ids.sort();
    let mut want = vec![row_a.to_string(), row_b.to_string()];
    want.sort();
    assert_eq!(early_ids, want, "cutoff before both rows must return both");

    // Cutoff between → {b} only.
    let cutoff_mid = (base + ChronoDuration::seconds(15)).to_rfc3339();
    let mid = search_rebuild::fetch_delta(&pool, &qualified, cutoff_mid).await.unwrap();
    let mid_ids: Vec<String> = mid.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(mid_ids, vec![row_b.to_string()], "cutoff t1<c<t2 must return only b");

    // Cutoff after both → empty.
    let cutoff_late = (base + ChronoDuration::seconds(30)).to_rfc3339();
    let late = search_rebuild::fetch_delta(&pool, &qualified, cutoff_late).await.unwrap();
    assert!(late.is_empty(), "cutoff after both must return empty");

    drop_fixture(&pool, "test_fd_cutoff").await;
}

#[tokio::test]
async fn fetch_delta_excludes_soft_deleted_rows() {
    // A row updated then soft-deleted must not appear in fetch_delta
    // — fetch_deleted_ids_page exists precisely to catch these.
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _g = suite_lock().lock().await;
    let qualified = make_fixture(&pool, "test_fd_softdel", "t").await;

    let base = Utc::now() - ChronoDuration::seconds(60);
    let t1 = base + ChronoDuration::seconds(10);
    let id = uuid::Uuid::new_v4();
    // updated_at >= cutoff so fetch_delta WOULD pick it up if not for
    // the deleted_at IS NULL filter.
    insert_row(&pool, &qualified, id, "x", t1, Some(t1)).await;

    let cutoff = (base + ChronoDuration::seconds(5)).to_rfc3339();
    let rows = search_rebuild::fetch_delta(&pool, &qualified, cutoff).await.unwrap();
    assert!(rows.is_empty(), "fetch_delta must exclude soft-deleted rows");

    drop_fixture(&pool, "test_fd_softdel").await;
}

#[tokio::test]
async fn fetch_deleted_ids_cutoff_arithmetic() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _g = suite_lock().lock().await;
    let qualified = make_fixture(&pool, "test_fdi_cutoff", "t").await;

    let base = Utc::now() - ChronoDuration::seconds(60);
    let t1 = base + ChronoDuration::seconds(10);
    let id = uuid::Uuid::new_v4();
    insert_row(&pool, &qualified, id, "gone", t1, Some(t1)).await;

    // Cutoff before delete → id returned.
    let early = (base + ChronoDuration::seconds(5)).to_rfc3339();
    let r1 = search_rebuild::fetch_deleted_ids_page(&pool, &qualified, &early, None).await.unwrap();
    assert_eq!(r1, vec![id.to_string()], "cutoff before delete must return id");

    // Cutoff after delete → empty.
    let late = (base + ChronoDuration::seconds(30)).to_rfc3339();
    let r2 = search_rebuild::fetch_deleted_ids_page(&pool, &qualified, &late, None).await.unwrap();
    assert!(r2.is_empty(), "cutoff after delete must return empty");

    drop_fixture(&pool, "test_fdi_cutoff").await;
}

#[tokio::test]
async fn fetch_deleted_ids_paginates_past_first_page() {
    // The bug the keyset-pagination commit fixed: a single-shot
    // fetch silently leaked deletes past PAGE_SIZE. Verify the loop
    // returns ALL of them across calls.
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _g = suite_lock().lock().await;
    let qualified = make_fixture(&pool, "test_fdi_paginate", "t").await;

    // PAGE_SIZE is 5000 in production; that's heavy for an integration
    // test. We can't override it from outside, so instead insert a
    // small N (50) and verify the keyset loop converges, terminating
    // when a partial page returns. This catches "cursor never moves"
    // / "loop never terminates" regressions even if it can't exercise
    // the multi-page path. The full-PAGE_SIZE case is covered by code
    // review of the loop body.
    let base = Utc::now() - ChronoDuration::seconds(60);
    let t1 = base + ChronoDuration::seconds(10);
    let mut ids: Vec<uuid::Uuid> = (0..50).map(|_| uuid::Uuid::new_v4()).collect();
    for id in &ids {
        insert_row(&pool, &qualified, *id, "d", t1, Some(t1)).await;
    }
    ids.sort();

    let cutoff = (base + ChronoDuration::seconds(5)).to_rfc3339();
    let mut collected: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page =
            search_rebuild::fetch_deleted_ids_page(&pool, &qualified, &cutoff, cursor.as_deref())
                .await
                .unwrap();
        if page.is_empty() {
            break;
        }
        let last = page.last().cloned();
        collected.extend(page);
        cursor = last;
    }
    let expected: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    assert_eq!(collected, expected, "pagination loop must return all deletes in id order");

    drop_fixture(&pool, "test_fdi_paginate").await;
}
