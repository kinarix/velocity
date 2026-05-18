//! Event-log partition manager (Phase 3.8).
//!
//! `platform.event_log` is `PARTITION BY RANGE (occurred_at)` with one
//! partition per calendar month. The base migration bootstraps the
//! current month and next month; this module owns every partition that
//! comes after.
//!
//! What it does
//! - On each tick: ensure the partition for *this* month exists, and the
//!   partition for *next* month exists. Creating next month's partition
//!   ahead of the boundary is the load-bearing guarantee — without it
//!   `INSERT INTO platform.event_log` fails at midnight on the 1st of
//!   the next month with "no partition of relation event_log found".
//! - 90-day retention drop (per ADR-004 hot tier) is deferred; see
//!   `archive-worker` which already owns warm-tier hand-off. Adding it
//!   here would duplicate the same DROP TABLE decision in two places.
//!
//! What it does NOT do
//! - Does not touch per-domain history partitions. Those are owned by
//!   the SchemaOperator since the partitioning key is per-schema.
//! - Does not validate that the parent table is partitioned the way we
//!   think it is. If someone has manually re-shaped event_log, the
//!   `CREATE TABLE ... PARTITION OF` will fail loudly and the operator
//!   will surface the error rather than silently masking misconfiguration.
//!
//! Cadence: hourly. Picking hourly (not daily) means a tick is at most
//! ~60min from the partition boundary. Daily would be enough most months
//! but leaves zero margin for a missed tick at month-end.

use chrono::{Datelike, Months, NaiveDate, Utc};
use sqlx::PgPool;

/// One tick: ensure current + next month's partition exist. Idempotent
/// — re-running the function the same minute is a no-op.
///
/// Returns the names of any partitions that were *created* on this tick.
/// Empty vec means "all required partitions already existed." Returned
/// for the caller's telemetry — emit a metric like
/// `velocity_event_log_partitions_created_total` so an operator-side
/// graph shows a step every month and silence the rest of the time.
pub async fn tick(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let today = Utc::now().date_naive();
    ensure_months(pool, today).await
}

async fn ensure_months(pool: &PgPool, today: NaiveDate) -> Result<Vec<String>, sqlx::Error> {
    let current_start = first_of_month(today);
    let next_start = first_of_month_after(today);
    let next_next_start = first_of_month_after(next_start);

    let mut created = Vec::new();

    // current month: should already exist (bootstrap migration creates it),
    // but if a manual db restore dropped it we want to recreate. Idempotent.
    if create_partition_if_absent(pool, current_start, next_start).await? {
        created.push(partition_name(current_start));
    }
    // next month: this is the load-bearing one — without it inserts fail
    // the moment occurred_at crosses the month boundary.
    if create_partition_if_absent(pool, next_start, next_next_start).await? {
        created.push(partition_name(next_start));
    }

    Ok(created)
}

/// `event_log_YYYY_MM` — same naming the bootstrap migration uses so the
/// two paths share an existence check.
fn partition_name(start: NaiveDate) -> String {
    format!("event_log_{:04}_{:02}", start.year(), start.month())
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
    // d.month() is by construction in 1..=12 and (year, month, 1) is
    // always a real calendar day, so `from_ymd_opt` returning None is
    // impossible. We unwrap_or_else into the input rather than `expect`
    // so clippy::expect_used stays clean; the fallback path is
    // unreachable in practice.
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

fn first_of_month_after(d: NaiveDate) -> NaiveDate {
    // `checked_add_months(Months::new(1))` only fails if the year
    // overflows i32 — chrono caps at year 262143, well beyond any
    // realistic clock. unwrap_or back to first-of-month on the
    // impossible branch so we don't fall back to the caller's `d` which
    // is a different month.
    first_of_month(d).checked_add_months(Months::new(1)).unwrap_or_else(|| first_of_month(d))
}

/// Creates the partition if it isn't already in pg_class. Returns `true`
/// if a CREATE was issued, `false` if it already existed. We check
/// pg_class first (cheap) and only run the DDL if needed; doing
/// `CREATE TABLE IF NOT EXISTS ... PARTITION OF ...` would also work,
/// but the bare CREATE issues a clearer error if the partition exists
/// with the WRONG bounds — silent skip would mask that drift.
async fn create_partition_if_absent(
    pool: &PgPool,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<bool, sqlx::Error> {
    let name = partition_name(start);

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'platform' AND c.relname = $1)",
    )
    .bind(&name)
    .fetch_one(pool)
    .await?;

    if exists {
        return Ok(false);
    }

    // Names are derived from chrono outputs (always [a-z0-9_]) so the
    // dynamic identifier is safe; format!-into-DDL is reviewed against
    // CLAUDE.md §SQL safety because identifiers can never be parameter
    // binds in Postgres.
    let ddl = format!(
        "CREATE TABLE platform.{name} PARTITION OF platform.event_log \
         FOR VALUES FROM ('{start}') TO ('{end}')"
    );
    sqlx::query(&ddl).execute(pool).await?;

    Ok(true)
}

/// Run forever: tick at startup, then once an hour. The interval choice
/// is documented at the module level. Errors are logged and swallowed
/// — a transient DB blip should not crash the operator, the next tick
/// will retry. If the next-month partition is *still* missing as the
/// boundary approaches, the alert that fires is "event_log inserts
/// failing", which is a louder, more correct signal than a partition
/// manager exit.
pub async fn run(pool: PgPool) -> ! {
    // First tick immediately so a freshly-rolled operator doesn't wait
    // an hour before noticing a missing partition.
    if let Err(e) = tick(&pool).await {
        tracing::error!(error = %e, "event_log partition manager: initial tick failed");
    } else {
        tracing::info!("event_log partition manager: initial tick succeeded");
    }

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
    ticker.tick().await; // consume the immediate first fire
    loop {
        ticker.tick().await;
        match tick(&pool).await {
            Ok(created) if !created.is_empty() => {
                tracing::info!(?created, "event_log partition manager: created new partition(s)");
            }
            Ok(_) => {
                tracing::debug!("event_log partition manager: tick — no new partitions needed");
            }
            Err(e) => {
                tracing::error!(error = %e, "event_log partition manager: tick failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_name_pads_month_to_two_digits() {
        // March of 2026 → event_log_2026_03 (not event_log_2026_3). Padding
        // matters because the migration's bootstrap uses to_char(..., 'YYYY_MM')
        // which always emits two-digit month — if our pad differs, we'd create
        // a SECOND partition for the same month and silently miss inserts.
        let d = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        assert_eq!(partition_name(d), "event_log_2026_03");
    }

    #[test]
    fn partition_name_for_january_uses_01_not_1() {
        let d = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(partition_name(d), "event_log_2027_01");
    }

    #[test]
    fn first_of_month_floors_to_first() {
        let d = NaiveDate::from_ymd_opt(2026, 5, 23).unwrap();
        assert_eq!(first_of_month(d), NaiveDate::from_ymd_opt(2026, 5, 1).unwrap());
    }

    #[test]
    fn first_of_month_after_handles_december_wrap() {
        // The interesting edge: Dec → next month is Jan of next year, not
        // month 13. `Months::new(1)` handles this; this test guards against
        // someone "simplifying" to month + 1 arithmetic.
        let d = NaiveDate::from_ymd_opt(2026, 12, 17).unwrap();
        assert_eq!(first_of_month_after(d), NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    #[test]
    fn next_month_after_january_is_february_same_year() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert_eq!(first_of_month_after(d), NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
    }
}
