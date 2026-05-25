//! Caller identity carried through every request.
//!
//! Built by the auth middleware from a verified JWT (Phase 2a) or from API
//! key lookup (Phase 2c). Anonymous identities are produced only on routes
//! that explicitly opt out of auth — handlers that touch tenant data must
//! refuse to run on an anonymous identity.

use std::collections::HashMap;

use crate::auth::api_key::ApiKeyScope;

/// Resolved actor + attributes for the current request.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Stable actor identifier — written into `app.current_user` so RLS
    /// policies and `platform.audit_insert()` can read it.
    pub actor_id: String,
    /// Optional email — purely informational, never used for authorization.
    pub email: Option<String>,
    /// Roles the caller carries for this request. RBAC compares against
    /// `schema.access.roles[op]`. Always empty for API-key callers —
    /// they authorize through [`Identity::api_key_scopes`] instead.
    pub roles: Vec<String>,
    /// Free-form per-request attributes (`store_id`, `tenant_id`, etc.) used
    /// by RLS predicates. Each pair becomes a `SET LOCAL app.<key>` in the
    /// transaction prelude.
    pub attributes: HashMap<String, String>,
    /// `{namespace}/{name}` of the `AuthStrategy` that admitted this
    /// request. Recorded in audit so a token's provenance is reproducible.
    pub strategy: String,
    /// Verified `iss` claim. Empty for anonymous identities.
    pub issuer: String,
    /// `Some` exactly when the request was admitted via the API-key path.
    /// The Layer-1 access gate dispatches on this — `Some` → scope check,
    /// `None` → role check. Stashing scopes here keeps handlers from
    /// caring how the credential was verified.
    pub api_key_scopes: Option<Vec<ApiKeyScope>>,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            actor_id: "anonymous".into(),
            email: None,
            roles: Vec::new(),
            attributes: HashMap::new(),
            strategy: String::new(),
            issuer: String::new(),
            api_key_scopes: None,
        }
    }
}

impl Identity {
    pub fn anonymous() -> Self {
        Self::default()
    }

    pub fn is_anonymous(&self) -> bool {
        self.actor_id == "anonymous" && self.strategy.is_empty()
    }
}
