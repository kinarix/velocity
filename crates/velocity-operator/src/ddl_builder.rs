//! DDL generation for `SchemaDefinition`.
//!
//! Pure function: given a `SchemaDefinitionSpec` + path metadata, produces a
//! [`DdlPlan`] of executable statements. The provisioner is responsible for
//! running them in order inside a single transaction.
//!
//! Conventions tracked here (see `docs/design.md §3`):
//!
//! - mandatory columns: `id`, `created_at`, `updated_at`, `deleted_at`,
//!   `version`, `created_by`, `updated_by`, `archived_at`, `archive_ref`
//! - partial unique indexes (`WHERE deleted_at IS NULL`) — soft-deleted rows
//!   never block re-use of unique values
//! - history table with same column shape + `history_id`/`changed_at`/`op`
//! - outbox table + audit/outbox trigger for Tier-3 schemas
//! - updated-at touch trigger for every table
//!
//! ## Safety
//!
//! Every identifier (schema name, table name, column name) flows through
//! [`validate_ident`](super::provisioner::validate_ident). Literal values from
//! the spec (`enum_values`, `default`, `pattern`) are quoted via
//! [`quote_literal`]. We never interpolate raw spec strings into DDL.

use thiserror::Error;
use velocity_types::common::{sanitize, SchemaPath};
use velocity_types::crds::schema::{
    FieldKind, FieldSpec, FtsWeight, SchemaDefinitionSpec, SearchTier,
};

use crate::provisioner::{validate_ident, ProvisionError};

/// Auto-provisioned columns (design §3.2). Order matters for the generated
/// `CREATE TABLE` to read top-down.
const SYSTEM_COLUMNS: &[(&str, &str)] = &[
    ("id", "UUID NOT NULL DEFAULT gen_random_uuid()"),
    ("created_at", "TIMESTAMPTZ NOT NULL DEFAULT now()"),
    ("updated_at", "TIMESTAMPTZ NOT NULL DEFAULT now()"),
    ("deleted_at", "TIMESTAMPTZ"),
    ("version", "INTEGER NOT NULL DEFAULT 1"),
    // `app.current_user` is set by the API in the transaction prelude
    // (ADR-007). Defaulting the column to that setting means even direct
    // INSERTs through psql get tagged with the caller, and the API
    // handler never has to remember to bind these columns.
    ("created_by", "TEXT NOT NULL DEFAULT current_setting('app.current_user', true)"),
    ("updated_by", "TEXT NOT NULL DEFAULT current_setting('app.current_user', true)"),
    ("archived_at", "TIMESTAMPTZ"),
    ("archive_ref", "TEXT"),
];

/// Reserved column names — user fields may not collide with system columns.
const RESERVED_FIELD_NAMES: &[&str] = &[
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

#[derive(Debug, Error)]
pub enum DdlError {
    #[error(transparent)]
    Provision(#[from] ProvisionError),

    #[error("field `{0}` collides with reserved system column")]
    ReservedFieldName(String),

    #[error("enum field `{0}` declares no enum_values")]
    EmptyEnum(String),

    #[error("ref field `{0}` missing target object reference")]
    RefMissingTarget(String),

    #[error("default value for field `{field}` is not representable in SQL: {reason}")]
    UnsupportedDefault { field: String, reason: String },
}

/// All statements needed to bring a schema's tables into the target state.
/// Generated in execution order — the provisioner just runs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlPlan {
    /// Fully-qualified Postgres table name (`{schema}.{table}`).
    pub qualified_table: String,
    /// `CREATE TABLE {schema}.{object}_{version} ( ... )`
    pub main_table: String,
    /// Structured column list of the main table — feeds [`migration_diff`](crate::migration_diff).
    pub columns: Vec<ColumnSpec>,
    /// `CREATE TABLE {schema}.{object}_{version}_history ( ... )`
    pub history_table: String,
    /// Tier-3 only — outbox table for CDC (ADR-002).
    pub outbox_table: Option<String>,
    /// `CREATE INDEX ...` — main table, in stable order.
    pub indexes: Vec<String>,
    /// PL/pgSQL functions + triggers (updated_at touch, history+outbox).
    pub triggers: Vec<String>,
    /// Layer-7 RLS — `ALTER TABLE ENABLE ROW LEVEL SECURITY` plus one
    /// permissive policy per `(table, user-role)` derived from
    /// `spec.access.rowFilter`. The API reads `app.scoped_roles` to
    /// signal which policies apply; see [`crate::ddl_builder::build_rls_policies`]
    /// for the encoding.
    pub rls_policies: Vec<String>,
    /// Phase 5d — the canonical `__fts` generated-column expression
    /// (the body of `GENERATED ALWAYS AS (...) STORED`) for this
    /// schema. `None` when the schema doesn't carry a tsvector column
    /// (Tier 1, or Tier 2/3 with no searchable fields). Used by the
    /// migration-diff layer to detect a searchable-set or weight
    /// change and emit `DROP COLUMN __fts; ADD COLUMN __fts ...` —
    /// generated columns aren't ALTER-able in Postgres, so it's
    /// drop-and-readd or nothing.
    pub fts_expression: Option<String>,
}

/// Structured view of one column. Used by [`DdlPlan`] and the diff layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    /// Lowercase base type as Postgres reports it (e.g. `text`, `numeric`,
    /// `bigint`, `boolean`, `timestamptz`, `uuid`, `jsonb`, `varchar`).
    /// Length/precision modifiers are NOT part of this — they live on
    /// [`length`](Self::length) so the diff treats `VARCHAR(64)` and `TEXT`
    /// as type-incompatible but `VARCHAR(64)` and `VARCHAR(128)` as a
    /// (safe) length change.
    pub base_type: String,
    pub length: Option<u32>,
    pub not_null: bool,
    /// True for the 9 mandatory system columns — diff layer ignores these.
    pub system: bool,
}

