//! Velocity CRDs.
//!
//! All CRDs use group `velocity.sh`, version `v1`. See `CLAUDE.md › CRD Conventions`.

pub mod auth;
pub mod hierarchy;
pub mod policies;
pub mod purge;
pub mod schema;

pub use auth::{
    ApiKey, ApiKeySpec, ApiKeyStatus, AuthStrategy, AuthStrategySpec, AuthStrategyStatus,
    AuthStrategyType, ClaimMapping, IssuerConfig, RevocationConfig, RoleBinding, RoleBindingSpec,
    RoleBindingStatus, ScopeSpec,
};
pub use hierarchy::{
    Application, ApplicationSpec, ApplicationStatus, Domain, DomainSpec, DomainStatus,
    Organisation, OrganisationSpec, OrganisationStatus, ResourceQuotas, TenancyMode,
};
pub use policies::{
    ArchivePolicy, ArchivePolicySpec, ArchivePolicyStatus, LogFilterPolicy, LogFilterPolicySpec,
    LogFilterPolicyStatus, LogRoutingPolicy, LogRoutingPolicySpec, LogRoutingPolicyStatus,
};
pub use purge::{PurgeRequest, PurgeRequestSpec, PurgeRequestStatus};
pub use schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, PartitioningSpec, ScalingSpec,
    SchemaDefinition, SchemaDefinitionSpec, SchemaDefinitionStatus, SearchSpec, SearchTier,
    Sensitivity, ValidationKind, ValidationRule,
};

use serde::{Deserialize, Serialize};

/// Common condition shape reused in `*Status.conditions`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "lastTransitionTime")]
    pub last_transition_time: Option<String>,
}

/// Phase reported by an operator-managed resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ReconcilePhase {
    Pending,
    Provisioning,
    Ready,
    Failed,
    Deprecated,
}
