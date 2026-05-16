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
use velocity_types::crds::schema::{FieldKind, FieldSpec, SchemaDefinitionSpec, SearchTier};

use crate::provisioner::{validate_ident, ProvisionError};

/// Auto-provisioned columns (design §3.2). Order matters for the generated
/// `CREATE TABLE` to read top-down.
const SYSTEM_COLUMNS: &[(&str, &str)] = &[
    ("id", "UUID NOT NULL DEFAULT gen_random_uuid()"),
    ("created_at", "TIMESTAMPTZ NOT NULL DEFAULT now()"),
    ("updated_at", "TIMESTAMPTZ NOT NULL DEFAULT now()"),
    ("deleted_at", "TIMESTAMPTZ"),
    ("version", "INTEGER NOT NULL DEFAULT 1"),
    ("created_by", "TEXT NOT NULL"),
    ("updated_by", "TEXT NOT NULL"),
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
    /// `CREATE TABLE {schema}.{object}_{version}_history ( ... )`
    pub history_table: String,
    /// Tier-3 only — outbox table for CDC (ADR-002).
    pub outbox_table: Option<String>,
    /// `CREATE INDEX ...` — main table, in stable order.
    pub indexes: Vec<String>,
    /// PL/pgSQL functions + triggers (updated_at touch, history+outbox).
    pub triggers: Vec<String>,
}

/// Build a complete [`DdlPlan`] for a `SchemaDefinition`.
pub fn build_ddl(spec: &SchemaDefinitionSpec, path: &SchemaPath) -> Result<DdlPlan, DdlError> {
    let schema_name =
        validate_ident(&sanitize(&format!("{}_{}_{}", path.org, path.app, path.domain)))?;
    let version_sfx = validate_ident(&sanitize(&path.version))?;
    let table = validate_ident(&format!("{}_{}", sanitize_object(&path.object)?, version_sfx))?;
    let qualified = format!("{schema_name}.{table}");

    let columns = build_columns(spec, &table)?;
    let main_table = build_create_table(&qualified, &columns, &table)?;
    let history_table = build_history_table(&schema_name, &table)?;
    let outbox_table = match spec.search.tier {
        SearchTier::Tier3 => Some(build_outbox_table(&schema_name, &table)),
        _ => None,
    };
    let indexes = build_indexes(spec, &schema_name, &table)?;
    let triggers = build_triggers(spec, &schema_name, &table);

    Ok(DdlPlan {
        qualified_table: qualified,
        main_table,
        history_table,
        outbox_table,
        indexes,
        triggers,
    })
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

fn build_create_table(
    qualified: &str,
    columns: &[ColumnDef],
    table: &str,
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
            default: None,
            min: None,
            max: None,
            max_length: None,
            pattern: None,
            enum_values: Vec::new(),
            r#ref: None,
            sensitivity: None,
            access: None,
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
