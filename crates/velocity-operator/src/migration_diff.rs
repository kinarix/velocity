//! Migration diff — compare the target [`DdlPlan`] against the existing
//! Postgres table state and decide what to apply.
//!
//! The classification is the heart of ADR-aligned safety: **safe ops apply
//! automatically; breaking ops are blocked unless the user explicitly
//! requests them via the `velocity.sh/breaking-change: approved` annotation.**
//! (See `CLAUDE.md › Blocking breaking changes`.)
//!
//! What this module does NOT do (deferred to later phases):
//! - constraint diffs (CHECK on enum changes)
//! - index drops (always keep — harmless leftovers)
//! - default-value diffs (always apply target's default; legacy rows untouched)
//! - column rename heuristics (we can't tell a rename from drop+add without an
//!   explicit oldName annotation — Phase 2+)
//!
//! Identifiers in emitted SQL are validated by [`validate_ident`] in
//! [`crate::provisioner`]. The diff never embeds raw spec strings into DDL.

use sqlx::{PgPool, Row};
use thiserror::Error;

use crate::ddl_builder::ColumnSpec;

/// A single delta between target and existing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOp {
    /// Target has a column the table is missing — add it (safe).
    AddColumn(ColumnSpec),
    /// Existing column missing from target — drop it (BREAKING).
    DropColumn { name: String },
    /// Both sides declare the column but the base type differs — alter (BREAKING).
    ChangeType { name: String, from: String, to: String },
    /// VARCHAR length increased — safe; decreased — BREAKING (silent truncation).
    ChangeLength { name: String, from: Option<u32>, to: Option<u32> },
    /// `NULL → NOT NULL` is breaking (existing nulls violate); reverse is safe.
    ChangeNullability { name: String, was_nullable: bool, now_nullable: bool },
}