/// Build a complete [`DdlPlan`] for a `SchemaDefinition`.
pub fn build_ddl(spec: &SchemaDefinitionSpec, path: &SchemaPath) -> Result<DdlPlan, DdlError> {
    let schema_name =
        validate_ident(&sanitize(&format!("{}_{}_{}", path.org, path.app, path.domain)))?;
    let version_sfx = validate_ident(&sanitize(&path.version))?;
    let table = validate_ident(&format!("{}_{}", sanitize_object(&path.object)?, version_sfx))?;
    let qualified = format!("{schema_name}.{table}");

    let columns = build_columns(spec, &table)?;
    let column_specs = build_column_specs(spec)?;
    // Phase 5b/5d — collect (column, weight) pairs for the tsvector
    // generated column. Only Tier 2 and Tier 3 get one; Tier 1 stays
    // on plain B-tree filters. Phase 5d adds per-field weights via
    // `setweight(...)`; an absent `ftsWeight` falls back to `D` which
    // is exactly the weight Phase 5b's uniform expression produced
    // implicitly (no setweight() == every position weighted D).
    let fts_columns: Vec<(String, FtsWeight)> = match spec.search.tier {
        SearchTier::Tier2 | SearchTier::Tier3 => spec
            .fields
            .iter()
            .filter(|f| {
                f.searchable && matches!(f.kind, FieldKind::String | FieldKind::Enum)
            })
            .map(|f| (sanitize(&f.name), f.fts_weight.unwrap_or_default()))
            .collect(),
        SearchTier::Tier1 => Vec::new(),
    };
    let fts_expression = build_fts_expression(&fts_columns);
    let main_table = build_create_table_with_fts(
        &qualified,
        &columns,
        &table,
        fts_expression.as_deref(),
    )?;
    let history_table = build_history_table(&schema_name, &table)?;
    let outbox_table = match spec.search.tier {
        SearchTier::Tier3 => Some(build_outbox_table(&schema_name, &table)),
        _ => None,
    };
    let mut indexes = build_indexes(spec, &schema_name, &table)?;
    if !fts_columns.is_empty() {
        // Phase 5b — GIN on __fts. Standard tsvector index; matches
        // websearch_to_tsquery() at query time.
        indexes.push(format!(
            "CREATE INDEX IF NOT EXISTS idx_{table}_fts ON {qualified} USING GIN (__fts);"
        ));
    }
    let triggers = build_triggers(spec, &schema_name, &table);
    let rls_policies = build_rls_policies(spec, &schema_name, &table)?;

    Ok(DdlPlan {
        qualified_table: qualified,
        main_table,
        columns: column_specs,
        history_table,
        outbox_table,
        indexes,
        triggers,
        rls_policies,
        fts_expression,
    })
}

/// Phase 5d — build the body of the `__fts` generated column from the
/// (sanitised column name, weight) pairs in declaration order. Returns
/// `None` when the schema has no searchable fields; otherwise a string
/// like `setweight(to_tsvector('english', coalesce(title, '')), 'A')
/// || setweight(to_tsvector('english', coalesce(body, '')), 'D')`.
///
/// Declaration order matters: Postgres compares generated-column
/// expressions as text, so reordering the same fields would force a
/// rebuild on every reconcile. Keeping the order tied to spec.fields
/// makes the hash stable.
fn build_fts_expression(fields: &[(String, FtsWeight)]) -> Option<String> {
    if fields.is_empty() {
        return None;
    }
    let parts: Vec<String> = fields
        .iter()
        .map(|(col, w)| {
            format!(
                "setweight(to_tsvector('english', coalesce({col}, '')), '{}')",
                w.as_pg_char()
            )
        })
        .collect();
    Some(parts.join(" || "))
}

/// Auto-provisioned columns as typed [`ColumnSpec`] values — used by the diff
/// layer. Keep in sync with [`SYSTEM_COLUMNS`].
fn system_column_specs() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec {
            name: "id".into(),
            base_type: "uuid".into(),
            length: None,
            not_null: true,
            system: true,
        },
        ColumnSpec {
            name: "created_at".into(),
            base_type: "timestamptz".into(),
            length: None,
            not_null: true,
            system: true,
        },
        ColumnSpec {
            name: "updated_at".into(),
            base_type: "timestamptz".into(),
            length: None,
            not_null: true,
            system: true,
        },
        ColumnSpec {
            name: "deleted_at".into(),
            base_type: "timestamptz".into(),
            length: None,
            not_null: false,
            system: true,
        },
        ColumnSpec {
            name: "version".into(),
            base_type: "integer".into(),
            length: None,
            not_null: true,
            system: true,
        },
        ColumnSpec {
            name: "created_by".into(),
            base_type: "text".into(),
            length: None,
            not_null: true,
            system: true,
        },
        ColumnSpec {
            name: "updated_by".into(),
            base_type: "text".into(),
            length: None,
            not_null: true,
            system: true,
        },
        ColumnSpec {
            name: "archived_at".into(),
            base_type: "timestamptz".into(),
            length: None,
            not_null: false,
            system: true,
        },
        ColumnSpec {
            name: "archive_ref".into(),
            base_type: "text".into(),
            length: None,
            not_null: false,
            system: true,
        },
    ]
}

fn build_column_specs(spec: &SchemaDefinitionSpec) -> Result<Vec<ColumnSpec>, DdlError> {
    let mut out = system_column_specs();
    for f in &spec.fields {
        let name = validate_ident(&sanitize(&f.name))?;
        if RESERVED_FIELD_NAMES.contains(&name.as_str()) {
            return Err(DdlError::ReservedFieldName(name));
        }
        let (base_type, length) = pg_base_type_for(f)?;
        out.push(ColumnSpec { name, base_type, length, not_null: f.required, system: false });
    }
    Ok(out)
}

/// Canonical (base_type, length) — matches what `information_schema.columns`
/// reports after normalisation by [`crate::migration_diff::normalise_pg_type`].
fn pg_base_type_for(f: &FieldSpec) -> Result<(String, Option<u32>), DdlError> {
    let pair = match f.kind {
        FieldKind::String => match f.max_length {
            Some(n) if n > 0 => ("varchar".into(), Some(n)),
            _ => ("text".into(), None),
        },
        FieldKind::Integer => ("bigint".into(), None),
        FieldKind::Number => ("numeric".into(), None),
        FieldKind::Boolean => ("boolean".into(), None),
        FieldKind::Date => ("date".into(), None),
        FieldKind::Datetime => ("timestamptz".into(), None),
        FieldKind::Uuid => ("uuid".into(), None),
        FieldKind::Json => ("jsonb".into(), None),
        FieldKind::Enum => {
            if f.enum_values.is_empty() {
                return Err(DdlError::EmptyEnum(f.name.clone()));
            }
            ("text".into(), None)
        }
        FieldKind::Ref => {
            if f.r#ref.is_none() {
                return Err(DdlError::RefMissingTarget(f.name.clone()));
            }
            ("text".into(), None)
        }
    };
    Ok(pair)
}

