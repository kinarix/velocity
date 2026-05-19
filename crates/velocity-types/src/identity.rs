//! Per-request identity and audit context propagated through API handlers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fail_mode::FailModeOutcome;

/// Where an actor came from. Mirrors the `actor_type` metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorType {
    Human,
    Service,
    Operator,
    Scheduler,
    Anonymous,
}

/// Resolved per-request identity. Built by the auth middleware from an
/// `AuthStrategy` claim pipeline; injected as an Axum `Extension`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub actor_id: String,
    pub actor_type: ActorType,
    pub roles: Vec<String>,
    /// Attribute claims (e.g., `store_id`, `region`). Used by row-filter
    /// templating and `SET LOCAL app.current_*` session variables.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// Name of the `AuthStrategy` that produced this identity.
    pub strategy: String,
    /// JWT issuer / OIDC issuer that minted the token, if applicable.
    #[serde(default)]
    pub issuer: Option<String>,
}

impl Identity {
    /// Anonymous identity for unauthenticated paths (e.g., `/healthz`).
    pub fn anonymous() -> Self {
        Self {
            actor_id: "anonymous".to_string(),
            actor_type: ActorType::Anonymous,
            roles: Vec::new(),
            attributes: BTreeMap::new(),
            strategy: "none".to_string(),
            issuer: None,
        }
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    pub fn has_any_role<I, S>(&self, roles: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        roles.into_iter().any(|r| self.has_role(r.as_ref()))
    }
}

/// Per-request audit context. Records the fail-modes that fired so the audit
/// row can document them (ADR-003 — "audit log records the fail-mode applied
/// to each request").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditContext {
    pub request_id: Option<String>,
    pub reason: Option<String>,
    pub ticket_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fail_modes: Vec<RecordedFailMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedFailMode {
    pub dependency: String,
    pub label: String,
    pub overridden: bool,
}

impl AuditContext {
    pub fn record(&mut self, outcome: &FailModeOutcome) {
        self.fail_modes.push(RecordedFailMode {
            dependency: format!("{:?}", outcome.dependency).to_lowercase(),
            label: outcome.label.to_string(),
            overridden: outcome.overridden,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fail_mode::{Dependency, FailMode};

    #[test]
    fn role_checks() {
        let id = Identity {
            actor_id: "ravi".into(),
            actor_type: ActorType::Human,
            roles: vec!["procurement-reader".into(), "audit-viewer".into()],
            attributes: BTreeMap::new(),
            strategy: "jwt-internal".into(),
            issuer: None,
        };
        assert!(id.has_role("procurement-reader"));
        assert!(!id.has_role("admin"));
        assert!(id.has_any_role(["admin", "audit-viewer"]));
        assert!(!id.has_any_role::<_, &str>([]));
    }

    #[test]
    fn audit_context_records_fail_modes() {
        let mut ctx = AuditContext::default();
        let outcome = FailMode::resolve(Dependency::RedisRevocation, true);
        ctx.record(&outcome);
        assert_eq!(ctx.fail_modes.len(), 1);
        assert!(ctx.fail_modes[0].overridden);
        assert_eq!(ctx.fail_modes[0].label, "redis_revocation_fail_open");
    }

    #[test]
    fn anonymous_identity_carries_safe_defaults() {
        let id = Identity::anonymous();
        assert_eq!(id.actor_id, "anonymous");
        assert!(matches!(id.actor_type, ActorType::Anonymous));
        assert!(id.roles.is_empty());
        assert!(id.attributes.is_empty());
        assert_eq!(id.strategy, "none");
        assert!(id.issuer.is_none());
        // Anonymous must not satisfy any role check.
        assert!(!id.has_role("admin"));
        assert!(!id.has_any_role(["admin", "reader"]));
    }
}
