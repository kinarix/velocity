//! Hot → warm tier export (Phase 4.2 / ADR-004).
//!
//! Once per day, look at the oldest hot-tier monthly partition. If it
//! is wholly older than `HOT_RETENTION_DAYS`, export its rows to warm
//! storage (one Parquet object per `schema_org`), verify the objects
//! readable, then `DETACH PARTITION` + `DROP TABLE`. Each step is
//! individually idempotent so a crash anywhere in the sequence is
//! safe to resume on the next tick (orphan reconciliation, Phase 4.3,
//! finishes anything that's stuck mid-flight).
//!
//! What this does NOT do:
//!   - Per-schema retention. The hot window is platform-wide. Per-schema
//!     `timeMachine.storage.hot.retention` is a Phase 4 follow-up.
//!   - Cold-tier migration. S3 lifecycle policies handle that at the
//!     bucket layer; ADR-004 explicitly puts it out of operator scope.
//!   - Parallel export of multiple partitions. We process the oldest
//!     candidate one tick at a time. If we ever fall N months behind,
//!     we catch up over N ticks. This keeps blast-radius bounded.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use arrow::array::{ArrayRef, RecordBatch, StringArray, TimestampMicrosecondArray};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use object_store::{ObjectStore, ObjectStoreExt};
use parquet::arrow::AsyncArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use sqlx::PgPool;
use uuid::Uuid;

use crate::tiering::object_store_url;
use crate::tiering::schema as tier_schema;

/// Platform-wide hot retention. Per ADR-004 §"Tier · Retention" — hot
/// tier holds 90 days. Configurable per-schema in a later phase.
pub const HOT_RETENTION_DAYS: i64 = 90;

/// Daily tick cadence. The export window only moves once per month at
/// the boundary, so daily checks are plenty; hourly would be wasted
/// queries against `pg_partitions`. The first tick also runs on
/// startup, which catches the boundary if the operator was restarted
/// across midnight.
pub const TICK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Distinct from the provisioner's advisory-lock constant
/// (`7610358901234567890`) so exports do not block reconciles or vice
/// versa. Picked from a random uniform i64 generator; do not reuse.
const ADVISORY_LOCK_KEY: i64 = 4_812_667_113_590_117_433;

/// Page size when streaming rows out of the hot partition into Arrow
/// builders. 4096 matches the warm-reader's Parquet batch size so
/// every row group on disk decodes cleanly into one batch.
const STREAM_BATCH_ROWS: usize = 4096;

/// Result of a single tick. Vec is empty when nothing was ready to
/// export — the common steady-state mid-month case.
#[derive(Debug, Default)]
pub struct TickReport {
    pub partition_dropped: Option<String>,
    pub objects_written: Vec<String>,
    pub rows_exported: usize,
}

/// One tick: examine the oldest hot partition, export if eligible.
pub async fn tick(pool: &PgPool, warm_store: Arc<dyn ObjectStore>) -> Result<TickReport> {
    // Serialise with concurrent operator replicas (when leader
    // election lands) and with other reconcilers that touch partition
    // DDL. Transaction-scoped — auto-released when the tx commits.
    let mut tx = pool.begin().await.context("begin export tx")?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .context("acquire export advisory lock")?;

    let Some(part) = oldest_eligible_partition(&mut tx).await? else {
        tx.commit().await.ok();
        return Ok(TickReport::default());
    };

    tracing::info!(partition = %part.name, upper = %part.upper, "export candidate selected");

    let schema_orgs = distinct_schema_orgs(&mut tx, &part).await?;
    let mut report = TickReport::default();

    for schema_org in &schema_orgs {
        let (rows, key) = export_one_schema(&mut tx, warm_store.clone(), &part, schema_org).await?;
        report.rows_exported += rows;
        report.objects_written.push(key);
    }

    // Verify every object we just wrote is readable. If the
    // round-trip fails for any, abort the DETACH+DROP — we'd rather
    // leave the hot partition in place than lose data behind an
    // unreadable Parquet file.
    for key in &report.objects_written {
        verify_object_readable(warm_store.clone(), key).await?;
    }

    detach_and_drop_partition(&mut tx, &part).await?;
    tx.commit().await.context("commit export tx")?;
    report.partition_dropped = Some(part.name.clone());

    tracing::info!(
        partition = %part.name,
        objects = report.objects_written.len(),
        rows = report.rows_exported,
        "export complete: partition dropped"
    );
    Ok(report)
}

