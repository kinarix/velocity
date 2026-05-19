#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Velocity archive worker — library surface.
//!
//! Phase 8 slice 4 lands the **archive primitive**: a single-transaction
//! `archive_batch` that moves a bounded set of rows from a hot table to
//! its archive-tier mirror, soft-marking the hot row's `archived_at` so
//! readers stop seeing it. The driver loop (cron-aware scheduling,
//! per-policy quotas, status updates) lands in slice 5.
//!
//! Design notes:
//!
//! - **One transaction**. Pick → insert → mark all happen in a single
//!   CTE so a crash mid-batch either commits the whole batch or none of
//!   it. The hot row's `archived_at` is the source of truth for "this
//!   row has been moved"; the archive copy is the durable home.
//!
//! - **Idempotent re-runs**. `INSERT ... ON CONFLICT (id) DO NOTHING`
//!   protects against the unlikely case where a previous run crashed
//!   after the archive INSERT committed but before the hot UPDATE did
//!   (impossible inside one tx, but defensive against future split-batch
//!   variants).
//!
//! - **Bounded blast radius**. `LIMIT $batch_size` caps how much work
//!   one call does; the caller decides whether to loop. A 10-minute
//!   policy run that hits LIMIT each iteration just keeps draining; one
//!   that returns < limit means it's caught up.
//!
//! - **Single-writer assumption (slice 4)**. We don't take
//!   `FOR UPDATE SKIP LOCKED` — there's one archive worker per pod and
//!   a planned single replica. When slice 6+ introduces sharded
//!   workers this primitive grows the skip-locked variant.

use std::time::Duration;

use sqlx::PgPool;
use thiserror::Error;

/// Result of a single [`archive_batch`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveBatchResult {
    /// Number of rows the call moved from hot → archive.
    pub rows_archived: usize,
    /// `true` when the call hit `batch_size` (more rows may be eligible);
    /// `false` when it processed fewer (caught up for this trigger).
    pub more_pending: bool,
}

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("invalid identifier {0:?}")]
    InvalidIdent(String),

    #[error("invalid column list: {0}")]
    InvalidColumns(String),

    #[error("invalid batch_size: {0} (must be 1..=10000)")]
    InvalidBatchSize(usize),

    #[error("invalid age: must be > 0")]
    InvalidAge,
}

/// Inputs to [`archive_batch`].
#[derive(Debug, Clone)]
pub struct ArchiveBatchArgs<'a> {
    pub hot_schema: &'a str,
    pub hot_table: &'a str,
    pub archive_schema: &'a str,
    pub archive_table: &'a str,
    /// Every column to copy from hot → archive, including `id` and the
    /// system columns. Order is preserved in both the SELECT and the
    /// INSERT column list so position-coupled types line up.
    pub columns: &'a [String],
    /// Trigger: rows whose `created_at` is older than this are eligible.
    /// Slice 5 will add other triggers (field, tableSize, cel).
    pub min_age: Duration,
    /// Upper bound on rows moved per call. Clamped 1..=10_000.
    pub batch_size: usize,
}

/// Pick → insert → mark, in one transaction.
///
/// Returns the number of rows moved and a `more_pending` flag the caller
/// can use to decide whether to loop. On any error the transaction is
/// rolled back implicitly when the `Transaction` drops.
pub async fn archive_batch(
    pool: &PgPool,
    args: &ArchiveBatchArgs<'_>,
) -> Result<ArchiveBatchResult, ArchiveError> {
    let sql = build_archive_batch_sql(args)?;

    let mut tx = pool.begin().await?;

    let age_secs = args.min_age.as_secs() as i64;
    let limit = args.batch_size as i64;

    let count: i64 = sqlx::query_scalar(&sql)
        .bind(age_secs)
        .bind(limit)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    let n = count as usize;
    Ok(ArchiveBatchResult {
        rows_archived: n,
        more_pending: n >= args.batch_size,
    })
}

/// Render the single-statement CTE that performs the archive move.
/// Pure — exposed for unit testing and for callers that want to embed
/// the SQL into a larger transaction (e.g. running multiple per-table
/// batches inside one policy tick).
pub fn build_archive_batch_sql(args: &ArchiveBatchArgs<'_>) -> Result<String, ArchiveError> {
    validate_ident(args.hot_schema)?;
    validate_ident(args.hot_table)?;
    validate_ident(args.archive_schema)?;
    validate_ident(args.archive_table)?;
    if args.columns.is_empty() {
        return Err(ArchiveError::InvalidColumns("columns list is empty".into()));
    }
    if !args.columns.iter().any(|c| c == "id") {
        return Err(ArchiveError::InvalidColumns(
            "columns must include `id` (used to join picked → inserted → marked)".into(),
        ));
    }
    for c in args.columns {
        validate_ident(c).map_err(|_| {
            ArchiveError::InvalidColumns(format!("column {c:?} is not a valid identifier"))
        })?;
    }
    if args.min_age.as_secs() == 0 {
        return Err(ArchiveError::InvalidAge);
    }
    if !(1..=10_000).contains(&args.batch_size) {
        return Err(ArchiveError::InvalidBatchSize(args.batch_size));
    }

    let cols = args.columns.join(", ");
    let hot = format!("{}.{}", args.hot_schema, args.hot_table);
    let arc = format!("{}.{}", args.archive_schema, args.archive_table);

    Ok(format!(
        "WITH picked AS (
    SELECT id FROM {hot}
    WHERE archived_at IS NULL
      AND deleted_at IS NULL
      AND created_at < now() - make_interval(secs => $1)
    ORDER BY id
    LIMIT $2
),
inserted AS (
    INSERT INTO {arc} ({cols})
    SELECT {cols} FROM {hot}
    WHERE id IN (SELECT id FROM picked)
    ON CONFLICT (id) DO NOTHING
    RETURNING id
),
marked AS (
    UPDATE {hot}
    SET archived_at = now()
    WHERE id IN (SELECT id FROM inserted)
    RETURNING id
)
SELECT count(*)::bigint FROM marked;"
    ))
}

