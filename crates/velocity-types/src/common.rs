//! Cross-CRD types: paths, references, naming helpers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Fully-qualified path to a versioned schema: `{org}/{app}/{domain}/{object}/{version}`.
///
/// The five segments map 1:1 to the Postgres schema/table conventions in
/// `docs/design.md §3.1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct SchemaPath {
    pub org: String,
    pub app: String,
    pub domain: String,
    pub object: String,
    pub version: String,
}

impl SchemaPath {
    pub fn new(
        org: impl Into<String>,
        app: impl Into<String>,
        domain: impl Into<String>,
        object: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            org: org.into(),
            app: app.into(),
            domain: domain.into(),
            object: object.into(),
            version: version.into(),
        }
    }

    /// Postgres schema name: `{org}_{app}_{domain}` (sanitized).
    pub fn pg_schema(&self) -> String {
        format!("{}_{}_{}", sanitize(&self.org), sanitize(&self.app), sanitize(&self.domain))
    }

    /// Postgres archive schema name: `{org}_{app}_{domain}_archive`.
    pub fn pg_archive_schema(&self) -> String {
        format!("{}_archive", self.pg_schema())
    }

    /// Postgres table name: `{object}_{version}` (sanitized).
    pub fn pg_table(&self) -> String {
        format!("{}_{}", sanitize(&self.object), sanitize(&self.version))
    }

    /// Fully-qualified table: `{pg_schema}.{pg_table}`.
    pub fn pg_qualified_table(&self) -> String {
        format!("{}.{}", self.pg_schema(), self.pg_table())
    }

    /// History table name.
    pub fn pg_history_table(&self) -> String {
        format!("{}_history", self.pg_table())
    }

    /// Outbox table name (Tier-3 search schemas).
    pub fn pg_outbox_table(&self) -> String {
        format!("{}_outbox", self.pg_table())
    }

    /// Kubernetes namespace for this schema: `{org}-{app}-{domain}` (kebab-case).
    pub fn k8s_namespace(&self) -> String {
        format!("{}-{}-{}", kebab(&self.org), kebab(&self.app), kebab(&self.domain),)
    }
}

impl std::fmt::Display for SchemaPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}/{}/{}", self.org, self.app, self.domain, self.object, self.version)
    }
}

/// Lossless reference to another schema in any domain.
///
/// Used for `ref` fields and `auth.strategyRef` style cross-CRD pointers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ObjectRef {
    pub org: String,
    pub app: String,
    pub domain: String,
    pub object: String,
    pub version: String,
    /// Field on the target schema this reference resolves by (default: `id`).
    #[serde(default = "default_ref_key", skip_serializing_if = "is_default_ref_key")]
    pub key: String,
}

fn default_ref_key() -> String {
    "id".to_string()
}
fn is_default_ref_key(s: &str) -> bool {
    s == "id"
}

/// Lightweight pointer to another CRD by name + namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct NamespacedRef {
    pub name: String,
    pub namespace: String,
}

/// Lowercase + replace `[- . space]` with `_` — Postgres-identifier-safe.
pub fn sanitize(s: &str) -> String {
    s.to_lowercase().replace(['-', '.', ' ', '/'], "_")
}

/// Lowercase + replace `[_ . space /]` with `-` — k8s-name-safe.
pub fn kebab(s: &str) -> String {
    s.to_lowercase().replace(['_', '.', ' ', '/'], "-")
}

/// JSON Schema helper that emits `x-kubernetes-preserve-unknown-fields: true`,
/// which the Kubernetes apiserver requires for fields whose schema is "any
/// JSON value." Use as `#[schemars(schema_with = "preserve_unknown_fields")]`
/// on a `serde_json::Value` or `BTreeMap<String, Value>` field — without this,
/// kube's CRD generator emits a properties block without a parent `type`,
/// which the apiserver rejects with `must not be empty for specified object fields`.
///
/// schemars 1.x dropped the `SchemaObject`/`extensions` API in favour of a
/// single `Schema` that wraps a `serde_json::Value`; we build the same
/// extension directly via the `json_schema!` macro.
pub fn preserve_unknown_fields(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "x-kubernetes-preserve-unknown-fields": true
    })
}

/// Errors raised by parsers in this module.
#[derive(Debug, Error)]
pub enum CommonError {
    #[error("invalid schema path: expected `org/app/domain/object/version`, got `{0}`")]
    InvalidPath(String),
}

impl std::str::FromStr for SchemaPath {
    type Err = CommonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 5 || parts.iter().any(|p| p.is_empty()) {
            return Err(CommonError::InvalidPath(s.to_string()));
        }
        Ok(Self::new(parts[0], parts[1], parts[2], parts[3], parts[4]))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn pg_schema_and_table_snake_case() {
        let p = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v2");
        assert_eq!(p.pg_schema(), "acme_supply_chain_procurement");
        assert_eq!(p.pg_table(), "purchase_order_v2");
        assert_eq!(p.pg_qualified_table(), "acme_supply_chain_procurement.purchase_order_v2");
        assert_eq!(p.pg_archive_schema(), "acme_supply_chain_procurement_archive");
        assert_eq!(p.pg_history_table(), "purchase_order_v2_history");
        assert_eq!(p.pg_outbox_table(), "purchase_order_v2_outbox");
    }

    #[test]
    fn k8s_namespace_kebab_case() {
        let p = SchemaPath::new("Acme", "Supply_Chain", "procurement", "x", "v1");
        assert_eq!(p.k8s_namespace(), "acme-supply-chain-procurement");
    }

    #[test]
    fn sanitize_handles_separators() {
        assert_eq!(sanitize("Foo-Bar.Baz Qux/x"), "foo_bar_baz_qux_x");
        assert_eq!(kebab("Foo_Bar.Baz Qux/x"), "foo-bar-baz-qux-x");
    }

    #[test]
    fn display_round_trips_through_fromstr() {
        let p = SchemaPath::new("a", "b", "c", "d", "v1");
        let s = p.to_string();
        let p2 = SchemaPath::from_str(&s).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn fromstr_rejects_malformed() {
        assert!(SchemaPath::from_str("too/few/segments").is_err());
        assert!(SchemaPath::from_str("a/b/c/d/").is_err());
        assert!(SchemaPath::from_str("a/b/c/d/v1/extra").is_err());
    }
}