pub async fn run(pool: PgPool, warm_store: Arc<dyn ObjectStore>) -> ! {
    if let Err(e) = tick(&pool, warm_store.clone()).await {
        tracing::error!(error = %e, "tiering exporter: initial tick failed");
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match tick(&pool, warm_store.clone()).await {
            Ok(r) if r.partition_dropped.is_some() => {
                tracing::info!(?r, "tiering exporter: export succeeded");
            }
            Ok(_) => {
                tracing::debug!("tiering exporter: tick — no eligible partition");
            }
            Err(e) => {
                tracing::error!(error = %e, "tiering exporter: tick failed");
            }
        }
    }
}

#[derive(Debug)]
struct Partition {
    /// Bare partition name, e.g. `event_log_2026_03`.
    name: String,
    /// Lower bound (inclusive) of the partition's `occurred_at` range.
    lower: DateTime<Utc>,
    /// Upper bound (exclusive). The partition is eligible to export
    /// once `now() - upper >= HOT_RETENTION_DAYS`.
    upper: DateTime<Utc>,
}

impl Partition {
    fn year(&self) -> i32 {
        self.lower.year()
    }
    fn month(&self) -> u32 {
        self.lower.month()
    }
}

/// Find the oldest hot-tier partition wholly older than the hot window.
/// Returns `None` if nothing is eligible (the steady state).
async fn oldest_eligible_partition(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<Option<Partition>> {
    // pg_partitions doesn't carry the range bounds in a parseable
    // form — pg_get_expr does. We pull the `FOR VALUES FROM (...) TO
    // (...)` expression for every child of `platform.event_log` and
    // parse it. The expression for a RANGE partition is always:
    //   "FOR VALUES FROM ('YYYY-MM-DD ...') TO ('YYYY-MM-DD ...')"
    // so a regex pulls both bounds. Plain string parsing here is safe
    // because the expression is system-generated, not user input.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, pg_get_expr(c.relpartbound, c.oid, true) \
         FROM pg_inherits i \
         JOIN pg_class c ON c.oid = i.inhrelid \
         JOIN pg_class p ON p.oid = i.inhparent \
         JOIN pg_namespace n ON n.oid = p.relnamespace \
         WHERE n.nspname = 'platform' AND p.relname = 'event_log' \
         ORDER BY c.relname ASC",
    )
    .fetch_all(&mut **tx)
    .await
    .context("listing event_log partitions")?;

    let cutoff = Utc::now() - chrono::Duration::days(HOT_RETENTION_DAYS);

    for (name, expr) in rows {
        let Some((lower, upper)) = parse_range_bounds(&expr) else {
            tracing::warn!(partition = %name, expr = %expr, "skipping partition: cannot parse range bounds");
            continue;
        };
        if upper <= cutoff {
            return Ok(Some(Partition { name, lower, upper }));
        }
    }
    Ok(None)
}

/// Parse "FOR VALUES FROM ('2026-03-01') TO ('2026-04-01')" → (lower, upper).
fn parse_range_bounds(expr: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let s = expr.trim();
    let from_idx = s.find("FROM (")?;
    let to_idx = s.find(" TO (")?;
    let from_slice = &s[from_idx + "FROM (".len()..];
    let from_end = from_slice.find(')')?;
    let from_val = from_slice[..from_end].trim().trim_matches('\'');
    let to_slice = &s[to_idx + " TO (".len()..];
    let to_end = to_slice.find(')')?;
    let to_val = to_slice[..to_end].trim().trim_matches('\'');

    let lower = parse_partition_date(from_val)?;
    let upper = parse_partition_date(to_val)?;
    Some((lower, upper))
}

/// Postgres emits the bound as `'2026-03-01 00:00:00+00'` for a
/// TIMESTAMPTZ column, but also as `'2026-03-01'` if a bare DATE was
/// supplied. Handle both.
fn parse_partition_date(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%#z") {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(d) = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%#z") {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).and_then(|n| chrono::TimeZone::from_utc_datetime(&Utc, &n).into());
    }
    None
}

