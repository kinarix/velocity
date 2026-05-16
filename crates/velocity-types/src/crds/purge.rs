//! `PurgeRequest` — operator-raised CRD when archived records hit `purgeAfter`.
//! Approved via an annotation; deletion is hard. See Phase 8.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crds::{Condition, ReconcilePhase};

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "PurgeRequest",
    namespaced,
    status = "PurgeRequestStatus",
    shortname = "purge"
)]
#[serde(rename_all = "camelCase")]
pub struct PurgeRequestSpec {
    /// Schema whose archived records are eligible for purge.
    pub schema: String,
    pub version: String,
    /// ISO-8601 cutoff; records older than this in archive will be purged.
    pub older_than: String,
    /// Estimated number of records the operator computed at creation time.
    #[serde(default)]
    pub estimated_records: Option<u64>,
    /// Free-form reason (set by operator from policy).
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PurgeRequestStatus {
    pub phase: Option<ReconcilePhase>,
    /// Set true when a human approves via `velocity.sh/approved-by` annotation.
    pub approved: Option<bool>,
    pub approved_by: Option<String>,
    pub purged_at: Option<String>,
    pub purged_records: Option<u64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
