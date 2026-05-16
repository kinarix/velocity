//! `Organisation`, `Application`, `Domain` — the three-level hierarchy that
//! owns every `SchemaDefinition`. See `docs/design.md §1.1-1.3`.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::NamespacedRef;
use crate::crds::{Condition, ReconcilePhase};

// ─── Organisation ───────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "Organisation",
    namespaced,
    status = "OrganisationStatus",
    shortname = "org"
)]
#[serde(rename_all = "camelCase")]
pub struct OrganisationSpec {
    pub display_name: String,
    #[serde(default)]
    pub tenancy_mode: TenancyMode,
    pub default_auth_strategy: Option<NamespacedRef>,
    #[serde(default)]
    pub default_policies: DefaultPolicies,
    #[serde(default)]
    pub admin_roles: Vec<String>,
    #[serde(default)]
    pub resource_quotas: Option<ResourceQuotas>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrganisationStatus {
    pub phase: Option<ReconcilePhase>,
    pub applications: Option<u32>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// ADR-010 — single vs multi-tenant scoping of cross-org refs and search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TenancyMode {
    #[default]
    Single,
    MultiTenant,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefaultPolicies {
    #[serde(default)]
    pub audit: Option<String>, // strict | standard | minimal
    #[serde(default)]
    pub retention: Option<String>, // e.g. "7years"
    #[serde(default)]
    pub time_machine: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceQuotas {
    #[serde(default)]
    pub max_applications: Option<u32>,
    #[serde(default)]
    pub max_schemas: Option<u32>,
    #[serde(default)]
    pub max_versions_per_schema: Option<u32>,
    #[serde(default)]
    pub max_fields_per_schema: Option<u32>,
    #[serde(default)]
    pub max_records_per_schema: Option<u64>,
    #[serde(default)]
    pub max_storage_gb: Option<u64>,
    #[serde(default)]
    pub requests_per_second: Option<u32>,
}

// ─── Application ────────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "Application",
    namespaced,
    status = "ApplicationStatus",
    shortname = "app"
)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationSpec {
    pub org: String,
    pub display_name: String,
    pub owner: Option<String>,
    pub team: Option<String>,
    pub auth_strategy: Option<NamespacedRef>,
    #[serde(default)]
    pub resource_quotas: Option<ResourceQuotas>,
    #[serde(default)]
    pub database_quota: Option<DatabaseQuota>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationStatus {
    pub phase: Option<ReconcilePhase>,
    pub domains: Option<u32>,
    pub schemas: Option<u32>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseQuota {
    #[serde(default)]
    pub pool_size: Option<u32>,
    #[serde(default)]
    pub read_replicas: Option<u32>,
    /// Use cold-tier tablespace for the archive schema.
    #[serde(default)]
    pub cold_tablespace: Option<bool>,
}

// ─── Domain ─────────────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "Domain",
    namespaced,
    status = "DomainStatus",
    shortname = "dom"
)]
#[serde(rename_all = "camelCase")]
pub struct DomainSpec {
    pub app: String,
    pub display_name: String,
    pub access: DomainAccess,
    #[serde(default)]
    pub database_quota: Option<DatabaseQuota>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DomainAccess {
    pub default_role: String,
    pub admin_role: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DomainStatus {
    pub phase: Option<ReconcilePhase>,
    /// Postgres schema this Domain provisioned, once Ready.
    pub pg_schema: Option<String>,
    /// Postgres roles created for this domain.
    #[serde(default)]
    pub pg_roles: Vec<String>,
    pub schemas: Option<u32>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn organisation_yaml_round_trip() {
        let yaml = r#"
displayName: Acme Corp
tenancyMode: single
defaultPolicies:
  audit: strict
  retention: 7years
  timeMachine: true
adminRoles: [platform-admin]
resourceQuotas:
  maxApplications: 50
"#;
        let spec: OrganisationSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.display_name, "Acme Corp");
        assert_eq!(spec.tenancy_mode, TenancyMode::Single);
        assert_eq!(spec.resource_quotas.as_ref().unwrap().max_applications, Some(50));
        let round_tripped = serde_yaml::to_string(&spec).unwrap();
        assert!(round_tripped.contains("displayName: Acme Corp"));
    }

    #[test]
    fn domain_yaml_round_trip() {
        let yaml = r#"
app: supply-chain
displayName: Procurement
access:
  defaultRole: procurement-reader
  adminRole: procurement-admin
databaseQuota:
  poolSize: 20
  coldTablespace: true
"#;
        let spec: DomainSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.app, "supply-chain");
        assert_eq!(spec.access.default_role, "procurement-reader");
        assert_eq!(spec.database_quota.as_ref().unwrap().pool_size, Some(20));
    }

    #[test]
    fn tenancy_mode_serializes_kebab_case() {
        assert_eq!(
            serde_yaml::to_string(&TenancyMode::MultiTenant).unwrap().trim(),
            "multi-tenant"
        );
    }
}