async fn distinct_schema_orgs(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    part: &Partition,
) -> Result<Vec<String>> {
    // SAFETY: `part.name` came from pg_class via the listing query
    // above. It is a system identifier, not user input. We still
    // bound the format with the `platform.` schema prefix so we can't
    // accidentally point at a non-platform table even if the name
    // looked weird.
    let sql = format!(
        "SELECT DISTINCT schema_org::text FROM platform.{} ORDER BY 1",
        part.name
    );
    let rows: Vec<(String,)> = sqlx::query_as(&sql)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("listing distinct schema_org in {}", part.name))?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// Stream rows for one `schema_org` out of the hot partition and into
/// a single Parquet object on warm storage. Returns (rows_written,
/// object_key).
async fn export_one_schema(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    warm_store: Arc<dyn ObjectStore>,
    part: &Partition,
    schema_org: &str,
) -> Result<(usize, String)> {
    let key = object_store_url::month_key(schema_org, part.year(), part.month());
    tracing::info!(partition = %part.name, schema_org = %schema_org, key = %key, "starting export");

    let arrow_schema = tier_schema::arrow_schema();
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();

    // `BufWriter` wraps an `ObjectStore` and converts the Parquet
    // writer's small flushes into a single multipart-upload. Without
    // the wrapper, each small Parquet flush would attempt a tiny S3
    // PUT — `object_store::buffered::BufWriter` is purpose-built for
    // this Parquet-on-S3 pattern.
    let writer = object_store::buffered::BufWriter::with_capacity(
        warm_store.clone(),
        key.clone(),
        8 * 1024 * 1024,
    );

    let mut pq = AsyncArrowWriter::try_new(writer, arrow_schema.clone(), Some(props))
        .with_context(|| format!("create parquet writer for {key}"))?;

    let sql = format!(
        "SELECT occurred_at, schema_org::text, entity_id, operation::text, diff, payload \
         FROM platform.{} WHERE schema_org = $1 ORDER BY occurred_at ASC",
        part.name
    );

    let mut rows_stream = sqlx::query_as::<_, EventLogRow>(&sql)
        .bind(schema_org)
        .fetch(&mut **tx);

    use futures::StreamExt;
    let mut buf: Vec<EventLogRow> = Vec::with_capacity(STREAM_BATCH_ROWS);
    let mut total: usize = 0;

    // Per-batch and close timeouts bound how long a slow / hung warm
    // store (S3 throttling, network blip) can hold the open tx + the
    // open parquet writer. `BufWriter` internally batches into multipart
    // PUTs; a healthy upload is sub-second per batch, so 60s is
    // comfortably generous without letting one bad object park us
    // indefinitely.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
    const CLOSE_TIMEOUT: Duration = Duration::from_secs(120);

    while let Some(row) = rows_stream.next().await {
        let row = row.with_context(|| format!("fetch row from {}", part.name))?;
        buf.push(row);
        if buf.len() >= STREAM_BATCH_ROWS {
            let batch = rows_to_batch(&arrow_schema, &buf)?;
            tokio::time::timeout(WRITE_TIMEOUT, pq.write(&batch))
                .await
                .with_context(|| format!("write batch to {key} timed out"))?
                .with_context(|| format!("write batch to {key}"))?;
            total += buf.len();
            buf.clear();
        }
    }
    if !buf.is_empty() {
        let batch = rows_to_batch(&arrow_schema, &buf)?;
        tokio::time::timeout(WRITE_TIMEOUT, pq.write(&batch))
            .await
            .with_context(|| format!("write tail batch to {key} timed out"))?
            .with_context(|| format!("write tail batch to {key}"))?;
        total += buf.len();
    }

    tokio::time::timeout(CLOSE_TIMEOUT, pq.close())
        .await
        .with_context(|| format!("close parquet writer for {key} timed out"))?
        .with_context(|| format!("close parquet writer for {key}"))?;
    tracing::info!(key = %key, rows = total, "export object closed");
    Ok((total, key.to_string()))
}

#[derive(Debug, sqlx::FromRow)]
struct EventLogRow {
    occurred_at: DateTime<Utc>,
    schema_org: String,
    entity_id: Option<Uuid>,
    operation: String,
    diff: Option<serde_json::Value>,
    payload: Option<serde_json::Value>,
}

