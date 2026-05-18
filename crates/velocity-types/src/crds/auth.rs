//! `AuthStrategy`, `RoleBinding`, `ApiKey` — identity and access CRDs.
//! See `docs/design.md §1.5-1.6`.

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::NamespacedRef;
use crate::crds::{Condition, ReconcilePhase};

// ─── AuthStrategy ───────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "AuthStrategy",
    namespaced,
    status = "AuthStrategyStatus",
    shortname = "auth"
)]
#[serde(rename_all = "camelCase")]
pub struct AuthStrategySpec {
    #[serde(rename = "type")]
    pub kind: AuthStrategyType,
    pub config: AuthStrategyConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthStrategyType {
    Jwt,
    Oidc,
    ApiKey,
    Composite,
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthStrategyConfig {
    #[serde(default)]
    pub issuers: Vec<IssuerConfig>,
    /// OIDC authorization-code flow configuration. Only meaningful when
    /// [`AuthStrategySpec::kind`] is [`AuthStrategyType::Oidc`]. The
    /// strategy still uses `issuers[].jwks_url` to verify the ID token's
    /// signature; this block carries the client-side endpoints and
    /// credentials needed to drive the redirect dance.
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
    #[serde(default)]
    pub revocation: Option<RevocationConfig>,
    #[serde(default)]
    pub cel: Option<CelConfig>,
    #[serde(default)]
    pub ttl_max: Option<u32>,
    #[serde(default)]
    pub clock_skew: Option<u32>,
    /// Ordered child strategy refs — only meaningful when [`AuthStrategySpec::kind`]
    /// is [`AuthStrategyType::Composite`]. The middleware walks this list in
    /// order, picks the first child whose credential scheme is present on
    /// the request, and runs *that* child's verification. There is no
    /// fall-through after a verification failure — JWT-fails-then-API-key
    /// is read as "no `Bearer` header, `ApiKey` header present", not "try
    /// JWT, then if it 401s try API key". (Defends against an attacker
    /// trading off two schemes' error oracles.)
    #[serde(default)]
    pub children: Vec<NamespacedRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssuerConfig {
    pub issuer: String,
    pub jwks_url: String,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub claims: ClaimMapping,
}

/// Claim mapping with optional JSONPath + transforms (see design §1.5).
/// We accept the right-hand side as a raw JSON value so transforms like
/// `{ path: "$.roles", transform: scope_to_roles }` round-trip losslessly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimMapping {
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub actor_id: Option<serde_json::Value>,
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub email: Option<serde_json::Value>,
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub roles: Option<serde_json::Value>,
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub attributes: BTreeMap<String, serde_json::Value>,
}

/// OIDC authorization-code flow configuration.
///
/// Endpoints are explicit (rather than via OIDC discovery) so a misconfigured
/// IdP can't change the redirect target at runtime. `client_secret_ref` names
/// a Kubernetes Secret holding the OAuth2 client secret — never inlined.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OidcConfig {
    /// IdP authorization endpoint — the user-agent is redirected here.
    pub authorization_endpoint: String,
    /// IdP token endpoint — back-channel exchange of the authorization code.
    pub token_endpoint: String,
    /// IdP userinfo endpoint — optional; when set, the callback fetches
    /// additional claims after token exchange. The middleware merges them
    /// over ID-token claims (later wins) before claim mapping runs.
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    /// OAuth2 client id registered with the IdP.
    pub client_id: String,
    /// Reference to a Secret containing the OAuth2 client secret. The
    /// operator reads it; the API server reads it via env at startup. The
    /// CRD never carries the plaintext.
    pub client_secret_ref: SecretRef,
    /// Where the IdP must redirect after authorization — must exactly match
    /// the value the IdP has registered for `client_id`.
    pub redirect_uri: String,
    /// Scopes requested at authorization time. Always includes `openid`.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Issuer string expected on the ID token — must match one of
    /// [`AuthStrategyConfig::issuers`]. Selects which JWKS to verify with.
    pub issuer: String,
    /// Browser-session lifetime in seconds. Defaults to 8 hours.
    #[serde(default)]
    pub session_ttl: Option<u32>,
}

/// Reference to a `kind: Secret` in the same namespace, with a specific
/// data key. Kubernetes-native — operators read it, the API server gets
/// the resolved value via env injection from the StatefulSet manifest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevocationConfig {
    pub backend: String, // redis
    #[serde(default)]
    pub key: Option<String>,
    /// ADR-003 — default false (deny on Redis failure).
    #[serde(default)]
    pub fail_open: bool,
    #[serde(default)]
    pub ttl: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CelConfig {
    /// Cap on per-rule CEL execution (milliseconds). ADR — CEL safety.
    pub max_execution_ms: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthStrategyStatus {
    pub phase: Option<ReconcilePhase>,
    pub issuers_resolved: Option<u32>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

// ─── RoleBinding ────────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "RoleBinding",
    namespaced,
    status = "RoleBindingStatus",
    shortname = "rb"
)]
#[serde(rename_all = "camelCase")]
pub struct RoleBindingSpec {
    pub actor_id: String,
    pub roles: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<ScopeSpec>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub granted_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScopeSpec {
    /// Schema name this scope applies to (within the binding's namespace).
    pub schema: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub operations: Vec<String>,
    /// Attribute filters (e.g., `region: west`).
    #[serde(default)]
    #[schemars(schema_with = "crate::common::preserve_unknown_fields")]
    pub attributes: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoleBindingStatus {
    pub phase: Option<ReconcilePhase>,
    pub revoked: Option<bool>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

// ─── ApiKey ─────────────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "velocity.sh",
    version = "v1",
    kind = "ApiKey",
    namespaced,
    status = "ApiKeyStatus",
    shortname = "key"
)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeySpec {
    pub actor: String,
    pub actor_type: String,
    #[serde(default)]
    pub scopes: Vec<ScopeSpec>,
    #[serde(default)]
    pub ip_allowlist: Vec<String>,
    #[serde(default)]
    pub expiry: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyStatus {
    pub phase: Option<ReconcilePhase>,
    /// Reference to the Secret holding the SHA256 hash (plaintext never stored).
    pub secret_ref: Option<String>,
    pub key_hash: Option<String>,
    pub revoked_at: Option<String>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authstrategy_yaml_jwt_minimal() {
        let yaml = r#"
type: jwt
config:
  issuers:
    - issuer: https://auth.acme.com
      jwksUrl: https://auth.acme.com/.well-known/jwks.json
      audience: velocity-api
  revocation:
    backend: redis
    failOpen: false
    ttl: 86400
  cel:
    maxExecutionMs: 10
  ttlMax: 3600
  clockSkew: 30
"#;
        let spec: AuthStrategySpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.kind, AuthStrategyType::Jwt);
        let cfg = &spec.config;
        assert_eq!(cfg.issuers.len(), 1);
        assert_eq!(cfg.issuers[0].issuer, "https://auth.acme.com");
        let rev = cfg.revocation.as_ref().unwrap();
        assert!(!rev.fail_open, "ADR-003 default must be fail-closed");
        assert_eq!(cfg.cel.as_ref().unwrap().max_execution_ms, 10);
    }

    #[test]
    fn rolebinding_scope_parses() {
        let yaml = r#"
actorId: ravi.kumar
roles: [procurement-reader]
scopes:
  - schema: purchase-order
    version: v2
    operations: [read]
    attributes:
      region: west
"#;
        let spec: RoleBindingSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.actor_id, "ravi.kumar");
        assert_eq!(spec.scopes[0].schema, "purchase-order");
        assert_eq!(spec.scopes[0].attributes.get("region").unwrap(), "west");
    }
}
