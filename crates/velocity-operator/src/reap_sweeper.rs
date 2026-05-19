//! Background sweep for the Typesense blue-green reap queue.
//!
//! Phase 5d-3c persistent-reap. After a successful alias flip the
//! rebuild task INSERTs a row into `platform.pending_typesense_reaps`
//! with `reap_after = now() + grace`. This module periodically scans
//! due rows, deletes the old concrete from Typesense, and deletes
//! the row from the queue.
//!
//! Crash-safety: the rebuild task no longer needs to keep an in-task
//! sleep alive — the queue row survives operator restarts, so a
//! restart during the grace window picks up where it left off.
//!
//! Multi-replica safety: the SELECT uses `FOR UPDATE SKIP LOCKED`
//! so two operators (leader-elected for reconciles, but DB-coordinated
//! for background sweeps) don't race on the same row. The DELETE
//! lives in the same transaction as the SELECT — if the operator
//! crashes between the Typesense delete and the row delete, the row
//! is unlocked by the transaction abort and the next sweep retries.
//! Idempotent on the Typesense side because `delete_collection`
//! returns Ok on 404.

use sqlx::PgPool;
use velocity_typesense::TypesenseClient;

/// How often to wake up and scan for due rows. Aligned with the
/// shortest plausible grace period (300s) — picking 60s means a
/// reap is at most ~60s late on its scheduled time.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// Max rows reaped per sweep tick. Bounds the work each tick does so
/// a backlog after a long outage doesn't monopolise the DB connection.
/// The remainder rolls into the next tick.
const SWEEP_BATCH: i64 = 32;

/// Run the sweeper loop forever. Detached from controllers — the only
/// shared state is `ctx.pg` and `ctx.typesense`.
pub async fn run(pool: PgPool, typesense: TypesenseClient) {
    tracing::info!(
        interval_secs = SWEEP_INTERVAL.as_secs(),
        batch = SWEEP_BATCH,
        "typesense reap sweeper started"
    );
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        if let Err(e) = sweep_once(&pool, &typesense).await {
            tracing::warn!(error = %e, "reap sweep tick failed; retrying next interval");
        }
    }
}

/// One sweep tick. Returns the number of rows successfully reaped.
pub async fn sweep_once(pool: &PgPool, typesense: &TypesenseClient) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock + take a batch of due rows. SKIP LOCKED so a sibling
    // sweeper (or a second operator replica) doesn't block here.
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, concrete_name, alias_name, schema_uid
         FROM platform.pending_typesense_reaps
         WHERE reap_after <= now()
         ORDER BY reap_after ASC
         LIMIT $1
         FOR UPDATE SKIP LOCKED",
    )
    .bind(SWEEP_BATCH)
    .fetch_all(&mut *tx)
    .await?;

    if rows.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }

    let mut reaped: u64 = 0;
    let mut to_delete: Vec<i64> = Vec::with_capacity(rows.len());
    for (id, concrete, alias, schema_uid) in &rows {
        match typesense.delete_collection(concrete).await {
            Ok(()) => {
                tracing::info!(
                    id,
                    concrete = %concrete,
                    alias = %alias,
                    schema_uid = %schema_uid,
                    "reaped old Typesense collection"
                );
                to_delete.push(*id);
                reaped += 1;
            }
            Err(e) => {
                // Leave the row locked-but-not-deleted for THIS tx;
                // it commits the un-deleted rows of `to_delete` but
                // releases the row lock on this one. Next tick will
                // re-lock and retry. Typesense's 404 path is treated
                // as success inside `delete_collection`, so this
                // branch is genuine failure.
                tracing::warn!(
                    id,
                    concrete = %concrete,
                    alias = %alias,
                    error = %e,
                    "typesense delete_collection failed; will retry"
                );
            }
        }
    }

    if !to_delete.is_empty() {
        sqlx::query("DELETE FROM platform.pending_typesense_reaps WHERE id = ANY($1)")
            .bind(&to_delete)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(reaped)
}

/// Enqueue a reap. Called by the rebuild task on successful alias
/// flip. Unique constraint on `concrete_name` makes this idempotent:
/// if a concrete is already queued (e.g. the rebuild re-ran after a
/// crash), `ON CONFLICT DO NOTHING` leaves the original schedule
/// intact rather than pushing the reap further into the future.
pub async fn enqueue(
    pool: &PgPool,
    concrete: &str,
    alias: &str,
    schema_uid: &str,
    grace_secs: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO platform.pending_typesense_reaps
            (concrete_name, alias_name, schema_uid, reap_after)
         VALUES ($1, $2, $3, now() + ($4::bigint || ' seconds')::interval)
         ON CONFLICT (concrete_name) DO NOTHING",
    )
    .bind(concrete)
    .bind(alias)
    .bind(schema_uid)
    .bind(grace_secs)
    .execute(pool)
    .await?;
    Ok(())
}
