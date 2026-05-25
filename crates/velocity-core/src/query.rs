//! Minimal LIST query builder.
//!
//! Phase 1 supports a single shape: `SELECT * FROM <table> WHERE deleted_at
//! IS NULL [AND <filter>...] [ORDER BY <sort>...] LIMIT <n>`. Every
//! WHERE/ORDER reference is gated on `ResolvedSchema.fields` so the only
//! identifiers that ever reach the SQL string are user-declared field names
//! the operator already sanitised. Values are always bound — `$1`, `$2`, …
//!
//! Cursor pagination (ADR-009) is a stub here; the cursor parameter is
//! accepted but a real keyset implementation lands with the QueryBuilder
//! rewrite in Phase 2. We do enforce the limit cap of 1000.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiError;
use crate::identity::Identity;
use crate::registry::ResolvedSchema;

/// Hard upper bound on a LIST page size. ADR-009 — anything past this needs
/// a cursor.
pub const MAX_PAGE_SIZE: u32 = 1000;
pub const DEFAULT_PAGE_SIZE: u32 = 50;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListQuery {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
    #[serde(default)]
    pub sort: Vec<SortField>,
    #[serde(default)]
    pub filter: Vec<FilterField>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SortField {
    pub field: String,
    #[serde(default)]
    pub desc: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilterField {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl FilterOp {
    pub fn sql(self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::Neq => "<>",
            FilterOp::Lt => "<",
            FilterOp::Lte => "<=",
            FilterOp::Gt => ">",
            FilterOp::Gte => ">=",
        }
    }

    /// Parse the operator string used in CRDs (`spec.access.rowFilter[].filter.op`).
    /// Accepts both the canonical lower-snake form (`eq`, `neq`, `gte`) and
    /// the SQL symbols (`=`, `<>`, `>=`) so authors can write either.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "eq" | "=" | "==" => FilterOp::Eq,
            "neq" | "ne" | "<>" | "!=" => FilterOp::Neq,
            "lt" | "<" => FilterOp::Lt,
            "lte" | "le" | "<=" => FilterOp::Lte,
            "gt" | ">" => FilterOp::Gt,
            "gte" | "ge" | ">=" => FilterOp::Gte,
            _ => return None,
        })
    }
}

/// Compiled SQL + bound JSON values, in $N order.
#[derive(Debug)]
pub struct CompiledList {
    pub sql: String,
    pub params: Vec<Value>,
}

