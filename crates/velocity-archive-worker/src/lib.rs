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
use velocity_types::crds::SchemaDefinitionSpec;

pub mod worker;

/// Canonical system-column order — must match the operator's
/// `ddl_builder::SYSTEM_COLUMNS`. The archive mirror table is created with
/// exactly these columns (in this order) followed by the user fields, so
/// the INSERT column list inside [`archive_batch`] has to enumerate them
/// the same way or Postgres rejects the move with a type-mismatch.
///
/// If the operator ever changes its system-column set, the integration
/// tests that move real rows through hot → archive will fail loudly on
/// the first `INSERT INTO archive (...) SELECT (...)` — wrong column
/// count or wrong type position. That's the lock-step gate.
pub const SYSTEM_COLUMN_NAMES: &[&str] = &[
    "id",
    "created_at",
    "updated_at",
    "deleted_at",
    "version",
    "created_by",
    "updated_by",
    "archived_at",
    "archive_ref",
];

/// Build the ordered column-name list for a `SchemaDefinitionSpec`: the
/// system columns first, then user fields in declaration order. The
/// `__fts` generated column is intentionally NOT included — it's
/// regenerated on insert into the archive table from the source columns.
pub fn ordered_column_names(spec: &SchemaDefinitionSpec) -> Vec<String> {
    let mut out: Vec<String> = SYSTEM_COLUMN_NAMES.iter().map(|s| (*s).to_string()).collect();
    for f in &spec.fields {
        out.push(f.name.clone());
    }
    out
}

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
    pub min_age: Duration,
    /// Upper bound on rows moved per call. Clamped 1..=10_000.
    pub batch_size: usize,
}

/// Field-comparison operator for the `field` trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl FieldOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lt" => Some(FieldOp::Lt),
            "le" => Some(FieldOp::Le),
            "gt" => Some(FieldOp::Gt),
            "ge" => Some(FieldOp::Ge),
            "eq" => Some(FieldOp::Eq),
            "ne" => Some(FieldOp::Ne),
            _ => None,
        }
    }
    pub fn as_sql(self) -> &'static str {
        match self {
            FieldOp::Lt => "<",
            FieldOp::Le => "<=",
            FieldOp::Gt => ">",
            FieldOp::Ge => ">=",
            FieldOp::Eq => "=",
            FieldOp::Ne => "<>",
        }
    }
}

/// Which trigger an `archive_batch` call is honouring. Drives both the
/// SQL the renderer emits and the bound parameter passed to `$1`.
#[derive(Debug, Clone)]
pub enum ArchivePredicate<'a> {
    /// Rows whose `created_at < now() - min_age` are eligible.
    Age { min_age: Duration },
    /// Rows where `{field} {op} $value` are eligible.
    Field {
        field: &'a str,
        op: FieldOp,
        value: &'a str,
    },
    /// No predicate beyond the existing soft filters — caller has
    /// already checked `pg_total_relation_size` against the threshold
    /// and decided to drain oldest rows.
    Oldest,
}

/// Pick → insert → mark, in one transaction. Convenience wrapper around
/// [`archive_batch_with_predicate`] for the common age trigger.
pub async fn archive_batch(
    pool: &PgPool,
    args: &ArchiveBatchArgs<'_>,
) -> Result<ArchiveBatchResult, ArchiveError> {
    archive_batch_with_predicate(
        pool,
        args,
        &ArchivePredicate::Age {
            min_age: args.min_age,
        },
    )
    .await
}

/// Pick → insert → mark, in one transaction. Returns the number of rows
/// moved and a `more_pending` flag.
pub async fn archive_batch_with_predicate(
    pool: &PgPool,
    args: &ArchiveBatchArgs<'_>,
    predicate: &ArchivePredicate<'_>,
) -> Result<ArchiveBatchResult, ArchiveError> {
    let sql = build_archive_batch_sql_for(args, predicate)?;

    let mut tx = pool.begin().await?;
    let limit = args.batch_size as i64;

    let count: i64 = match predicate {
        ArchivePredicate::Age { min_age } => {
            let age_secs = min_age.as_secs() as i64;
            sqlx::query_scalar(&sql)
                .bind(age_secs)
                .bind(limit)
                .fetch_one(&mut *tx)
                .await?
        }
        ArchivePredicate::Field { value, .. } => {
            sqlx::query_scalar(&sql)
                .bind(*value)
                .bind(limit)
                .fetch_one(&mut *tx)
                .await?
        }
        ArchivePredicate::Oldest => {
            sqlx::query_scalar(&sql).bind(limit).fetch_one(&mut *tx).await?
        }
    };

    tx.commit().await?;

    let n = count as usize;
    Ok(ArchiveBatchResult {
        rows_archived: n,
        more_pending: n >= args.batch_size,
    })
}