// ─── Columns ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ColumnDef {
    name: String,
    sql_type: String,
    not_null: bool,
    default: Option<String>,
    check: Option<String>,
}

fn build_columns(spec: &SchemaDefinitionSpec, table: &str) -> Result<Vec<ColumnDef>, DdlError> {
    let mut out: Vec<ColumnDef> = SYSTEM_COLUMNS
        .iter()
        .map(|(name, ddl)| ColumnDef {
            name: (*name).to_string(),
            sql_type: (*ddl).to_string(),
            not_null: false, // baked into sql_type above
            default: None,
            check: None,
        })
        .collect();

    for f in &spec.fields {
        let col_name = validate_ident(&sanitize(&f.name))?;
        if RESERVED_FIELD_NAMES.contains(&col_name.as_str()) {
            return Err(DdlError::ReservedFieldName(col_name));
        }
        let sql_type = pg_type_for(f)?;
        let check = enum_check_clause(f, table)?;
        let default = field_default_sql(f)?;

        out.push(ColumnDef { name: col_name, sql_type, not_null: f.required, default, check });
    }

    Ok(out)
}

fn pg_type_for(f: &FieldSpec) -> Result<String, DdlError> {
    let t = match f.kind {
        FieldKind::String => match f.max_length {
            Some(n) if n > 0 => format!("VARCHAR({n})"),
            _ => "TEXT".to_string(),
        },
        FieldKind::Integer => "BIGINT".to_string(),
        FieldKind::Number => "NUMERIC(19,4)".to_string(),
        FieldKind::Boolean => "BOOLEAN".to_string(),
        FieldKind::Date => "DATE".to_string(),
        FieldKind::Datetime => "TIMESTAMPTZ".to_string(),
        FieldKind::Uuid => "UUID".to_string(),
        FieldKind::Json => "JSONB".to_string(),
        FieldKind::Enum => {
            if f.enum_values.is_empty() {
                return Err(DdlError::EmptyEnum(f.name.clone()));
            }
            "TEXT".to_string()
        }
        FieldKind::Ref => {
            if f.r#ref.is_none() {
                return Err(DdlError::RefMissingTarget(f.name.clone()));
            }
            "TEXT".to_string()
        }
    };
    Ok(t)
}

fn enum_check_clause(f: &FieldSpec, table: &str) -> Result<Option<String>, DdlError> {
    if f.kind != FieldKind::Enum {
        return Ok(None);
    }
    let col = sanitize(&f.name);
    let values: Vec<String> = f.enum_values.iter().map(|v| quote_literal(v)).collect();
    Ok(Some(format!("CONSTRAINT chk_{table}_{col}_enum CHECK ({col} IN ({}))", values.join(", "))))
}

fn field_default_sql(f: &FieldSpec) -> Result<Option<String>, DdlError> {
    let Some(v) = f.default.as_ref() else { return Ok(None) };
    // Only well-formed scalar/JSON defaults are supported. Anything else means
    // the spec is wrong, not that we should silently drop it.
    let sql = match (&f.kind, v) {
        (FieldKind::String | FieldKind::Enum, serde_json::Value::String(s)) => quote_literal(s),
        (FieldKind::Integer, serde_json::Value::Number(n)) if n.is_i64() => n.to_string(),
        (FieldKind::Number, serde_json::Value::Number(n)) => n.to_string(),
        (FieldKind::Boolean, serde_json::Value::Bool(b)) => {
            if *b { "TRUE" } else { "FALSE" }.into()
        }
        (FieldKind::Json, _) => format!("{}::jsonb", quote_literal(&v.to_string())),
        (FieldKind::Uuid, serde_json::Value::String(s)) => format!("{}::uuid", quote_literal(s)),
        (FieldKind::Date | FieldKind::Datetime, serde_json::Value::String(s)) => quote_literal(s),
        _ => {
            return Err(DdlError::UnsupportedDefault {
                field: f.name.clone(),
                reason: format!("type {:?} cannot accept default {}", f.kind, v),
            })
        }
    };
    Ok(Some(sql))
}

// ─── CREATE TABLE ───────────────────────────────────────────────────────────

/// Build a `CREATE TABLE IF NOT EXISTS` with an optional `__fts`
/// column when `fts_expression` is `Some(expr)`. Lifted out so the
/// Phase 5b plan can opt in for Tier-2 schemas without rewriting the
/// entire builder. Phase 5d swapped the input from a column list to
/// a pre-rendered expression so per-field weights are expressed once
/// (in [`build_fts_expression`]) and the migration-diff layer can use
/// the same string to detect drift.
fn build_create_table_with_fts(
    qualified: &str,
    columns: &[ColumnDef],
    table: &str,
    fts_expression: Option<&str>,
) -> Result<String, DdlError> {
    // String::write_fmt is infallible — writing to a String never fails.
    let mut s = String::with_capacity(1024);
    s.push_str(&format!("CREATE TABLE IF NOT EXISTS {qualified} (\n"));
    let mut lines: Vec<String> = Vec::with_capacity(columns.len() + 4);
    for c in columns {
        let mut line = format!("    {} {}", c.name, c.sql_type);
        if c.not_null && !c.sql_type.contains("NOT NULL") {
            line.push_str(" NOT NULL");
        }
        if let Some(d) = &c.default {
            line.push_str(&format!(" DEFAULT {d}"));
        }
        if let Some(check) = &c.check {
            line.push_str(&format!(" {check}"));
        }
        lines.push(line);
    }
    // Phase 5b — Tier-2 FTS stored generated column. Phase 5d added
    // per-field weights, so the expression is now `setweight(...)
    // || setweight(...) || ...` — see [`build_fts_expression`].
    // Skipped silently when there are no searchable fields — a
    // Tier-2 schema with `searchable: false` everywhere is
    // well-formed; it just doesn't get FTS.
    if let Some(expr) = fts_expression {
        lines.push(format!(
            "    __fts tsvector GENERATED ALWAYS AS ({expr}) STORED"
        ));
    }
    lines.push(format!("    CONSTRAINT {table}_pkey PRIMARY KEY (id)"));
    s.push_str(&lines.join(",\n"));
    s.push_str("\n);");
    Ok(s)
}

// ─── History ────────────────────────────────────────────────────────────────