impl MigrationOp {
    /// `true` if applying this op against existing data could lose information
    /// or break running readers.
    pub fn is_breaking(&self) -> bool {
        match self {
            MigrationOp::AddColumn(c) => {
                // Adding a NOT NULL column without a default is breaking — existing
                // rows have no value to put there. NOT NULL + default is safe.
                c.not_null
            }
            MigrationOp::DropColumn { .. } => true,
            MigrationOp::ChangeType { .. } => true,
            MigrationOp::ChangeLength { from, to, .. } => match (from, to) {
                (Some(a), Some(b)) => b < a, // shrink = breaking
                (None, Some(_)) => true,     // TEXT → VARCHAR(n) can truncate
                (Some(_), None) => false,    // VARCHAR(n) → TEXT widens
                (None, None) => false,
            },
            MigrationOp::ChangeNullability { was_nullable, now_nullable, .. } => {
                // Going from NULL-allowed to NOT NULL is breaking.
                *was_nullable && !*now_nullable
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum DiffError {
    #[error("breaking migration ops rejected (set annotation `velocity.sh/breaking-change: approved` to override): {0:?}")]
    BreakingOpsBlocked(Vec<MigrationOp>),

    /// Some breaking ops are recognised but the executor for them is not yet
    /// implemented (DropColumn, ChangeType). We refuse rather than silently
    /// no-op'ing them, so users don't think a destructive change ran when it
    /// didn't. Returns the ops the user would have to handle out-of-band.
    #[error("breaking migration ops recognised but not yet executable (deferred to Phase 2+); refusing to silently skip: {0:?}")]
    BreakingOpsDeferred(Vec<MigrationOp>),

    #[error("postgres error: {0}")]
    Sql(#[from] sqlx::Error),
}

/// Compute the diff between target columns (from [`build_ddl`]) and existing
/// columns (fetched via [`fetch_existing_columns`]).
///
/// System columns are filtered out — they're invariant by construction.
pub fn diff_columns(target: &[ColumnSpec], existing: &[ColumnSpec]) -> Vec<MigrationOp> {
    let mut ops = Vec::new();

    let user_target: Vec<&ColumnSpec> = target.iter().filter(|c| !c.system).collect();
    let user_existing: Vec<&ColumnSpec> = existing.iter().filter(|c| !c.system).collect();

    // Drops: in existing, not in target.
    for e in &user_existing {
        if !user_target.iter().any(|t| t.name == e.name) {
            ops.push(MigrationOp::DropColumn { name: e.name.clone() });
        }
    }

    // Adds + type/length/nullability changes.
    for t in &user_target {
        match user_existing.iter().find(|e| e.name == t.name) {
            None => ops.push(MigrationOp::AddColumn((*t).clone())),
            Some(e) => {
                if e.base_type != t.base_type {
                    ops.push(MigrationOp::ChangeType {
                        name: t.name.clone(),
                        from: e.base_type.clone(),
                        to: t.base_type.clone(),
                    });
                } else if e.length != t.length {
                    ops.push(MigrationOp::ChangeLength {
                        name: t.name.clone(),
                        from: e.length,
                        to: t.length,
                    });
                }
                if e.not_null != t.not_null {
                    ops.push(MigrationOp::ChangeNullability {
                        name: t.name.clone(),
                        was_nullable: !e.not_null,
                        now_nullable: !t.not_null,
                    });
                }
            }
        }
    }

    ops
}

/// Render the SQL for a single op. Returns `None` for breaking ops that this
/// module deliberately refuses to translate (DropColumn, ChangeType) — those
/// belong to a separate, audited path. Caller decides what to do with `None`.
pub fn op_to_sql(qualified_table: &str, op: &MigrationOp) -> Option<String> {
    Some(match op {
        MigrationOp::AddColumn(c) => {
            let ty = render_type(&c.base_type, c.length);
            let null = if c.not_null { " NOT NULL" } else { "" };
            format!("ALTER TABLE {qualified_table} ADD COLUMN IF NOT EXISTS {} {ty}{null};", c.name)
        }
        MigrationOp::ChangeLength { name, to, .. } => {
            let new_ty = render_type("varchar", *to);
            format!("ALTER TABLE {qualified_table} ALTER COLUMN {name} TYPE {new_ty};")
        }
        MigrationOp::ChangeNullability { name, now_nullable, .. } => {
            if *now_nullable {
                format!("ALTER TABLE {qualified_table} ALTER COLUMN {name} DROP NOT NULL;")
            } else {
                format!("ALTER TABLE {qualified_table} ALTER COLUMN {name} SET NOT NULL;")
            }
        }
        MigrationOp::DropColumn { .. } | MigrationOp::ChangeType { .. } => return None,
    })
}

/// Split ops into (safe, breaking). Safe ops are returned as ready-to-run SQL
/// statements; breaking ops are returned as-is for either error reporting or
/// (when approved) downstream rendering.
pub fn classify(
    qualified_table: &str,
    ops: Vec<MigrationOp>,
    allow_breaking: bool,
) -> Result<Vec<String>, DiffError> {
    let mut breaking = Vec::new();
    let mut safe_sql = Vec::new();
    // Breaking ops with no SQL renderer (DropColumn, ChangeType). We collect
    // these separately so that even with `allow_breaking = true` we refuse
    // rather than silently dropping the user's intent on the floor.
    let mut deferred = Vec::new();
    for op in ops {
        let is_breaking = op.is_breaking();
        match (is_breaking, op_to_sql(qualified_table, &op)) {
            (false, Some(sql)) => safe_sql.push(sql),
            (false, None) => {
                // Safe op with no SQL — currently unreachable (op_to_sql only
                // returns None for DropColumn/ChangeType, both breaking) but
                // we don't want a silent drop if that ever changes.
                deferred.push(op);
            }
            (true, Some(sql)) => {
                // Breaking but renderable (e.g. AddColumn NOT NULL, length
                // shrink, NOT NULL tightening). Held under `breaking` until
                // we know whether allow_breaking permits applying them.
                breaking.push((op, Some(sql)));
            }
            (true, None) => {
                breaking.push((op, None));
            }
        }
    }

    if !breaking.is_empty() && !allow_breaking {
        let ops: Vec<MigrationOp> = breaking.into_iter().map(|(op, _)| op).collect();
        return Err(DiffError::BreakingOpsBlocked(ops));
    }

    // allow_breaking is true (or breaking is empty). Apply renderable
    // breaking ops; refuse if any breaking op has no executor yet.
    for (op, sql) in breaking {
        match sql {
            Some(sql) => safe_sql.push(sql),
            None => deferred.push(op),
        }
    }

    if !deferred.is_empty() {
        return Err(DiffError::BreakingOpsDeferred(deferred));
    }

    Ok(safe_sql)
}

/// Read the live column list from Postgres for a given schema/table.
/// Returns an empty Vec if the table does not yet exist (caller treats that
/// as "first-run — execute the full CREATE TABLE").
pub async fn fetch_existing_columns(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnSpec>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT column_name, data_type, character_maximum_length, is_nullable
         FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2
         ORDER BY ordinal_position",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let name: String = r.get("column_name");
            let pg_type: String = r.get("data_type");
            let length: Option<i32> = r.get("character_maximum_length");
            let nullable: String = r.get("is_nullable");
            ColumnSpec {
                system: is_system_column(&name),
                name,
                base_type: normalise_pg_type(&pg_type),
                length: length.map(|n| n as u32),
                not_null: nullable == "NO",
            }
        })
        .collect())
}

fn is_system_column(name: &str) -> bool {
    matches!(
        name,
        "id" | "created_at"
            | "updated_at"
            | "deleted_at"
            | "version"
            | "created_by"
            | "updated_by"
            | "archived_at"
            | "archive_ref"
            // Phase 5b — generated FTS column. Not declared by the
            // user but always present on Tier-2+ schemas with
            // searchable fields. Treated as system so the diff layer
            // ignores it (and so `velocity drift check` doesn't flag
            // it as an orphan).
            | "__fts"
    )
}

/// Map Postgres' verbose `information_schema.data_type` to the canonical
/// vocabulary used by [`ColumnSpec::base_type`].
pub fn normalise_pg_type(pg_type: &str) -> String {
    match pg_type {
        "character varying" => "varchar",
        "timestamp with time zone" => "timestamptz",
        "timestamp without time zone" => "timestamp",
        // The rest already match: text, integer, bigint, numeric, boolean,
        // date, uuid, jsonb.
        other => other,
    }
    .to_string()
}

fn render_type(base: &str, length: Option<u32>) -> String {
    match (base, length) {
        ("varchar", Some(n)) => format!("VARCHAR({n})"),
        ("varchar", None) => "TEXT".into(),
        (other, _) => other.to_ascii_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, base: &str, len: Option<u32>, not_null: bool) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            base_type: base.into(),
            length: len,
            not_null,
            system: false,
        }
    }

