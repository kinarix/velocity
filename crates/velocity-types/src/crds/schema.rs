//! `SchemaDefinition` — the heart of Velocity. See `docs/design.md §1.4`.
//!
//! Phase 0 lands a substantial-but-not-exhaustive spec. Field-level masking,
//! hooks, time machine storage tiers, and per-operation auth overrides
//! materialise in their respective phases.

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{NamespacedRef, ObjectRef};
use crate::crds::{Condition, ReconcilePhase};

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "SchemaDefinition",
    namespaced,
    status = "SchemaDefinitionStatus",
    shortname = "sd"
)]
#[serde(rename_all = "camelCase")]
pub struct SchemaDefinitionSpec {
    pub version: String,

    #[serde(default)]
    pub partitioning: Option<PartitioningSpec>,

    pub auth: AuthSpec,
    pub access: AccessSpec,

    pub fields: Vec<FieldSpec>,

    #[serde(default)]
    pub validations: Vec<ValidationRule>,

    pub search: SearchSpec,

    #[serde(default)]
    pub time_machine: Option<TimeMachineSpec>,

    #[serde(default)]
    pub audit: Option<AuditSpec>,

    #[serde(default)]
    pub archive: Option<ArchiveRef>,

    #[serde(default)]
    pub observability: ObservabilitySpec,