fn build_history_table(schema: &str, table: &str) -> Result<String, DdlError> {
    let hist = format!("{schema}.{table}_history");
    // History rows are immutable snapshots — no constraints beyond pk + index.
    // Real time-machine columns (op, changed_at, actor) land in Phase 3; for
    // now we capture the bare minimum so the trigger can write.
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {hist} (
    history_id   BIGSERIAL PRIMARY KEY,
    entity_id    UUID NOT NULL,
    op           TEXT NOT NULL,
    changed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor        TEXT,
    snapshot     JSONB NOT NULL
);"
    ))
}

// ─── Outbox ─────────────────────────────────────────────────────────────────

fn build_outbox_table(schema: &str, table: &str) -> String {
    let outbox = format!("{schema}.{table}_outbox");
    format!(
        "CREATE TABLE IF NOT EXISTS {outbox} (
    id           BIGSERIAL PRIMARY KEY,
    op           TEXT NOT NULL,
    entity_id    UUID NOT NULL,
    payload      JSONB,
    occurred_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_{table}_outbox_unpub
    ON {outbox} (id) WHERE published_at IS NULL;"
    )
}

// ─── Indexes ────────────────────────────────────────────────────────────────

fn build_indexes(
    spec: &SchemaDefinitionSpec,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, DdlError> {
    let mut out = Vec::new();
    let qualified = format!("{schema}.{table}");

    // Soft-delete partial index — every table gets one.
    out.push(format!(
        "CREATE INDEX IF NOT EXISTS idx_{table}_active \
         ON {qualified} (deleted_at) WHERE deleted_at IS NULL;"
    ));

    for f in &spec.fields {
        let col = validate_ident(&sanitize(&f.name))?;

        // Partial unique on `unique: true` (design §3.3).
        if f.unique {
            out.push(format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_{table}_{col}_active \
                 ON {qualified} ({col}) WHERE deleted_at IS NULL;"
            ));
        }

        // Plain B-tree for indexed / filterable / sortable scalars.
        let scalar = !matches!(f.kind, FieldKind::Json);
        if scalar && (f.indexed || f.filterable || f.sortable) {
            out.push(format!(
                "CREATE INDEX IF NOT EXISTS idx_{table}_{col} ON {qualified} ({col});"
            ));
        }

        // GIN for JSONB.
        if f.kind == FieldKind::Json && (f.indexed || f.filterable) {
            out.push(format!(
                "CREATE INDEX IF NOT EXISTS idx_{table}_{col}_gin \
                 ON {qualified} USING GIN ({col});"
            ));
        }
    }

    // De-dup while preserving first occurrence — protects against the same
    // field being indexed for multiple reasons.
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    Ok(out)
}

// ─── RLS policies (Layer 7) ─────────────────────────────────────────────────

