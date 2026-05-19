//! `ArchivePolicy`, `LogFilterPolicy`, `LogRoutingPolicy`.
//!
//! Skeleton-level specs for Phase 0. Full fields land in Phases 6 (logging)
//! and 8 (archive). Keep deserializable from the YAML examples in design.md.

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crds::{Condition, ReconcilePhase};

// ─── ArchivePolicy ──────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "ArchivePolicy",
    namespaced,
    status = "ArchivePolicyStatus",
    shortname = "ap"
)]
#[serde(rename_all = "camelCase")]
pub struct ArchivePolicySpec {
    pub schedule: String,
    pub trigger: ArchiveTrigger,
    #[serde(default)]
    pub batch_size: Option<u32>,
    #[serde(default)]
    pub max_duration: Option<String>,
    pub destination: ArchiveDestination,
    #[serde(default)]
    pub purge_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveTrigger {
    #[serde(rename = "type")]
    pub kind: String, // age|field|tableSize|cel
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub max_execution_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveDestination {
    pub backend: String, // postgres-cold | s3
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArchivePolicyStatus {
    pub phase: Option<ReconcilePhase>,
    pub last_run_at: Option<String>,
    pub records_archived: Option<u64>,
    /// Postgres schema name that will receive archived rows when the
    /// policy's `destination.backend = postgres-cold`. Set by the
    /// operator once the cold schema is provisioned; absent for s3
    /// destinations or until the spec passes validation.
    pub cold_schema: Option<String>,
    /// Postgres roles granted on the cold schema (reader/writer/admin).
    /// Surfaced for operator visibility — the eventual archive worker
    /// `SET LOCAL ROLE`s into the writer for inserts, mirroring ADR-007.
    #[serde(default)]
    pub cold_roles: Vec<String>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

// ─── LogFilterPolicy ────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "LogFilterPolicy",
    namespaced,
    status = "LogFilterPolicyStatus",
    shortname = "lfp"
)]
#[serde(rename_all = "camelCase")]
pub struct LogFilterPolicySpec {
    #[serde(default)]
    pub rules: Vec<LogFilterRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogFilterRule {
    pub name: String,
    pub priority: i32,
    pub action: String, // keep|drop|sample|redact
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub when: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub sample_rate: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogFilterPolicyStatus {
    pub phase: Option<ReconcilePhase>,
    pub distributed_to: Option<u32>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

// ─── LogRoutingPolicy ───────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "LogRoutingPolicy",
    namespaced,
    status = "LogRoutingPolicyStatus",
    shortname = "lrp"
)]
#[serde(rename_all = "camelCase")]
pub struct LogRoutingPolicySpec {
    #[serde(default)]
    pub destinations: Vec<LogDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogDestination {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String, // loki|s3|kafka
    #[serde(default, flatten)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub config: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogRoutingPolicyStatus {
    pub phase: Option<ReconcilePhase>,
    pub configured: Option<u32>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
