//! `ResolvedSchema` — the runtime view of a `SchemaDefinition` after the
//! operator merges in inherited `Organisation`/`Application`/`Domain` policies.
//!
//! This is the value the `SchemaRegistry` actually serves on the hot path.
//! See `docs/design.md §4` and `CLAUDE.md › SchemaRegistry Implementation`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::common::SchemaPath;
use crate::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, PartitioningSpec, ScalingSpec, SearchSpec,
    ValidationRule,
};

/// Where a schema sits in its life cycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Draft,
    #[default]
    Stable,
    Deprecated,
    Sunset,
}

/// Merged, runtime-ready schema. Built by the operator after combining the
/// `SchemaDefinition` spec with policies inherited from the parent
/// `Domain`/`Application`/`Organisation`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedSchema {
    pub path: SchemaPath,
    pub uid: String,
    pub lifecycle: Lifecycle,
    pub fields: Vec<FieldSpec>,
    pub validations: Vec<ValidationRule>,
    pub auth: AuthSpec,
    pub access: AccessSpec,
    pub search: SearchSpec,
    pub partitioning: Option<PartitioningSpec>,
    pub observability: ObservabilitySpec,
    pub scaling: Option<ScalingSpec>,
    /// Hash of (spec, effective policy) — reconcile-skip key.
    pub policy_hash: String,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

impl ResolvedSchema {
    /// Postgres table this schema writes to.
    pub fn pg_qualified_table(&self) -> String {
        self.path.pg_qualified_table()
    }

    /// Whether this schema participates in the outbox CDC pipeline.
    pub fn requires_outbox(&self) -> bool {
        self.search.tier == 3
    }
}