/// Returns `pg_total_relation_size(schema.table)` in bytes. Used by the
/// `tableSize` trigger to decide whether to drain oldest rows.
pub async fn table_size_bytes(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<i64, ArchiveError> {
    validate_ident(schema)?;
    validate_ident(table)?;
    let qualified = format!("{schema}.{table}");
    let size: i64 = sqlx::query_scalar("SELECT pg_total_relation_size($1::regclass)::bigint")
        .bind(&qualified)
        .fetch_one(pool)
        .await?;
    Ok(size)
}

/// Inputs to [`purge_batch`].
#[derive(Debug, Clone)]
pub struct PurgeBatchArgs<'a> {
    pub hot_schema: &'a str,
    pub hot_table: &'a str,
    /// Rows with `archived_at < now() - min_age_since_archive` are eligible
    /// for hard-delete from the hot table. The archive copy stays.
    pub min_age_since_archive: Duration,
    /// Upper bound on rows deleted per call. Clamped 1..=10_000.
    pub batch_size: usize,
}

/// Hard-delete archived rows from the hot table.
///
/// `archive_batch` (slice 4) sets `archived_at` on the hot row; readers
/// already filter `archived_at IS NULL` so the row is invisible. This
/// primitive reclaims storage by deleting those marked rows after the
/// policy's `purgeAfter` window has elapsed. The archive copy is the
/// long-term home.
///
/// One transaction. Returns the number of rows deleted.
pub async fn purge_batch(
    pool: &PgPool,
    args: &PurgeBatchArgs<'_>,
) -> Result<ArchiveBatchResult, ArchiveError> {
    let sql = build_purge_batch_sql(args)?;
    let mut tx = pool.begin().await?;

    let age_secs = args.min_age_since_archive.as_secs() as i64;
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

/// Render the CTE that picks + deletes archived rows from the hot table.
/// Pure — exposed for unit testing.
pub fn build_purge_batch_sql(args: &PurgeBatchArgs<'_>) -> Result<String, ArchiveError> {
    validate_ident(args.hot_schema)?;
    validate_ident(args.hot_table)?;
    if args.min_age_since_archive.as_secs() == 0 {
        return Err(ArchiveError::InvalidAge);
    }
    if !(1..=10_000).contains(&args.batch_size) {
        return Err(ArchiveError::InvalidBatchSize(args.batch_size));
    }
    let hot = format!("{}.{}", args.hot_schema, args.hot_table);
    Ok(format!(
        "WITH picked AS (
    SELECT id FROM {hot}
    WHERE archived_at IS NOT NULL
      AND archived_at < now() - make_interval(secs => $1)
    ORDER BY id
    LIMIT $2
),
deleted AS (
    DELETE FROM {hot}
    WHERE id IN (SELECT id FROM picked)
    RETURNING id
)
SELECT count(*)::bigint FROM deleted;"
    ))
}

/// Back-compat shim: render the age-trigger SQL.
pub fn build_archive_batch_sql(args: &ArchiveBatchArgs<'_>) -> Result<String, ArchiveError> {
    build_archive_batch_sql_for(
        args,
        &ArchivePredicate::Age {
            min_age: args.min_age,
        },
    )
}