/// Build Layer-7 RLS DDL for the table.
///
/// Always emits:
/// - `ALTER TABLE … ENABLE ROW LEVEL SECURITY`
/// - A *wildcard* permissive policy `pol_{table}_unrestricted` that
///   admits a row whenever `app.scoped_roles` contains the literal
///   `*` sentinel. Both "schema declares no rowFilter" and "actor has
///   an unrestricted role" collapse to the same `*` value on the API
///   side (see [`crate::row_filter::scoped_roles_for_session`]), so the
///   policy only checks for that single sentinel. Empty string is the
///   *deny* sentinel and must NOT admit — matches the SQL-fragment
///   path which renders to `(false)` in that case.
///
/// Then, for each *user-role* that has at least one `rowFilter[]` entry,
/// one scoped permissive policy `pol_{table}_role_{role}`:
///
///   `USING (member-of('app.scoped_roles', '<role>') AND <all clauses ANDed>)`
///
/// Postgres ORs permissive policies for the same command on the same
/// role, which gives us free union across user-roles. ANDing clauses
/// within a user-role's policy preserves the row_filter.rs semantic
/// "multiple entries for the same role AND together; different roles
/// OR" — see [`crate::row_filter`] for the matching API-side rendering.
///
/// We DROP every named policy before re-creating it so a reconcile after
/// a `rowFilter[]` edit replaces the predicate cleanly. Drop is gated on
/// `IF EXISTS` so the first-run path is fine.
///
/// **Identifier safety**: the policy name embeds the user-role string,
/// which can contain `-` and other characters that aren't valid in a
/// Postgres identifier. We hash unsanitisable role names into a
/// stable suffix so two roles never collide on the same policy name.
fn build_rls_policies(
    spec: &SchemaDefinitionSpec,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, DdlError> {
    use std::collections::BTreeMap;
    use velocity_types::crds::schema::RowFilterRule;

    let qualified = format!("{schema}.{table}");
    let mut out = Vec::new();

    // 1. Always enable RLS — even with no policies declared, the wildcard
    //    policy below admits the right traffic.
    out.push(format!("ALTER TABLE {qualified} ENABLE ROW LEVEL SECURITY;"));

    // 2. Group rowFilter entries by user-role. BTreeMap so the output is
    //    deterministic across reconciles (important for idempotency
    //    checks at the migration_diff layer).
    let mut by_role: BTreeMap<&str, Vec<&RowFilterRule>> = BTreeMap::new();
    for rule in &spec.access.row_filter {
        by_role.entry(rule.role.as_str()).or_default().push(rule);
    }

    // 3. Drop *every* policy this builder might have emitted previously,
    //    so a `rowFilter` edit doesn't leave stale predicates behind.
    //    We can't enumerate "all old policies" (we'd need to query the
    //    catalog), but we *can* drop the names we'd emit *now* plus the
    //    wildcard — anything previously emitted for a now-removed user
    //    role lingers and must be cleaned up by a manual ops procedure
    //    or a future "list pg_policies and reconcile" pass. Documented.
    out.push(format!(
        "DROP POLICY IF EXISTS pol_{table}_unrestricted ON {qualified};"
    ));
    for role in by_role.keys() {
        let suffix = policy_role_suffix(role);
        out.push(format!("DROP POLICY IF EXISTS pol_{table}_role_{suffix} ON {qualified};"));
    }

    // 4. Wildcard admit. The API encodes any path that should see every
    //    row (no rowFilter declared OR caller has an unrestricted role)
    //    as `app.scoped_roles = '*'`. Empty string `''` is the *deny*
    //    sentinel (compiled rules + zero matched user-roles) and must
    //    NOT admit here — defense-in-depth requires this policy to
    //    match the SQL fragment's `(false)` rendering for that case.
    //    A NULL setting (prelude missing entirely) also fails closed:
    //    `current_setting('app.scoped_roles', true)` returns NULL when
    //    unset → `NULL = '*'` is NULL → row excluded.
    out.push(format!(
        "CREATE POLICY pol_{table}_unrestricted ON {qualified} \
         AS PERMISSIVE FOR ALL \
         USING ( \
           current_setting('app.scoped_roles', true) = '*' \
         );"
    ));

    // 5. Per-user-role scoped policies.
    for (role, rules) in &by_role {
        let suffix = policy_role_suffix(role);
        let mut and_parts: Vec<String> = Vec::with_capacity(rules.len());
        for r in rules {
            and_parts.push(render_clause_as_literal(&r.filter, table)?);
        }
        let predicate = and_parts.join(" AND ");
        // Membership check: split `app.scoped_roles` on `,` and look for
        // the user-role literal. Using `= ANY (string_to_array(...))`
        // avoids array-position quirks and is short-circuited by the
        // planner when the setting is `*` (the wildcard above already
        // matched, but Postgres still considers this policy in the OR
        // — harmless, just an extra evaluation).
        let role_literal = quote_literal(role);
        out.push(format!(
            "CREATE POLICY pol_{table}_role_{suffix} ON {qualified} \
             AS PERMISSIVE FOR ALL \
             USING ( \
               {role_literal} = ANY (string_to_array(current_setting('app.scoped_roles', true), ',')) \
               AND ({predicate}) \
             );"
        ));
    }

    Ok(out)
}

/// Render one [`RowFilter`](velocity_types::crds::schema::RowFilter) into a
/// SQL fragment with the value inlined as a literal. The API side (row_filter.rs)
/// emits the same predicate via `$N` binds; the two must agree on op + value
/// shape. Restricted to scalar JSON values (string / number / bool) — anything
/// richer needs explicit handling and is rejected as `UnsupportedDefault` so a
/// CRD with `value: { … }` fails apply rather than producing an injection
/// vector.
fn render_clause_as_literal(
    filter: &velocity_types::crds::schema::RowFilter,
    _table: &str,
) -> Result<String, DdlError> {
    let col = validate_ident(&sanitize(&filter.field))?;
    let op_sql = match filter.op.as_str() {
        "eq" => "=",
        "neq" => "<>",
        "gt" => ">",
        "gte" => ">=",
        "lt" => "<",
        "lte" => "<=",
        other => {
            return Err(DdlError::UnsupportedDefault {
                field: filter.field.clone(),
                reason: format!("rowFilter op `{other}` is not supported in RLS DDL"),
            });
        }
    };
    let value_sql = match &filter.value {
        serde_json::Value::String(s) => quote_literal(s),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => {
            if *b { "TRUE".into() } else { "FALSE".into() }
        }
        other => {
            return Err(DdlError::UnsupportedDefault {
                field: filter.field.clone(),
                reason: format!("rowFilter value `{other}` is not a scalar"),
            });
        }
    };
    Ok(format!("{col} {op_sql} {value_sql}"))
}

/// Build a safe identifier suffix for a user-role string. Postgres
/// identifiers can't carry `-`, `.`, or other special chars; we
/// sanitise them and append a short hash to keep distinct roles from
/// colliding when they sanitise to the same string (e.g. `pii-reader`
/// and `pii.reader`).
fn policy_role_suffix(role: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let sanitised: String = role
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hasher = DefaultHasher::new();
    role.hash(&mut hasher);
    let suffix = format!("{:x}", hasher.finish() & 0xffff_ffff);
    // 8-hex-char hash bound + truncated sanitised body keeps the
    // identifier well under Postgres' 63-char cap even on long table
    // names.
    let head: String = sanitised.chars().take(40).collect();
    format!("{head}_{suffix}")
}

// ─── Triggers ───────────────────────────────────────────────────────────────

fn build_triggers(spec: &SchemaDefinitionSpec, schema: &str, table: &str) -> Vec<String> {
    let qualified = format!("{schema}.{table}");
    let hist = format!("{schema}.{table}_history");
    let outbox = format!("{schema}.{table}_outbox");
    let tier3 = matches!(spec.search.tier, SearchTier::Tier3);

    // updated_at touch
    let touch_fn = format!(
        "CREATE OR REPLACE FUNCTION {schema}.{table}_touch() \
         RETURNS TRIGGER AS $$ \
         BEGIN \
             NEW.updated_at := now(); \
             NEW.version := COALESCE(OLD.version, 0) + 1; \
             RETURN NEW; \
         END; \
         $$ LANGUAGE plpgsql;"
    );
    let touch_trg = format!(
        "DROP TRIGGER IF EXISTS trg_{table}_touch ON {qualified};
CREATE TRIGGER trg_{table}_touch \
 BEFORE UPDATE ON {qualified} \
 FOR EACH ROW EXECUTE FUNCTION {schema}.{table}_touch();"
    );

    // history + (optional) outbox writer
    let outbox_branch = if tier3 {
        format!(
            "INSERT INTO {outbox} (op, entity_id, payload) \
             VALUES (TG_OP, COALESCE(NEW.id, OLD.id), \
                     CASE WHEN TG_OP = 'DELETE' THEN NULL ELSE to_jsonb(NEW) END);"
        )
    } else {
        String::new()
    };

    let hist_fn = format!(
        "CREATE OR REPLACE FUNCTION {schema}.{table}_audit() \
         RETURNS TRIGGER AS $$ \
         DECLARE \
             v_actor TEXT := current_setting('app.current_user', true); \
         BEGIN \
             INSERT INTO {hist} (entity_id, op, actor, snapshot) \
             VALUES ( \
                 COALESCE(NEW.id, OLD.id), \
                 TG_OP, \
                 v_actor, \
                 CASE WHEN TG_OP = 'DELETE' THEN to_jsonb(OLD) ELSE to_jsonb(NEW) END \
             ); \
             {outbox_branch} \
             RETURN COALESCE(NEW, OLD); \
         END; \
         $$ LANGUAGE plpgsql;"
    );

    let hist_trg = format!(
        "DROP TRIGGER IF EXISTS trg_{table}_audit ON {qualified};
CREATE TRIGGER trg_{table}_audit \
 AFTER INSERT OR UPDATE OR DELETE ON {qualified} \
 FOR EACH ROW EXECUTE FUNCTION {schema}.{table}_audit();"
    );

    vec![touch_fn, touch_trg, hist_fn, hist_trg]
}

// ─── helpers ────────────────────────────────────────────────────────────────

fn sanitize_object(name: &str) -> Result<String, ProvisionError> {
    validate_ident(&sanitize(name))
}

/// SQL string literal — escape embedded single quotes. Identifiers must come
/// from [`validate_ident`]; this is only for VALUES / CHECK constants.
fn quote_literal(s: &str) -> String {
    let escaped = s.replace('\'', "''");
    format!("'{escaped}'")
}

// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    fn path() -> SchemaPath {
        SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1")
    }

    fn auth() -> AuthSpec {
        AuthSpec {
            strategy_ref: velocity_types::common::NamespacedRef {
                name: "default".into(),
                namespace: "acme-platform".into(),
            },
            overrides: Vec::new(),
        }
    }

    fn minimal_spec(fields: Vec<FieldSpec>, tier: SearchTier) -> SchemaDefinitionSpec {
        SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: auth(),
            access: AccessSpec::default(),
            fields,
            validations: Vec::new(),
            search: SearchSpec { tier, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        }
    }

    fn field(name: &str, kind: FieldKind) -> FieldSpec {
        FieldSpec {
            name: name.into(),
            kind,
            required: false,
            unique: false,
            indexed: false,
            filterable: false,
            sortable: false,
            searchable: false,
            fts_weight: None,
            default: None,
            min: None,
            max: None,
            max_length: None,
            pattern: None,
            enum_values: Vec::new(),
            r#ref: None,
            sensitivity: None,
            access: None,
            mask: None,
        }
    }

    #[test]
    fn builds_main_table_with_system_columns() {
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        assert_eq!(plan.qualified_table, "acme_supply_chain_procurement.purchase_order_v1");
        assert!(plan.main_table.contains("id UUID NOT NULL DEFAULT gen_random_uuid()"));
        assert!(plan.main_table.contains("deleted_at TIMESTAMPTZ"));
        assert!(plan.main_table.contains("version INTEGER NOT NULL DEFAULT 1"));
        assert!(plan.main_table.contains("CONSTRAINT purchase_order_v1_pkey PRIMARY KEY (id)"));
    }

    #[test]
    fn user_fields_appended_with_correct_pg_types() {
        let f1 = FieldSpec { required: true, ..field("po_number", FieldKind::String) };
        let f2 = field("amount", FieldKind::Number);
        let f3 = field("delivered", FieldKind::Boolean);
        let f4 = FieldSpec {
            enum_values: vec!["draft".into(), "approved".into()],
            ..field("status", FieldKind::Enum)
        };
        let plan =
            build_ddl(&minimal_spec(vec![f1, f2, f3, f4], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.main_table.contains("po_number TEXT NOT NULL"));
        assert!(plan.main_table.contains("amount NUMERIC(19,4)"));
        assert!(plan.main_table.contains("delivered BOOLEAN"));
        assert!(plan.main_table.contains("status TEXT"));
        assert!(plan.main_table.contains("status IN ('draft', 'approved')"));
    }

    #[test]
    fn reserved_column_name_rejected() {
        let spec = minimal_spec(vec![field("created_at", FieldKind::String)], SearchTier::Tier1);
        let err = build_ddl(&spec, &path()).unwrap_err();
        assert!(matches!(err, DdlError::ReservedFieldName(_)));
    }

    #[test]
    fn enum_without_values_rejected() {
        let spec = minimal_spec(vec![field("status", FieldKind::Enum)], SearchTier::Tier1);
        let err = build_ddl(&spec, &path()).unwrap_err();
        assert!(matches!(err, DdlError::EmptyEnum(_)));
    }

    #[test]
    fn ref_without_target_rejected() {
        let spec = minimal_spec(vec![field("supplier", FieldKind::Ref)], SearchTier::Tier1);
        let err = build_ddl(&spec, &path()).unwrap_err();
        assert!(matches!(err, DdlError::RefMissingTarget(_)));
    }

    #[test]
    fn unique_field_gets_partial_index() {
        let f = FieldSpec { unique: true, ..field("po_number", FieldKind::String) };
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        let idx = plan.indexes.iter().find(|s| s.contains("po_number_active")).unwrap();
        assert!(idx.contains("UNIQUE"));
        assert!(idx.contains("WHERE deleted_at IS NULL"));
    }

    #[test]
    fn json_field_gets_gin_index() {
        let f = FieldSpec { indexed: true, ..field("metadata", FieldKind::Json) };
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.indexes.iter().any(|s| s.contains("USING GIN (metadata)")));
    }

    #[test]
    fn soft_delete_index_always_emitted() {
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.indexes.iter().any(|s| s.contains("idx_purchase_order_v1_active")));
    }

    #[test]
    fn tier1_omits_fts_column() {
        let mut f = field("description", FieldKind::String);
        f.searchable = true;
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        assert!(
            !plan.main_table.contains("__fts"),
            "Tier-1 must not provision the FTS column"
        );
        assert!(!plan.indexes.iter().any(|s| s.contains("__fts")));
    }

    #[test]
    fn tier2_emits_fts_column_and_gin_index() {
        let mut a = field("title", FieldKind::String);
        a.searchable = true;
        let mut b = field("notes", FieldKind::String);
        b.searchable = true;
        let plain = field("po_number", FieldKind::String);
        let plan =
            build_ddl(&minimal_spec(vec![a, b, plain], SearchTier::Tier2), &path()).unwrap();
        assert!(plan.main_table.contains("__fts tsvector GENERATED ALWAYS"));
        // Both searchable fields are wired in; the non-searchable one isn't.
        assert!(plan.main_table.contains("coalesce(title"));
        assert!(plan.main_table.contains("coalesce(notes"));
        assert!(!plan.main_table.contains("coalesce(po_number"));
        // GIN index emitted.
        assert!(plan
            .indexes
            .iter()
            .any(|s| s.contains("USING GIN (__fts)")));
        // Phase 5d — default weight (D) emitted for every searchable field.
        // setweight() | setweight() composition replaces the Phase-5b
        // flat to_tsvector(... || ' ' || ...) form.
        assert!(plan.main_table.contains("setweight(to_tsvector('english', coalesce(title, '')), 'D')"));
        assert!(plan.main_table.contains("setweight(to_tsvector('english', coalesce(notes, '')), 'D')"));
        assert!(plan.fts_expression.is_some());
    }

    #[test]
    fn tier2_per_field_weights_emit_setweight_with_correct_class() {
        let mut title = field("title", FieldKind::String);
        title.searchable = true;
        title.fts_weight =
            Some(velocity_types::crds::schema::FtsWeight::A);
        let mut body = field("body", FieldKind::String);
        body.searchable = true;
        body.fts_weight =
            Some(velocity_types::crds::schema::FtsWeight::C);
        let plan =
            build_ddl(&minimal_spec(vec![title, body], SearchTier::Tier2), &path()).unwrap();
        // Title gets A, body gets C — order preserved from spec.fields[].
        let expr = plan.fts_expression.as_deref().unwrap();
        assert!(expr.contains("setweight(to_tsvector('english', coalesce(title, '')), 'A')"));
        assert!(expr.contains("setweight(to_tsvector('english', coalesce(body, '')), 'C')"));
        // Concatenation order: title first.
        let title_pos = expr.find("coalesce(title").unwrap();
        let body_pos = expr.find("coalesce(body").unwrap();
        assert!(title_pos < body_pos);
    }

    #[test]
    fn fts_weight_default_when_absent_is_d() {
        // A field declared `searchable: true` with no `ftsWeight` falls
        // back to D. Confirms that an existing Phase-5b CRD (no weight
        // knob) doesn't ship a different ranking just because the
        // operator binary upgraded.
        let mut f = field("desc", FieldKind::String);
        f.searchable = true;
        // f.fts_weight stays None.
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier2), &path()).unwrap();
        let expr = plan.fts_expression.as_deref().unwrap();
        assert!(expr.contains(", 'D')"));
    }

    #[test]
    fn tier2_no_searchable_fields_skips_fts() {
        let plan = build_ddl(
            &minimal_spec(vec![field("po_number", FieldKind::String)], SearchTier::Tier2),
            &path(),
        )
        .unwrap();
        assert!(!plan.main_table.contains("__fts"));
        assert!(!plan.indexes.iter().any(|s| s.contains("__fts")));
    }

    #[test]
    fn tier1_omits_outbox_tier3_includes_it() {
        let p1 = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        assert!(p1.outbox_table.is_none());
        let p3 = build_ddl(&minimal_spec(vec![], SearchTier::Tier3), &path()).unwrap();
        let outbox = p3.outbox_table.unwrap();
        assert!(outbox.contains("purchase_order_v1_outbox"));
        assert!(outbox.contains("published_at"));
    }

    #[test]
    fn history_table_always_emitted() {
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.history_table.contains("purchase_order_v1_history"));
        assert!(plan.history_table.contains("snapshot     JSONB"));
    }

    #[test]
    fn triggers_include_touch_and_audit() {
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.triggers.iter().any(|s| s.contains("_touch()")));
        assert!(plan.triggers.iter().any(|s| s.contains("_audit()")));
        // Tier-1 audit fn should NOT write to outbox.
        let audit = plan.triggers.iter().find(|s| s.contains("_audit()")).unwrap();
        assert!(!audit.contains("_outbox"));
    }

    #[test]
    fn tier3_audit_trigger_writes_to_outbox() {
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier3), &path()).unwrap();
        let audit = plan.triggers.iter().find(|s| s.contains("_audit()")).unwrap();
        assert!(
            audit.contains("INSERT INTO acme_supply_chain_procurement.purchase_order_v1_outbox")
        );
    }

    #[test]
    fn default_value_for_string_quoted() {
        let f = FieldSpec { default: Some(json!("draft")), ..field("status", FieldKind::String) };
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.main_table.contains("DEFAULT 'draft'"));
    }

    #[test]
    fn default_value_sql_injection_safe() {
        // Spec authors may write malicious defaults; we must escape.
        let f = FieldSpec {
            default: Some(json!("a'); DROP TABLE x;--")),
            ..field("status", FieldKind::String)
        };
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        // Single quotes inside the literal must be doubled.
        assert!(plan.main_table.contains("DEFAULT 'a''); DROP TABLE x;--'"));
        assert!(!plan.main_table.contains("'a'); DROP"));
    }

    #[test]
    fn unsupported_default_rejected() {
        let f = FieldSpec {
            default: Some(json!({"nested": true})),
            ..field("flag", FieldKind::Boolean)
        };
        let err = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap_err();
        assert!(matches!(err, DdlError::UnsupportedDefault { .. }));
    }

    #[test]
    fn string_with_max_length_uses_varchar() {
        let f = FieldSpec { max_length: Some(64), ..field("code", FieldKind::String) };
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.main_table.contains("code VARCHAR(64)"));
    }

    fn spec_with_row_filter(
        fields: Vec<FieldSpec>,
        row_filter: Vec<velocity_types::crds::schema::RowFilterRule>,
    ) -> SchemaDefinitionSpec {
        let mut s = minimal_spec(fields, SearchTier::Tier1);
        s.access.row_filter = row_filter;
        s
    }

    fn rfrule(
        role: &str,
        field: &str,
        op: &str,
        value: serde_json::Value,
    ) -> velocity_types::crds::schema::RowFilterRule {
        velocity_types::crds::schema::RowFilterRule {
            role: role.into(),
            filter: velocity_types::crds::schema::RowFilter {
                field: field.into(),
                op: op.into(),
                value,
            },
        }
    }

    #[test]
    fn rls_always_enabled_even_without_row_filter() {
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        assert!(plan.rls_policies.iter().any(|s| s.contains("ENABLE ROW LEVEL SECURITY")));
        // Wildcard policy is always emitted so the schema admits traffic
        // when no rowFilter is declared.
        assert!(plan.rls_policies.iter().any(|s| s.contains("pol_purchase_order_v1_unrestricted")));
    }

    #[test]
    fn rls_wildcard_admits_only_star_sentinel() {
        // Pinpoints the exact encoding contract with the API: `*` is the
        // ONLY admit sentinel. `''` is the deny sentinel (compiled rules
        // + zero matched user-roles) — the SQL-fragment path renders that
        // case to `(false)`, so the RLS path MUST NOT admit on `''`
        // either, or defense-in-depth diverges. NULL also fails closed.
        let plan = build_ddl(&minimal_spec(vec![], SearchTier::Tier1), &path()).unwrap();
        let wild = plan
            .rls_policies
            .iter()
            .find(|s| {
                s.starts_with("CREATE POLICY")
                    && s.contains("pol_purchase_order_v1_unrestricted")
            })
            .unwrap();
        assert!(wild.contains("= '*'"));
        assert!(!wild.contains("= ''"), "wildcard policy must not admit empty-string sentinel");
        assert!(wild.contains("AS PERMISSIVE FOR ALL"));
        // No bare reference to current_user / current_role — the encoding
        // is `app.scoped_roles`, nothing else.
        assert!(!wild.contains("current_user"));
    }

    #[test]
    fn rls_per_role_policy_ands_within_role() {
        // Two `west` rules must AND together in one policy — emitting
        // two separate policies would OR them under Postgres' permissive
        // policy semantics and *widen* the access vs the SQL fragment.
        let plan = build_ddl(
            &spec_with_row_filter(
                vec![
                    {
                        let mut f = field("region", FieldKind::String);
                        f.filterable = true;
                        f
                    },
                    {
                        let mut f = field("status", FieldKind::String);
                        f.filterable = true;
                        f
                    },
                ],
                vec![
                    rfrule("west", "region", "eq", json!("west")),
                    rfrule("west", "status", "neq", json!("archived")),
                ],
            ),
            &path(),
        )
        .unwrap();
        let west = plan
            .rls_policies
            .iter()
            .find(|s| {
                s.starts_with("CREATE POLICY")
                    && s.contains("pol_purchase_order_v1_role_west_")
            })
            .unwrap();
        assert!(west.contains("region = 'west'"));
        assert!(west.contains("status <> 'archived'"));
        assert!(west.contains(" AND "));
        // The role-literal must appear once (membership check).
        assert_eq!(west.matches("'west'").count(), 2); // membership + value
    }

    #[test]
    fn rls_different_roles_get_separate_policies() {
        // Two distinct user-roles → two distinct policies, which
        // Postgres ORs together for free. The OR semantic is the same
        // as the SQL-fragment path.
        let plan = build_ddl(
            &spec_with_row_filter(
                vec![{
                    let mut f = field("region", FieldKind::String);
                    f.filterable = true;
                    f
                }],
                vec![
                    rfrule("west", "region", "eq", json!("west")),
                    rfrule("east", "region", "eq", json!("east")),
                ],
            ),
            &path(),
        )
        .unwrap();
        assert!(plan
            .rls_policies
            .iter()
            .any(|s| s.contains("pol_purchase_order_v1_role_west_")));
        assert!(plan
            .rls_policies
            .iter()
            .any(|s| s.contains("pol_purchase_order_v1_role_east_")));
    }

    #[test]
    fn rls_drops_policy_before_creating_it() {
        // Reconciles after a rowFilter edit must replace the predicate
        // cleanly. Idempotency depends on DROP IF EXISTS gating the
        // CREATE in the same transaction.
        let plan = build_ddl(
            &spec_with_row_filter(
                vec![{
                    let mut f = field("region", FieldKind::String);
                    f.filterable = true;
                    f
                }],
                vec![rfrule("west", "region", "eq", json!("west"))],
            ),
            &path(),
        )
        .unwrap();
        let drop_idx = plan
            .rls_policies
            .iter()
            .position(|s| s.contains("DROP POLICY IF EXISTS pol_purchase_order_v1_unrestricted"))
            .unwrap();
        let create_idx = plan
            .rls_policies
            .iter()
            .position(|s| {
                s.starts_with("CREATE POLICY pol_purchase_order_v1_unrestricted")
            })
            .unwrap();
        assert!(drop_idx < create_idx, "DROP must precede CREATE in the same plan");
    }

    #[test]
    fn rls_dash_in_role_does_not_break_identifier() {
        // Roles like `regional-reader-west` aren't valid Postgres
        // identifiers; sanitisation + hash suffix keeps the policy name
        // legal AND unique.
        let plan = build_ddl(
            &spec_with_row_filter(
                vec![{
                    let mut f = field("region", FieldKind::String);
                    f.filterable = true;
                    f
                }],
                vec![rfrule("regional-reader-west", "region", "eq", json!("west"))],
            ),
            &path(),
        )
        .unwrap();
        let policy = plan
            .rls_policies
            .iter()
            .find(|s| s.starts_with("CREATE POLICY") && s.contains("regional_reader_west"))
            .unwrap();
        // No dashes in the identifier.
        let policy_name = policy.split(" ON ").next().unwrap();
        assert!(
            !policy_name.contains('-'),
            "policy identifier must be sanitised: {policy_name}"
        );
        // The role literal in the membership check keeps the original form.
        assert!(policy.contains("'regional-reader-west' = ANY"));
    }

    #[test]
    fn rls_unsupported_op_rejected() {
        let err = build_ddl(
            &spec_with_row_filter(
                vec![{
                    let mut f = field("region", FieldKind::String);
                    f.filterable = true;
                    f
                }],
                vec![rfrule("west", "region", "between", json!("west"))],
            ),
            &path(),
        )
        .unwrap_err();
        assert!(matches!(err, DdlError::UnsupportedDefault { .. }));
    }

    #[test]
    fn rls_non_scalar_value_rejected() {
        // A `value: { … }` would have to be inlined verbatim into DDL —
        // refuse rather than emit an injection vector.
        let err = build_ddl(
            &spec_with_row_filter(
                vec![{
                    let mut f = field("region", FieldKind::String);
                    f.filterable = true;
                    f
                }],
                vec![rfrule("west", "region", "eq", json!({ "complex": true }))],
            ),
            &path(),
        )
        .unwrap_err();
        assert!(matches!(err, DdlError::UnsupportedDefault { .. }));
    }

    #[test]
    fn rls_value_literal_is_escaped() {
        // Same SQL-injection escape the rest of the builder relies on:
        // a CRD author can't break out of the literal with a single quote.
        let plan = build_ddl(
            &spec_with_row_filter(
                vec![{
                    let mut f = field("region", FieldKind::String);
                    f.filterable = true;
                    f
                }],
                vec![rfrule("west", "region", "eq", json!("west'); DROP TABLE x;--"))],
            ),
            &path(),
        )
        .unwrap();
        let policy = plan
            .rls_policies
            .iter()
            .find(|s| {
                s.starts_with("CREATE POLICY")
                    && s.contains("pol_purchase_order_v1_role_west_")
            })
            .unwrap();
        assert!(policy.contains("'west''); DROP TABLE x;--'"));
        assert!(!policy.contains("'west'); DROP"));
    }

    #[test]
    fn duplicate_indexes_dedup() {
        let f = FieldSpec {
            unique: true,
            indexed: true,
            filterable: true,
            ..field("po_number", FieldKind::String)
        };
        let plan = build_ddl(&minimal_spec(vec![f], SearchTier::Tier1), &path()).unwrap();
        let active_idx_count = plan
            .indexes
            .iter()
            .filter(|s| s.contains("idx_purchase_order_v1_po_number_active"))
            .count();
        // exactly one partial unique
        assert_eq!(active_idx_count, 1);
    }
}