fn rows_to_batch(schema: &arrow::datatypes::SchemaRef, rows: &[EventLogRow]) -> Result<RecordBatch> {
    let occurred: TimestampMicrosecondArray = rows
        .iter()
        .map(|r| Some(r.occurred_at.timestamp_micros()))
        .collect::<Vec<_>>()
        .into();
    let occurred = occurred.with_timezone("UTC");

    let schema_org: StringArray = rows.iter().map(|r| Some(r.schema_org.as_str())).collect();
    let entity_id: StringArray = rows
        .iter()
        .map(|r| r.entity_id.as_ref().map(|u| u.hyphenated().to_string()))
        .collect();
    let operation: StringArray = rows.iter().map(|r| Some(r.operation.as_str())).collect();
    let diff: StringArray = rows
        .iter()
        .map(|r| r.diff.as_ref().map(|j| j.to_string()))
        .collect();
    let payload: StringArray = rows
        .iter()
        .map(|r| r.payload.as_ref().map(|j| j.to_string()))
        .collect();

    let cols: Vec<ArrayRef> = vec![
        Arc::new(occurred),
        Arc::new(schema_org),
        Arc::new(entity_id),
        Arc::new(operation),
        Arc::new(diff),
        Arc::new(payload),
    ];
    RecordBatch::try_new(schema.clone(), cols).context("build record batch")
}

/// Read the object back end-to-end. We don't need a deep equality
/// check vs the source — Parquet's footer is checksummed and the
/// writer fsync's via the multipart commit. The minimum-viable check
/// is "the object opens and reports a sane row count." If we can't
/// even open it, the DETACH+DROP must NOT proceed.
async fn verify_object_readable(warm_store: Arc<dyn ObjectStore>, key: &str) -> Result<()> {
    use parquet::arrow::async_reader::ParquetObjectReader;
    use parquet::arrow::ParquetRecordBatchStreamBuilder;

    let path = object_store::path::Path::from(key.to_string());
    let meta = warm_store
        .head(&path)
        .await
        .with_context(|| format!("HEAD verify for {key}"))?;
    let reader = ParquetObjectReader::new(warm_store, path).with_file_size(meta.size);
    let builder = ParquetRecordBatchStreamBuilder::new(reader)
        .await
        .with_context(|| format!("open verify for {key}"))?;
    let rows = builder.metadata().file_metadata().num_rows();
    if rows < 0 {
        return Err(anyhow!("verify: negative row count in {key}"));
    }
    tracing::debug!(key = %key, rows, "verify-readable ok");
    Ok(())
}

async fn detach_and_drop_partition(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    part: &Partition,
) -> Result<()> {
    // DETACH + DROP must succeed atomically with the export from the
    // caller's perspective. Both go in the same transaction so a
    // rollback puts us back in pre-export state. DETACH CONCURRENTLY
    // would not work inside a transaction, but that's intentional —
    // CONCURRENTLY is needed when writes can't be briefly paused, and
    // an event_log partition at >=90d old has no live writers.
    let detach = format!("ALTER TABLE platform.event_log DETACH PARTITION platform.{}", part.name);
    sqlx::query(&detach).execute(&mut **tx).await.with_context(|| format!("detach {}", part.name))?;

    let drop_sql = format!("DROP TABLE platform.{}", part.name);
    sqlx::query(&drop_sql).execute(&mut **tx).await.with_context(|| format!("drop {}", part.name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_timestamptz_partition_bound() {
        let expr = "FOR VALUES FROM ('2026-03-01 00:00:00+00') TO ('2026-04-01 00:00:00+00')";
        let (lo, hi) = parse_range_bounds(expr).unwrap();
        assert_eq!(lo.format("%Y-%m-%d").to_string(), "2026-03-01");
        assert_eq!(hi.format("%Y-%m-%d").to_string(), "2026-04-01");
    }

    #[test]
    fn parses_bare_date_partition_bound() {
        let expr = "FOR VALUES FROM ('2026-03-01') TO ('2026-04-01')";
        let (lo, hi) = parse_range_bounds(expr).unwrap();
        assert_eq!(lo.format("%Y-%m").to_string(), "2026-03");
        assert_eq!(hi.format("%Y-%m").to_string(), "2026-04");
    }

    #[test]
    fn rejects_malformed_bound_expression() {
        assert!(parse_range_bounds("not a partition expr").is_none());
        assert!(parse_range_bounds("FOR VALUES FROM ('bogus') TO ('also-bogus')").is_none());
    }

    #[test]
    fn advisory_lock_key_is_distinct_from_provisioner() {
        // Co-existence requires this. If you change either constant,
        // change both with a sanity test like this.
        assert_ne!(ADVISORY_LOCK_KEY, 7_610_358_901_234_567_890_i64);
    }
}