/// Render the single-statement CTE for any supported predicate.
/// Pure — exposed for unit testing.
pub fn build_archive_batch_sql_for(
    args: &ArchiveBatchArgs<'_>,
    predicate: &ArchivePredicate<'_>,
) -> Result<String, ArchiveError> {
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
    if !(1..=10_000).contains(&args.batch_size) {
        return Err(ArchiveError::InvalidBatchSize(args.batch_size));
    }

    let (where_clause, limit_param) = match predicate {
        ArchivePredicate::Age { min_age } => {
            if min_age.as_secs() == 0 {
                return Err(ArchiveError::InvalidAge);
            }
            (
                "AND created_at < now() - make_interval(secs => $1)".to_string(),
                "$2",
            )
        }
        ArchivePredicate::Field { field, op, .. } => {
            validate_ident(field).map_err(|_| {
                ArchiveError::InvalidColumns(format!("field {field:?} is not a valid identifier"))
            })?;
            (format!("AND {field} {} $1", op.as_sql()), "$2")
        }
        ArchivePredicate::Oldest => (String::new(), "$1"),
    };

    let cols = args.columns.join(", ");
    let hot = format!("{}.{}", args.hot_schema, args.hot_table);
    let arc = format!("{}.{}", args.archive_schema, args.archive_table);
    let order_by = match predicate {
        ArchivePredicate::Oldest => "ORDER BY created_at",
        _ => "ORDER BY id",
    };

    Ok(format!(
        "WITH picked AS (
    SELECT id FROM {hot}
    WHERE archived_at IS NULL
      AND deleted_at IS NULL
      {where_clause}
    {order_by}
    LIMIT {limit_param}
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
    fn field_predicate_renders_correctly() {
        let (cols, _) = args();
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "po_v1",
            archive_schema: "sa",
            archive_table: "po_v1",
            columns: &cols,
            min_age: Duration::from_secs(1),
            batch_size: 200,
        };
        let p = ArchivePredicate::Field {
            field: "status",
            op: FieldOp::Eq,
            value: "closed",
        };
        let sql = build_archive_batch_sql_for(&a, &p).unwrap();
        assert!(sql.contains("AND status = $1"));
        assert!(sql.contains("LIMIT $2"));
        assert!(!sql.contains("make_interval"));
    }

    #[test]
    fn field_predicate_validates_field_ident() {
        let (cols, _) = args();
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: Duration::from_secs(1),
            batch_size: 1,
        };
        let p = ArchivePredicate::Field {
            field: "bad name",
            op: FieldOp::Eq,
            value: "x",
        };
        let err = build_archive_batch_sql_for(&a, &p).unwrap_err();
        assert!(matches!(err, ArchiveError::InvalidColumns(_)));
    }

    #[test]
    fn oldest_predicate_uses_one_param_and_orders_by_created_at() {
        let (cols, _) = args();
        let a = ArchiveBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            archive_schema: "sa",
            archive_table: "t",
            columns: &cols,
            min_age: Duration::from_secs(1),
            batch_size: 100,
        };
        let sql = build_archive_batch_sql_for(&a, &ArchivePredicate::Oldest).unwrap();
        assert!(sql.contains("LIMIT $1"));
        assert!(sql.contains("ORDER BY created_at"));
        // no extra AND beyond the soft-filter pair
        assert!(!sql.contains("make_interval"));
        assert!(!sql.contains(" = $1"));
    }

    #[test]
    fn field_op_parsing() {
        for (s, expected) in [
            ("lt", FieldOp::Lt),
            ("le", FieldOp::Le),
            ("gt", FieldOp::Gt),
            ("ge", FieldOp::Ge),
            ("eq", FieldOp::Eq),
            ("ne", FieldOp::Ne),
        ] {
            assert_eq!(FieldOp::parse(s), Some(expected));
        }
        assert_eq!(FieldOp::parse("regex"), None);
    }

    #[test]
    fn field_op_to_sql() {
        assert_eq!(FieldOp::Lt.as_sql(), "<");
        assert_eq!(FieldOp::Ge.as_sql(), ">=");
        assert_eq!(FieldOp::Ne.as_sql(), "<>");
    }

    #[test]
    fn purge_sql_basic_shape() {
        let a = PurgeBatchArgs {
            hot_schema: "acme_sc_proc",
            hot_table: "purchase_order_v1",
            min_age_since_archive: Duration::from_secs(90 * 86_400),
            batch_size: 500,
        };
        let sql = build_purge_batch_sql(&a).unwrap();
        assert!(sql.contains("DELETE FROM acme_sc_proc.purchase_order_v1"));
        assert!(sql.contains("archived_at IS NOT NULL"));
        assert!(sql.contains("archived_at < now() - make_interval(secs => $1)"));
        assert!(sql.contains("LIMIT $2"));
        assert!(sql.contains("SELECT count(*)::bigint FROM deleted"));
    }

    #[test]
    fn purge_sql_validates_inputs() {
        let mut a = PurgeBatchArgs {
            hot_schema: "s",
            hot_table: "t",
            min_age_since_archive: Duration::from_secs(0),
            batch_size: 100,
        };
        assert!(matches!(
            build_purge_batch_sql(&a).unwrap_err(),
            ArchiveError::InvalidAge
        ));
        a.min_age_since_archive = Duration::from_secs(60);
        a.batch_size = 0;
        assert!(matches!(
            build_purge_batch_sql(&a).unwrap_err(),
            ArchiveError::InvalidBatchSize(_)
        ));
        a.batch_size = 100;
        a.hot_schema = "bad name";
        assert!(matches!(
            build_purge_batch_sql(&a).unwrap_err(),
            ArchiveError::InvalidIdent(_)
        ));
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