    #[serde(default)]
    pub scaling: Option<ScalingSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaDefinitionStatus {
    pub phase: Option<ReconcilePhase>,
    pub provisioned_at: Option<String>,
    pub pg_table: Option<String>,
    pub policy_hash: Option<String>,
    pub records: Option<u64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Phase 5d-3b: in-flight or last-completed blue-green Typesense
    /// rebuild. Only populated for Tier-3 schemas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_rebuild: Option<SearchRebuildStatus>,
    /// Phase 5d-3c: the Typesense concrete collection name that the
    /// schema's alias currently points at. Surfaces in
    /// `kubectl describe sd` so an SRE can answer "which underlying
    /// collection is serving search right now" without an out-of-band
    /// `GET /aliases` call. Only populated for Tier-3 schemas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchRebuildStatus {
    /// `<alias>__<hash>` Typesense concrete collection currently
    /// receiving backfill traffic.
    pub target_concrete: String,
    /// `<alias>__<hash>` Typesense concrete collection that the alias
    /// is *currently* pointing at — i.e. the live one search reads
    /// from until the swap.
    pub source_concrete: String,
    /// RFC 3339 timestamp when the rebuild started.
    pub started_at: String,
    /// RFC 3339 timestamp when the rebuild ended (success or
    /// abandoned). `None` while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Rows copied from Postgres to the target concrete collection
    /// (best-effort; updated periodically).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_processed: Option<u64>,
    /// Last delta-pass timestamp; used by the operator to scope the
    /// next `WHERE updated_at >= …` query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delta_at: Option<String>,
    /// Set when the rebuild ended with an error. Plain string so
    /// `kubectl describe sd` shows it without JSON noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ─── Partitioning ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PartitioningSpec {
    pub enabled: bool,
    pub strategy: PartitionStrategy,
    pub field: String,
    pub interval: PartitionInterval,
    /// e.g. `"7years"` — auto-drop older partitions.
    #[serde(default)]
    pub retention: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PartitionInterval {
    Monthly,
    Quarterly,
    Yearly,
}

// ─── Auth (per-schema) ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthSpec {
    pub strategy_ref: NamespacedRef,
    #[serde(default)]
    pub overrides: Vec<AuthOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthOverride {
    pub operations: Vec<String>,
    pub strategy_ref: NamespacedRef,
}

// ─── Access ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessSpec {
    #[serde(default)]
    pub roles: Vec<RoleAccess>,
    #[serde(default)]
    pub row_filter: Vec<RowFilterRule>,
    #[serde(default)]
    pub policies: Vec<AbacPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoleAccess {
    pub role: String,
    pub operations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RowFilterRule {
    pub role: String,
    pub filter: RowFilter,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RowFilter {
    pub field: String,
    pub op: String,
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AbacPolicy {
    pub name: String,
    pub action: String,
    #[serde(default)]
    pub fields: Vec<String>,
    pub condition: String,
    #[serde(default)]
    pub message: Option<String>,
}

// ─── Fields ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FieldSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,

    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub indexed: bool,
    #[serde(default)]
    pub filterable: bool,
    #[serde(default)]
    pub sortable: bool,
    #[serde(default)]
    pub searchable: bool,

    /// Phase 5d — FTS weight class for this field's contribution to the
    /// generated `__fts` tsvector. Only meaningful when `searchable:
    /// true` and `kind` is `String` or `Enum`; the webhook rejects it
    /// otherwise. Defaults to `D` so that a CRD with no `ftsWeight` on
    /// any field reproduces the Phase 5b "uniform weighting" behaviour
    /// — `ts_rank()` collapses to a constant across hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fts_weight: Option<FtsWeight>,

    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub default: Option<serde_json::Value>,

    // Numeric constraints
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,

    // String constraints
    #[serde(default)]
    pub max_length: Option<u32>,
    #[serde(default)]
    pub pattern: Option<String>,

    // Enum values (when kind == Enum)
    #[serde(default)]
    pub enum_values: Vec<String>,

    /// Cross-schema reference (string field acting as FK).
    #[serde(default)]
    pub r#ref: Option<ObjectRef>,

    /// Sensitivity tag — drives masking and log redaction.
    #[serde(default)]
    pub sensitivity: Option<Sensitivity>,

    /// Per-role read/write access on the field.
    #[serde(default)]
    pub access: Option<FieldAccess>,

    /// Layer-6 masking. When set, the field's value is transformed before
    /// being written to the response — unless the caller carries a role in
    /// `unmaskedFor`. Strip (Layer 5) runs first; masking only transforms
    /// values the caller is already entitled to read.
    #[serde(default)]
    pub mask: Option<MaskingSpec>,
}

/// Per-field masking configuration. The discriminator is `strategy`; the
/// meaningful auxiliary fields depend on it (`partial` → `keepLast`).
/// Modelled flat because kube's CRD generator can't express
/// internally-tagged enums.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaskingSpec {
    pub strategy: MaskingStrategyKind,
    /// Required when `strategy = partial`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_last: Option<u32>,
    /// Roles whose holders see the raw value. Empty list ⇒ everyone is
    /// masked. The unmask check is OR across the caller's roles.
    #[serde(default)]
    pub unmasked_for: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MaskingStrategyKind {
    /// Replace the value with an opaque marker.
    Redact,
    /// Keep the trailing N chars verbatim; mask the rest with `*`.
    Partial,
    /// Replace with `sha256:<hex>` so equal values still compare equal.
    Hash,
    // NOTE: a future `Range` variant will bucket the value into a coarse
    // band. It isn't declared here yet — adding it without an
    // implementation makes deserialize succeed on configs the runtime
    // can't honour. A CRD with `strategy: range` should fail to parse
    // until the runtime support lands.
}

/// Postgres FTS weight class for a `searchable` field. Maps to the
/// fourth argument of `setweight(tsvector, 'A'|'B'|'C'|'D')`.
/// Default (`D`) preserves existing Phase 5b ranking — every field
/// equal — so omitting the knob across the board is a no-op.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum FtsWeight {
    A,
    B,
    C,
    #[default]
    #[serde(other)]
    D,
}

impl FtsWeight {
    /// The single-character label Postgres expects in `setweight(..., $)`.
    pub fn as_pg_char(&self) -> char {
        match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    String,
    Integer,
    Number,
    Boolean,
    Date,
    Datetime,
    Uuid,
    Json,
    Enum,
    Ref,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Public,
    Internal,
    Confidential,
    Pii,
    Financial,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FieldAccess {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

// ─── Validations ────────────────────────────────────────────────────────────

/// Validation rule. The discriminator is `type`; the meaningful fields depend
/// on it (`compare` → left/operator/right, `cel` → rule). Modeled as a flat
/// struct because kube's CRD generator can't express internally-tagged enums.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ValidationRule {
    #[serde(rename = "type")]
    pub kind: ValidationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Per-rule timeout cap (CEL only); clamped to AuthStrategy.cel.maxExecutionMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_execution_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ValidationKind {
    Compare,
    Cel,
}

// ─── Search ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchSpec {
    pub tier: SearchTier,
    #[serde(default)]
    pub cross_search: bool,
    #[serde(default)]
    pub cross_search_weight: Option<u8>,
    #[serde(default)]
    pub display: Option<SearchDisplay>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchDisplay {
    pub label: Option<String>,
    pub title_field: Option<String>,
    pub subtitle_field: Option<String>,
    pub url_template: Option<String>,
}

/// 1 = Postgres filters only, 2 = Postgres FTS, 3 = Typesense (CDC via outbox).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[repr(u8)]
pub enum SearchTier {
    #[default]
    Tier1 = 1,
    Tier2 = 2,
    Tier3 = 3,
}

impl PartialEq<u8> for SearchTier {
    fn eq(&self, other: &u8) -> bool {
        (*self as u8) == *other
    }
}

// ─── Time machine / audit / archive / observability / scaling ───────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimeMachineSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub storage: TimeMachineStorage,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimeMachineStorage {
    #[serde(default)]
    pub hot: Option<TierConfig>,
    #[serde(default)]
    pub warm: Option<TierConfig>,
    #[serde(default)]
    pub cold: Option<TierConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TierConfig {
    pub backend: String,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub retention: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub reads: Option<AuditReads>,
    #[serde(default)]
    pub writes: Option<AuditWrites>,
    #[serde(default)]
    pub regulations: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditReads {
    #[serde(default)]
    pub sensitive_fields: bool,
    #[serde(default)]
    pub bulk_threshold: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditWrites {
    #[serde(default)]
    pub require_reason: Vec<String>,
    #[serde(default)]
    pub require_ticket_ref: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveRef {
    pub policy_ref: NamespacedRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilitySpec {
    #[serde(default)]
    pub slos: Vec<SloSpec>,
    #[serde(default, flatten)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub extras: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SloSpec {
    pub operation: String,
    #[serde(default)]
    pub target_p99_ms: Option<u32>,
    #[serde(default)]
    pub availability: Option<f64>,
    #[serde(default)]
    pub window: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScalingSpec {
    pub min: u32,
    pub max: u32,
    #[serde(default)]
    pub triggers: Vec<ScalingTrigger>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScalingTrigger {
    #[serde(rename = "type")]
    pub kind: String,
    pub threshold: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_tier_default_is_1() {
        assert_eq!(SearchTier::default() as u8, 1);
    }

    #[test]
    fn search_tier_eq_u8() {
        assert!(SearchTier::Tier3 == 3u8);
        assert!(SearchTier::Tier1 == 1u8);
    }

    #[test]
    fn field_with_ref_parses() {
        let yaml = r#"
name: supplier_code
type: string
required: true
filterable: true
ref:
  org: acme
  app: supply-chain
  domain: procurement
  object: supplier
  version: v1
  key: code
"#;
        let f: FieldSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(f.name, "supplier_code");
        assert_eq!(f.kind, FieldKind::String);
        assert!(f.required);
        assert!(f.filterable);
        let r = f.r#ref.unwrap();
        assert_eq!(r.object, "supplier");
        assert_eq!(r.key, "code");
    }

    #[test]
    fn cel_validation_round_trip() {
        let yaml = r#"
type: cel
rule: "self.status == 'cancelled' ? has(self.cancellation_reason) : true"
message: Cancellation reason required when cancelled
maxExecutionMs: 10
"#;
        let v: ValidationRule = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v.kind, ValidationKind::Cel);
        assert!(v.rule.as_deref().unwrap().starts_with("self.status"));
        assert_eq!(v.max_execution_ms, Some(10));
    }

    #[test]
    fn compare_validation_round_trip() {
        let yaml = r#"
type: compare
left: unit_price
operator: lte
right: approved_budget
message: unit_price cannot exceed approved_budget
"#;
        let v: ValidationRule = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v.kind, ValidationKind::Compare);
        assert_eq!(v.left.as_deref(), Some("unit_price"));
        assert_eq!(v.operator.as_deref(), Some("lte"));
        assert_eq!(v.right.as_deref(), Some("approved_budget"));
    }
}
