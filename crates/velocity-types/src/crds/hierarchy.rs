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
    /// Default data-API scope for domains in this application.
    /// Domains may override with their own `spec.deployment.scope`.
    /// Absent → `App` scope.
    #[serde(default)]
    pub deployment: Option<DeploymentConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationStatus {
    pub phase: Option<ReconcilePhase>,
    pub domains: Option<u32>,
    pub schemas: Option<u32>,
    /// Name of the operator-managed app-scoped data-API Deployment, set when
    /// an app-scoped data-API has been materialised.
    pub data_api_deployment: Option<String>,
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
    /// ADR-011: data-API scope override for this domain.
    /// Absent → inherits the Application's `spec.deployment.scope`, or `app`
    /// if the Application also omits it.
    /// `scope: domain` → the operator materialises a per-domain data-API
    /// Deployment in this domain's namespace.
    #[serde(default)]
    pub deployment: Option<DeploymentConfig>,
}

/// API deployment configuration — appears on both `ApplicationSpec` and
/// `DomainSpec`. `scope` controls whether a shared app-scoped or a dedicated
/// domain-scoped data-API is materialised by the operator (ADR-011).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentConfig {
    #[serde(default)]
    pub scope: DeploymentScope,
    /// Minimum replicas (KEDA `minReplicaCount`). Default 1 — never
    /// scale to zero, so a domain's first request is never cold.
    #[serde(default)]
    pub min_replicas: Option<u32>,
    /// Maximum replicas (KEDA/HPA ceiling). Default 10.
    #[serde(default)]
    pub max_replicas: Option<u32>,
    /// Pod resource requests/limits for the data-API container.
    #[serde(default)]
    pub resources: Option<DeploymentResources>,
}

/// `app` → one operator-managed data-API pod per `{org}/{app}`, serving all
/// of that app's domains via label selector. `domain` → one pod per
/// `{org}/{app}/{domain}`, watching only that namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentScope {
    #[default]
    App,
    Domain,
}

/// Returns the effective `DeploymentScope` for a domain, applying the
/// domain-level override when present, then the app-level default, then `App`.
pub fn effective_scope(
    app: Option<&DeploymentConfig>,
    domain: Option<&DeploymentConfig>,
) -> DeploymentScope {
    domain
        .map(|d| d.scope)
        .or_else(|| app.map(|a| a.scope))
        .unwrap_or(DeploymentScope::App)
}

/// Kubernetes resource requests/limits, mirrored as plain strings so the
/// operator can drop them straight into the pod template (e.g. `"250m"`,
/// `"256Mi"`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentResources {
    #[serde(default)]
    pub requests: Option<ResourceQuantities>,
    #[serde(default)]
    pub limits: Option<ResourceQuantities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceQuantities {
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
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
    /// Name of the operator-managed data-API Deployment, set when
    /// `spec.deployment.scope == domain`. Absent when served by an app-scoped pod.
    pub data_api_deployment: Option<String>,
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

    #[test]
    fn deployment_scope_serializes_lowercase() {
        assert_eq!(serde_yaml::to_string(&DeploymentScope::App).unwrap().trim(), "app");
        assert_eq!(serde_yaml::to_string(&DeploymentScope::Domain).unwrap().trim(), "domain");
    }

    #[test]
    fn effective_scope_both_none_returns_app() {
        assert_eq!(effective_scope(None, None), DeploymentScope::App);
    }

    #[test]
    fn effective_scope_app_only_returns_app_scope() {
        let app_cfg = DeploymentConfig { scope: DeploymentScope::Domain, ..Default::default() };
        assert_eq!(effective_scope(Some(&app_cfg), None), DeploymentScope::Domain);
    }

    #[test]
    fn effective_scope_domain_only_returns_domain_scope() {
        let domain_cfg = DeploymentConfig { scope: DeploymentScope::Domain, ..Default::default() };
        assert_eq!(effective_scope(None, Some(&domain_cfg)), DeploymentScope::Domain);
    }

    #[test]
    fn effective_scope_domain_overrides_app() {
        let app_cfg = DeploymentConfig { scope: DeploymentScope::App, ..Default::default() };
        let domain_cfg = DeploymentConfig { scope: DeploymentScope::Domain, ..Default::default() };
        assert_eq!(effective_scope(Some(&app_cfg), Some(&domain_cfg)), DeploymentScope::Domain);
    }

    #[test]
    fn domain_deployment_scope_round_trip() {
        let yaml = r#"
app: supply-chain
displayName: Procurement
access:
  defaultRole: procurement-reader
  adminRole: procurement-admin
deployment:
  scope: domain
  minReplicas: 2
"#;
        let spec: DomainSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.deployment.as_ref().unwrap().scope, DeploymentScope::Domain);
        assert_eq!(spec.deployment.as_ref().unwrap().min_replicas, Some(2));
    }

    #[test]
    fn application_deployment_scope_round_trip() {
        let yaml = r#"
org: acme
displayName: Supply Chain
deployment:
  scope: app
"#;
        let spec: ApplicationSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.deployment.as_ref().unwrap().scope, DeploymentScope::App);
    }
}