// ─── Identifier validation ─────────────────────────────────────────────────

fn validate_ident(s: &str) -> Result<(), ArchiveError> {
    if s.is_empty()
        || s.len() > 63
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        || s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true)
    {
        return Err(ArchiveError::InvalidIdent(s.into()));
    }
    Ok(())
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> (Vec<String>, Duration) {
        (
            vec![
                "id".into(),
                "created_at".into(),
                "updated_at".into(),
                "po_number".into(),
                "supplier_code".into(),
            ],
            Duration::from_secs(30 * 86_400),
        )
    }

    #[test]
    fn happy_path_renders_cte() {
        let (cols, age) = args();
        let a = ArchiveBatchArgs {
            hot_schema: "acme_sc_proc",
            hot_table: "purchase_order_v1",
            archive_schema: "acme_sc_proc_archive",
            archive_table: "purchase_order_v1",
            columns: &cols,
            min_age: age,
            batch_size: 500,
        };
        let sql = build_archive_batch_sql(&a).unwrap();
        assert!(sql.contains("WITH picked AS"));
        assert!(sql.contains("FROM acme_sc_proc.purchase_order_v1"));
        assert!(sql.contains("INSERT INTO acme_sc_proc_archive.purchase_order_v1"));
        assert!(sql.contains("id, created_at, updated_at, po_number, supplier_code"));
        assert!(sql.contains("ON CONFLICT (id) DO NOTHING"));
        assert!(sql.contains("UPDATE acme_sc_proc.purchase_order_v1"));
        assert!(sql.contains("archived_at = now()"));
        assert!(sql.contains("SELECT count(*)::bigint FROM marked"));
        assert!(sql.contains("LIMIT $2"));
        assert!(sql.contains("make_interval(secs => $1)"));
        assert!(sql.contains("archived_at IS NULL"));
        assert!(sql.contains("deleted_at IS NULL"));
    }

    #[test]
    fn rejects_missing_id_column() {
        let cols = vec!["created_at".into(), "po_number".into()];
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: Duration::from_secs(1),
            batch_size: 1,
        };
        let err = build_archive_batch_sql(&a).unwrap_err();
        assert!(matches!(err, ArchiveError::InvalidColumns(_)));
    }

    #[test]
    fn rejects_empty_columns() {
        let cols: Vec<String> = vec![];
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: Duration::from_secs(1),
            batch_size: 1,
        };
        let err = build_archive_batch_sql(&a).unwrap_err();
        assert!(matches!(err, ArchiveError::InvalidColumns(_)));
    }

    #[test]
    fn rejects_invalid_identifier_in_schema_or_table() {
        let cols = vec!["id".into()];
        for bad in ["1bad", "drop;", "name space", "", &"x".repeat(64)] {
            let a = ArchiveBatchArgs {
                hot_schema: bad,
                hot_table: "t",
                archive_schema: "sa",
                archive_table: "t",
                columns: &cols,
                min_age: Duration::from_secs(1),
                batch_size: 1,
            };
            let err = build_archive_batch_sql(&a).unwrap_err();
            assert!(matches!(err, ArchiveError::InvalidIdent(_)), "bad={bad:?}");
        }
    }

    #[test]
    fn rejects_invalid_column_identifier() {
        let cols = vec!["id".into(), "bad name".into()];
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: Duration::from_secs(1),
            batch_size: 1,
        };
        let err = build_archive_batch_sql(&a).unwrap_err();
        assert!(matches!(err, ArchiveError::InvalidColumns(_)));
    }

    #[test]
    fn rejects_zero_age() {
        let (cols, _) = args();
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: Duration::from_secs(0),
            batch_size: 1,
        };
        assert!(matches!(
            build_archive_batch_sql(&a).unwrap_err(),
            ArchiveError::InvalidAge
        ));
    }

    #[test]
    fn rejects_out_of_range_batch_size() {
        let (cols, age) = args();
        for size in [0, 10_001, usize::MAX] {
            let a = ArchiveBatchArgs {
                hot_schema: "s",
                hot_table: "t",
                archive_schema: "sa",
                archive_table: "t",
                columns: &cols,
                min_age: age,
                batch_size: size,
            };
            assert!(matches!(
                build_archive_batch_sql(&a).unwrap_err(),
                ArchiveError::InvalidBatchSize(_)
            ));
        }
    }

    #[test]
    fn allows_max_batch_size() {
        let (cols, age) = args();
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: age,
            batch_size: 10_000,
        };
        assert!(build_archive_batch_sql(&a).is_ok());
    }

    #[test]
    fn batch_result_more_pending_flag() {
        let r = ArchiveBatchResult {
            rows_archived: 500,
            more_pending: true,
        };
        assert!(r.more_pending);
        let r = ArchiveBatchResult {
            rows_archived: 0,
            more_pending: false,
        };
        assert_eq!(r.rows_archived, 0);
    }
}