    #[test]
    fn no_diff_when_identical() {
        let t = vec![col("po", "text", None, true)];
        let e = vec![col("po", "text", None, true)];
        assert_eq!(diff_columns(&t, &e), vec![]);
    }

    #[test]
    fn add_column_safe_when_nullable() {
        let t = vec![col("po", "text", None, false)];
        let ops = diff_columns(&t, &[]);
        assert_eq!(ops.len(), 1);
        assert!(!ops[0].is_breaking());
    }

    #[test]
    fn add_column_breaking_when_not_null() {
        let t = vec![col("po", "text", None, true)];
        let ops = diff_columns(&t, &[]);
        assert!(ops[0].is_breaking());
    }

    #[test]
    fn drop_column_breaking() {
        let e = vec![col("legacy", "text", None, false)];
        let ops = diff_columns(&[], &e);
        assert_eq!(ops, vec![MigrationOp::DropColumn { name: "legacy".into() }]);
        assert!(ops[0].is_breaking());
    }

    #[test]
    fn type_change_breaking() {
        let t = vec![col("amount", "numeric", None, false)];
        let e = vec![col("amount", "text", None, false)];
        let ops = diff_columns(&t, &e);
        assert!(matches!(&ops[0], MigrationOp::ChangeType { .. }));
        assert!(ops[0].is_breaking());
    }

    #[test]
    fn length_grow_safe_shrink_breaking() {
        let grow_ops = diff_columns(
            &[col("c", "varchar", Some(128), false)],
            &[col("c", "varchar", Some(64), false)],
        );
        assert!(!grow_ops[0].is_breaking());
        let shrink_ops = diff_columns(
            &[col("c", "varchar", Some(32), false)],
            &[col("c", "varchar", Some(64), false)],
        );
        assert!(shrink_ops[0].is_breaking());
    }

