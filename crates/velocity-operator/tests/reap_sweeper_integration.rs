//! Integration tests for the Typesense reap sweeper (Phase 5d residual).
//!
//! Real Postgres, in-process axum mock for Typesense. Skipped unless
//! `VELOCITY_OPERATOR_PG_URL` is set. From repo root:
//!
//! ```sh
//! make up-pg db-bootstrap migrate
//! VELOCITY_OPERATOR_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!   cargo test -p velocity-operator --test reap_sweeper_integration
//! ```

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::delete;
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use velocity_operator::reap_sweeper;
use velocity_typesense::TypesenseClient;

/// `sweep_once` scans the whole `pending_typesense_reaps` table, so
/// concurrent tests would observe each other's queued rows. Serialise
/// them — slow but correct, and this suite has six small cases.
fn suite_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Wipe the queue between tests. Cheap; the table is empty in steady
/// state on a dev DB.
async fn wipe(pool: &PgPool) {
    sqlx::query("DELETE FROM platform.pending_typesense_reaps").execute(pool).await.unwrap();
}

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_OPERATOR_PG_URL").ok()
}

/// Shared state for the axum Typesense mock: count of DELETE
/// requests, and the status code to return.
#[derive(Clone)]
struct MockState {
    delete_count: Arc<AtomicUsize>,
    delete_status: Arc<AtomicU16>,
    last_path: Arc<tokio::sync::Mutex<Option<String>>>,
}

async fn handle_delete(State(state): State<MockState>, Path(name): Path<String>) -> StatusCode {
    state.delete_count.fetch_add(1, Ordering::SeqCst);
    *state.last_path.lock().await = Some(name);
    StatusCode::from_u16(state.delete_status.load(Ordering::SeqCst)).unwrap()
}

/// Spin up an in-process axum mock for Typesense's
/// `DELETE /collections/{name}` endpoint. Returns the bound URL and
/// the shared state for assertions.
async fn spawn_mock() -> (String, MockState) {
    let state = MockState {
        delete_count: Arc::new(AtomicUsize::new(0)),
        delete_status: Arc::new(AtomicU16::new(200)),
        last_path: Arc::new(tokio::sync::Mutex::new(None)),
    };
    let app =
        Router::new().route("/collections/{name}", delete(handle_delete)).with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

async fn connect() -> Option<PgPool> {
    let url = pg_url()?;
    Some(PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap())
}

async fn row_count(pool: &PgPool, concrete: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM platform.pending_typesense_reaps WHERE concrete_name = $1",
    )
    .bind(concrete)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn enqueue_then_sweep_deletes_row_and_calls_typesense() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    wipe(&pool).await;

    let concrete = "test_reap_happy_concrete";
    let alias = "test_reap_happy_alias";

    // grace = -1s so the row is due immediately.
    reap_sweeper::enqueue(&pool, concrete, alias, "uid-1", -1).await.unwrap();
    assert_eq!(row_count(&pool, concrete).await, 1);

    let (base, state) = spawn_mock().await;
    let ts = TypesenseClient::new(base, "test-key").unwrap();

    let reaped = reap_sweeper::sweep_once(&pool, &ts).await.unwrap();
    assert_eq!(reaped, 1, "one due row should be reaped");
    assert_eq!(state.delete_count.load(Ordering::SeqCst), 1);
    assert_eq!(state.last_path.lock().await.as_deref(), Some(concrete));
    assert_eq!(row_count(&pool, concrete).await, 0, "row must be deleted after successful reap");
}

#[tokio::test]
async fn sweep_skips_not_yet_due_rows() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    wipe(&pool).await;

    let concrete = "test_reap_future_concrete";
    let alias = "test_reap_future_alias";

    // grace = 1h → reap_after is well in the future.
    reap_sweeper::enqueue(&pool, concrete, alias, "uid-2", 3600).await.unwrap();

    let (base, state) = spawn_mock().await;
    let ts = TypesenseClient::new(base, "test-key").unwrap();

    let reaped = reap_sweeper::sweep_once(&pool, &ts).await.unwrap();
    assert_eq!(reaped, 0, "no due rows should be reaped");
    assert_eq!(state.delete_count.load(Ordering::SeqCst), 0, "typesense must not be called");
    assert_eq!(row_count(&pool, concrete).await, 1, "future-due row must remain in queue");
}

