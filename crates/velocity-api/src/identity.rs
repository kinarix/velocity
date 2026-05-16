//! Caller identity carried through every request.
//!
//! Phase 1 is auth-free; we plumb a stub `Identity` so the rest of the stack
//! (transaction context, audit, RBAC) can be written against the real shape
//! and not need rewriting when Phase 2 adds JWT / API-key / OIDC.

use std::collections::HashMap;

/// Resolved actor + attributes for the current request. The handler builds
/// this from request headers (Phase 2+); for now `Default` is used.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Stable actor identifier — written into `app.current_user` so RLS
    /// policies and `platform.audit_insert()` can read it.
    pub actor_id: String,
    /// Free-form per-request attributes (`store_id`, `tenant_id`, etc.) used
    /// by RLS predicates. Each pair becomes a `SET LOCAL app.<key>` in the
    /// transaction prelude.
    pub attributes: HashMap<String, String>,
}

impl Default for Identity {
    fn default() -> Self {
        Self { actor_id: "anonymous".into(), attributes: HashMap::new() }
    }
}

impl Identity {
    pub fn anonymous() -> Self {
        Self::default()
    }
}
