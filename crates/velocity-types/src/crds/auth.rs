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
    /// JWKS endpoint URL. Required for JWT strategies. For OIDC strategies
    /// the API server fills this from the discovery document at strategy
    /// load time when [`OidcConfig::config_url`] is set and this is empty,
    /// so users with a discovery URL can omit it.
    #[serde(default)]
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
/// Two operating modes:
///
/// * **Pinned (default).** Specify `authorization_endpoint`, `token_endpoint`,
///   `issuer`, and `issuers[].jwks_url` explicitly. The API server never
///   contacts the IdP's discovery doc — a compromised
///   `.well-known/openid-configuration` cannot move the redirect target.
/// * **Discovery.** Set `config_url` to the IdP's
///   `.well-known/openid-configuration`. The API server fetches it once
///   when the `AuthStrategy` is loaded into the registry and uses it to
///   fill in any endpoint fields you left unset. Discovery happens at
///   apply time (a Kubernetes event), not on every request, so the
///   endpoints are still effectively pinned for the lifetime of the
///   `AuthStrategy` revision. Explicit fields always win over discovery —
///   you can mix and match (e.g. discover everything except the userinfo
///   endpoint, which you override to a tenant-specific URL).
///
/// `client_secret_ref` names a Kubernetes Secret holding the OAuth2 client
/// secret — never inlined into the CRD.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OidcConfig {
    /// OIDC discovery URL — usually `https://<idp>/.well-known/openid-configuration`.
    /// When set, the API server fetches this document once at strategy
    /// load time and uses it to populate any endpoint fields below (and
    /// the matching `issuers[].jwks_url`) that you left unset. Explicit
    /// values in the CRD always win. If discovery fails at load time and
    /// any required endpoint is still unset, the strategy is rejected and
    /// will not be registered — fail-closed.
    #[serde(default)]
    pub config_url: Option<String>,
    /// IdP authorization endpoint — the user-agent is redirected here.
    /// Required unless [`Self::config_url`] is set and discovery returns
    /// an `authorization_endpoint`.
    #[serde(default)]
    pub authorization_endpoint: String,
    /// IdP token endpoint — back-channel exchange of the authorization
    /// code. Required unless [`Self::config_url`] is set and discovery
    /// returns a `token_endpoint`.
    #[serde(default)]
    pub token_endpoint: String,
    /// IdP userinfo endpoint — optional; when set, the callback fetches
    /// additional claims after token exchange. The middleware merges them
    /// over ID-token claims (later wins) before claim mapping runs. May
    /// also be populated from discovery's `userinfo_endpoint`.
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
    /// May be populated from discovery's `issuer` claim when
    /// [`Self::config_url`] is set.
    #[serde(default)]
    pub issuer: String,
    /// Browser-session lifetime in seconds. Defaults to 8 hours.
    #[serde(default)]
    pub session_ttl: Option<u32>,
}

impl OidcConfig {
    /// True when at least one endpoint field is unset AND `config_url` is
    /// set — the API server needs to fetch discovery to make the strategy
    /// usable. When `config_url` is unset, missing endpoint fields are a
    /// hard config error caught at strategy load time.
    pub fn needs_discovery(&self) -> bool {
        self.config_url.is_some()
            && (self.authorization_endpoint.is_empty()
                || self.token_endpoint.is_empty()
                || self.issuer.is_empty()
                || self.userinfo_endpoint.is_none())
    }

    /// True when every endpoint required by the OIDC redirect flow is
    /// populated. Callers use this after merging discovery to decide
    /// whether the strategy is fit to register.
    pub fn endpoints_complete(&self) -> bool {
        !self.authorization_endpoint.is_empty()
            && !self.token_endpoint.is_empty()
            && !self.issuer.is_empty()
    }
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
    fn oidc_config_url_only_parses_with_empty_endpoints() {
        let yaml = r#"
type: oidc
config:
  issuers:
    - issuer: "https://idp.example.com"
      audience: "velocity-api"
      claims:
        actorId: "$.sub"
  oidc:
    configUrl: "https://idp.example.com/.well-known/openid-configuration"
    clientId: "velocity-api"
    clientSecretRef:
      name: oidc-client-secret
      key: client_secret
    redirectUri: "https://velocity.example.com/auth/callback/platform/oidc-default"
    scopes: [openid, profile, email]
"#;
        let spec: AuthStrategySpec = serde_yaml::from_str(yaml).unwrap();
        let oidc = spec.config.oidc.as_ref().unwrap();
        assert_eq!(
            oidc.config_url.as_deref(),
            Some("https://idp.example.com/.well-known/openid-configuration")
        );
        assert_eq!(oidc.authorization_endpoint, "");
        assert_eq!(oidc.token_endpoint, "");
        assert_eq!(oidc.issuer, "");
        assert!(oidc.needs_discovery());
        assert!(!oidc.endpoints_complete());
        // The lone issuer also relies on discovery for jwks_url.
        assert_eq!(spec.config.issuers[0].jwks_url, "");
    }

    #[test]
    fn oidc_explicit_endpoints_dont_need_discovery() {
        let yaml = r#"
type: oidc
config:
  issuers:
    - issuer: "https://idp.example.com"
      jwksUrl: "https://idp.example.com/.well-known/jwks.json"
      audience: "velocity-api"
      claims: { actorId: "$.sub" }
  oidc:
    authorizationEndpoint: "https://idp.example.com/oauth2/authorize"
    tokenEndpoint: "https://idp.example.com/oauth2/token"
    userinfoEndpoint: "https://idp.example.com/oauth2/userinfo"
    clientId: "velocity-api"
    clientSecretRef: { name: s, key: client_secret }
    redirectUri: "https://velocity.example.com/auth/callback/platform/oidc-default"
    issuer: "https://idp.example.com"
"#;
        let spec: AuthStrategySpec = serde_yaml::from_str(yaml).unwrap();
        let oidc = spec.config.oidc.as_ref().unwrap();
        assert!(!oidc.needs_discovery());
        assert!(oidc.endpoints_complete());
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