    #[test]
    fn nullability_tighten_breaking_relax_safe() {
        let tighten =
            diff_columns(&[col("c", "text", None, true)], &[col("c", "text", None, false)]);
        assert!(tighten[0].is_breaking());
        let relax = diff_columns(&[col("c", "text", None, false)], &[col("c", "text", None, true)]);
        assert!(!relax[0].is_breaking());
    }

    #[test]
    fn system_columns_ignored() {
        let mut sys = col("id", "uuid", None, true);
        sys.system = true;
        let mut sys_existing = col("id", "uuid", None, true);
        sys_existing.system = true;
        // Even if values differ across system columns (which shouldn't happen
        // in practice), the diff ignores them — they are invariant by
        // construction and forcing them through would mask real user-column
        // diffs in error reporting.
        let ops = diff_columns(&[sys], &[sys_existing]);
        assert_eq!(ops, vec![]);
    }

    #[test]
    fn classify_blocks_breaking_by_default() {
        let ops = vec![MigrationOp::DropColumn { name: "legacy".into() }];
        let err = classify("s.t", ops, false).unwrap_err();
        assert!(matches!(err, DiffError::BreakingOpsBlocked(_)));
    }

    #[test]
    fn classify_approves_renderable_breaking_ops() {
        // Approval + a renderable breaking op (NOT NULL tighten) → SQL emitted.
        let ops = vec![MigrationOp::ChangeNullability {
            name: "amount".into(),
            was_nullable: true,
            now_nullable: false,
        }];
        let sql = classify("s.t", ops, true).unwrap();
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("SET NOT NULL"));
    }

    #[test]
    fn classify_defers_unrenderable_breaking_even_when_approved() {
        // Approval is not enough for DropColumn — we have no executor for it
        // yet, and we must NOT silently no-op a destructive intent. Refuse
        // with a distinct error so the controller can surface "deferred".
        let ops = vec![
            MigrationOp::DropColumn { name: "legacy".into() },
            MigrationOp::AddColumn(col("po", "text", None, false)),
        ];
        let err = classify("s.t", ops, true).unwrap_err();
        match err {
            DiffError::BreakingOpsDeferred(deferred) => {
                assert_eq!(deferred.len(), 1);
                assert!(matches!(deferred[0], MigrationOp::DropColumn { .. }));
            }
            other => panic!("expected BreakingOpsDeferred, got {other:?}"),
        }
    }

    #[test]
    fn op_to_sql_renders_add_column_with_nullable_default() {
        let op = MigrationOp::AddColumn(col("note", "text", None, false));
        assert_eq!(
            op_to_sql("s.t", &op).unwrap(),
            "ALTER TABLE s.t ADD COLUMN IF NOT EXISTS note TEXT;"
        );
    }

    #[test]
    fn op_to_sql_renders_length_change() {
        let op = MigrationOp::ChangeLength { name: "code".into(), from: Some(64), to: Some(128) };
        assert!(op_to_sql("s.t", &op).unwrap().contains("TYPE VARCHAR(128)"));
    }

    #[test]
    fn op_to_sql_renders_nullability() {
        let drop = MigrationOp::ChangeNullability {
            name: "c".into(),
            was_nullable: false,
            now_nullable: true,
        };
        assert!(op_to_sql("s.t", &drop).unwrap().contains("DROP NOT NULL"));
        let set = MigrationOp::ChangeNullability {
            name: "c".into(),
            was_nullable: true,
            now_nullable: false,
        };
        assert!(op_to_sql("s.t", &set).unwrap().contains("SET NOT NULL"));
    }

    #[test]
    fn op_to_sql_refuses_breaking_ops() {
        assert!(op_to_sql("s.t", &MigrationOp::DropColumn { name: "x".into() }).is_none());
        assert!(op_to_sql(
            "s.t",
            &MigrationOp::ChangeType { name: "x".into(), from: "text".into(), to: "uuid".into() },
        )
        .is_none());
    }

    #[test]
    fn normalise_pg_type_maps_canonical_forms() {
        assert_eq!(normalise_pg_type("character varying"), "varchar");
        assert_eq!(normalise_pg_type("timestamp with time zone"), "timestamptz");
        assert_eq!(normalise_pg_type("text"), "text");
        assert_eq!(normalise_pg_type("jsonb"), "jsonb");
    }
}