pub fn build_list(
    schema: &ResolvedSchema,
    q: &ListQuery,
    identity: &Identity,
) -> Result<CompiledList, ApiError> {
    let mut params: Vec<Value> = Vec::new();
    let mut sql = format!("SELECT * FROM {} t WHERE deleted_at IS NULL", schema.pg_qualified);

    for f in &q.filter {
        if !schema.fields.by_name.contains_key(&f.field) {
            return Err(ApiError::UnknownField(f.field.clone()));
        }
        if !schema.fields.filterable.contains(&f.field) {
            return Err(ApiError::NotFilterable(f.field.clone()));
        }
        params.push(f.value.clone());
        sql.push_str(&format!(" AND {} {} ${}", f.field, f.op.sql(), params.len()));
    }

    // PHASE 5 — Layer-3 cross-schema RBAC hook lives here.
    // When the query engine learns to parse `include[]` / `join` (Phase 5),
    // every referenced target schema MUST be access-checked against
    // `identity` (default deny) BEFORE any join SQL is emitted. The check
    // belongs at this position — after the local schema's WHERE is built
    // but before the JOIN compilation — so a missing read on the target
    // surfaces as 403, not as 500 from a half-built query. See Phase 2b
    // Layer-3 note in docs/phases.md.

    // Layer-4 row filter — AND'd in after the user's WHERE so an actor
    // can never widen their scope with their own filter clauses. A broken
    // index pages an operator via 500 rather than silently admitting.
    match schema.row_filter.predicate(&identity.roles, params.len() + 1) {
        Ok(Some(pred)) => {
            sql.push_str(" AND ");
            sql.push_str(&pred.sql);
            params.extend(pred.params);
        }
        Ok(None) => {}
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "rowFilter broken on role `{}`: {}",
                e.role, e.reason
            )));
        }
    }

    if !q.sort.is_empty() {
        sql.push_str(" ORDER BY ");
        let mut first = true;
        for s in &q.sort {
            if !schema.fields.by_name.contains_key(&s.field) {
                return Err(ApiError::UnknownField(s.field.clone()));
            }
            if !schema.fields.sortable.contains(&s.field) {
                return Err(ApiError::NotSortable(s.field.clone()));
            }
            if !first {
                sql.push_str(", ");
            }
            first = false;
            sql.push_str(&s.field);
            sql.push_str(if s.desc { " DESC" } else { " ASC" });
        }
    } else {
        sql.push_str(" ORDER BY created_at DESC, id DESC");
    }

    let limit = q.limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    sql.push_str(&format!(" LIMIT {limit}"));

    Ok(CompiledList { sql, params })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use serde_json::json;
    use velocity_types::common::SchemaPath;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
        SearchSpec, SearchTier,
    };

    fn anon() -> Identity {
        Identity::anonymous()
    }

    fn field(name: &str, filterable: bool, sortable: bool) -> FieldSpec {
        let mut f: FieldSpec =
            serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
        f.kind = FieldKind::String;
        f.filterable = filterable;
        f.sortable = sortable;
        f
    }

    fn schema(fields: Vec<FieldSpec>) -> ResolvedSchema {
        let spec = SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "acme-platform".into(),
                },
                overrides: Vec::new(),
            },
            access: AccessSpec::default(),
            fields,
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        };
        ResolvedSchema::from_spec(
            SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1"),
            spec,
        )
    }

    #[test]
    fn empty_query_emits_default_ordering_and_limit() {
        let s = schema(vec![field("po_number", true, true)]);
        let c = build_list(&s, &ListQuery::default(), &anon()).unwrap();
        assert!(c.sql.contains("WHERE deleted_at IS NULL"));
        assert!(c.sql.contains("ORDER BY created_at DESC, id DESC"));
        assert!(c.sql.contains("LIMIT 50"));
        assert!(c.params.is_empty());
    }

    #[test]
    fn filter_on_unknown_field_rejected() {
        let s = schema(vec![field("po_number", true, true)]);
        let q = ListQuery {
            filter: vec![FilterField {
                field: "ghost".into(),
                op: FilterOp::Eq,
                value: json!("x"),
            }],
            ..Default::default()
        };
        let err = build_list(&s, &q, &anon()).unwrap_err();
        assert!(matches!(err, ApiError::UnknownField(_)));
    }

    #[test]
    fn filter_on_non_filterable_rejected() {
        let s = schema(vec![field("notes", false, false)]);
        let q = ListQuery {
            filter: vec![FilterField {
                field: "notes".into(),
                op: FilterOp::Eq,
                value: json!("x"),
            }],
            ..Default::default()
        };
        let err = build_list(&s, &q, &anon()).unwrap_err();
        assert!(matches!(err, ApiError::NotFilterable(_)));
    }

    #[test]
    fn sort_on_non_sortable_rejected() {
        let s = schema(vec![field("notes", true, false)]);
        let q = ListQuery {
            sort: vec![SortField { field: "notes".into(), desc: true }],
            ..Default::default()
        };
        let err = build_list(&s, &q, &anon()).unwrap_err();
        assert!(matches!(err, ApiError::NotSortable(_)));
    }

    #[test]
    fn limit_is_capped() {
        let s = schema(vec![]);
        let q = ListQuery { limit: Some(100_000), ..Default::default() };
        let c = build_list(&s, &q, &anon()).unwrap();
        assert!(c.sql.contains("LIMIT 1000"));
    }

    #[test]
    fn filters_bind_values_in_order() {
        let s = schema(vec![field("status", true, false), field("supplier_code", true, false)]);
        let q = ListQuery {
            filter: vec![
                FilterField { field: "status".into(), op: FilterOp::Eq, value: json!("approved") },
                FilterField {
                    field: "supplier_code".into(),
                    op: FilterOp::Neq,
                    value: json!("TATA001"),
                },
            ],
            ..Default::default()
        };
        let c = build_list(&s, &q, &anon()).unwrap();
        assert!(c.sql.contains("status = $1"));
        assert!(c.sql.contains("supplier_code <> $2"));
        assert_eq!(c.params.len(), 2);
    }
}