#[tokio::test]
async fn enqueue_is_idempotent_on_concrete_name() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    wipe(&pool).await;

    let concrete = "test_reap_idem_concrete";
    let alias_a = "test_reap_idem_alias_a";
    let alias_b = "test_reap_idem_alias_b";

    reap_sweeper::enqueue(&pool, concrete, alias_a, "uid-3", 3600).await.unwrap();
    // Second enqueue with same concrete must be a no-op — ON CONFLICT
    // DO NOTHING preserves the original schedule rather than pushing
    // it further out.
    reap_sweeper::enqueue(&pool, concrete, alias_b, "uid-3", 7200).await.unwrap();

    let (count, alias_in_db): (i64, String) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, MIN(alias_name) FROM platform.pending_typesense_reaps \
         WHERE concrete_name = $1",
    )
    .bind(concrete)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "duplicate enqueue must not create a second row");
    assert_eq!(alias_in_db, alias_a, "original alias must be preserved");
}

#[tokio::test]
async fn typesense_failure_leaves_row_for_retry() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    wipe(&pool).await;

    let concrete = "test_reap_fail_concrete";
    let alias = "test_reap_fail_alias";

    reap_sweeper::enqueue(&pool, concrete, alias, "uid-4", -1).await.unwrap();

    let (base, state) = spawn_mock().await;
    state.delete_status.store(500, Ordering::SeqCst);
    let ts = TypesenseClient::new(base, "test-key").unwrap();

    // Whole sweep still returns Ok — the row is left for retry. The
    // count of reaped rows is 0 because nothing was successfully
    // deleted.
    let reaped = reap_sweeper::sweep_once(&pool, &ts).await.unwrap();
    assert_eq!(reaped, 0, "failed delete must not count as reaped");
    assert_eq!(state.delete_count.load(Ordering::SeqCst), 1, "delete was attempted");
    assert_eq!(
        row_count(&pool, concrete).await,
        1,
        "row must remain in queue when Typesense returns an error"
    );

    // Recovery: next sweep with mock back to 200 reaps the row.
    state.delete_status.store(200, Ordering::SeqCst);
    let reaped2 = reap_sweeper::sweep_once(&pool, &ts).await.unwrap();
    assert_eq!(reaped2, 1, "second sweep with healthy Typesense reaps the row");
    assert_eq!(row_count(&pool, concrete).await, 0);
}

#[tokio::test]
async fn typesense_404_is_treated_as_success() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    wipe(&pool).await;

    let concrete = "test_reap_404_concrete";
    let alias = "test_reap_404_alias";

    reap_sweeper::enqueue(&pool, concrete, alias, "uid-5", -1).await.unwrap();

    let (base, state) = spawn_mock().await;
    state.delete_status.store(404, Ordering::SeqCst);
    let ts = TypesenseClient::new(base, "test-key").unwrap();

    // A 404 from Typesense (collection already gone) is the
    // idempotent path — the queue row must still be cleared so the
    // sweeper doesn't retry forever.
    let reaped = reap_sweeper::sweep_once(&pool, &ts).await.unwrap();
    assert_eq!(reaped, 1, "404 counts as successful reap");
    assert_eq!(row_count(&pool, concrete).await, 0);
}

#[tokio::test]
async fn empty_queue_is_a_noop() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let _guard = suite_lock().lock().await;
    wipe(&pool).await;

    let (base, state) = spawn_mock().await;
    let ts = TypesenseClient::new(base, "test-key").unwrap();
    let reaped = reap_sweeper::sweep_once(&pool, &ts).await.unwrap();
    assert_eq!(reaped, 0, "empty queue must reap nothing");
    assert_eq!(state.delete_count.load(Ordering::SeqCst), 0, "no DELETE call");
}
