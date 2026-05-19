//! Actor revocation check — ADR-003 fail-mode matrix.
//!
//! The auth middleware calls [`RevocationChecker::is_revoked`] after the
//! JWT signature verifies. The check is one Redis `SISMEMBER revoked_actors`
//! against the actor id the claim mapping produced.
//!
//! ## Why a trait
//!
//! Two reasons:
//!
//! 1. Integration tests need a deterministic implementation — spinning up a
//!    real Redis for every test multiplies CI time and adds flake risk.
//!    [`MockChecker`] gives us in-memory revocation behaviour.
//! 2. The `RoleBinding` operator (task #24) will publish revocations from
//!    Kubernetes by writing to the same Redis key, so the production impl
//!    is a thin wrapper around `redis::aio::ConnectionManager`. Other
//!    backends (HashiCorp Vault, an external IdP feed) could be slotted in
//!    behind the same trait without churning the middleware.
//!
//! ## Decision shape
//!
//! [`RevocationDecision`] is what the middleware persists onto the request
//! extension. ADR-003 requires that *every* request records the fail mode
//! that admitted (or rejected) it — even the happy `Allowed` case, so that
//! audit can prove the revocation backend was queried. Collapsing this into
//! a single bool would lose the difference between "Redis up, actor not
//! revoked" and "Redis down, admitted by fail-open policy".

use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use thiserror::Error;

/// Default Redis set key holding revoked actor ids. The `AuthStrategy`
/// CRD may override this per strategy via `config.revocation.key`.
pub const DEFAULT_REVOKED_SET_KEY: &str = "revoked_actors";

/// Transport-level failures from a [`RevocationChecker`]. Surface only —
/// the fail-open/fail-closed decision lives in the middleware, since it
/// needs to read `revocation_fail_open` off the strategy.
#[derive(Debug, Error)]
pub enum RevocationError {
    /// Network error, auth error, malformed response — anything that means
    /// "we couldn't reach the backend or got nonsense back."
    #[error("revocation backend unavailable: {0}")]
    Backend(String),
}

/// Outcome of a revocation check, recorded on the request extension so the
/// audit pipeline can render `p_fail_modes` JSONB for every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationDecision {
    /// Backend was reachable, actor is not in the revoked set.
    Allowed,
    /// Backend was reachable, actor *is* in the revoked set. The middleware
    /// converts this to `ApiError::Revoked` (403).
    RevokedActor,
    /// Backend was unreachable; strategy is fail-closed → reject with 503.
    BackendDownDenied,
    /// Backend was unreachable; strategy is fail-open → admit, but the
    /// audit row will show `fail_mode = "open"`.
    BackendDownAdmitted,
}

impl RevocationDecision {
    /// Stable string used in the audit row's `p_fail_modes` JSONB blob.
    pub fn as_audit_str(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::RevokedActor => "revoked",
            Self::BackendDownDenied => "backend_down_denied",
            Self::BackendDownAdmitted => "backend_down_admitted",
        }
    }
}

#[async_trait]
pub trait RevocationChecker: Send + Sync + Debug {
    /// Returns `Ok(true)` iff `actor_id` is in the revoked set. The trait
    /// makes no statement about how to handle `Err` — the middleware reads
    /// the `revocation_fail_open` flag and decides.
    async fn is_revoked(&self, actor_id: &str) -> Result<bool, RevocationError>;
}

// ─── Redis impl ────────────────────────────────────────────────────────────

/// Production [`RevocationChecker`] backed by `SISMEMBER` on a Redis set.
///
/// The connection manager auto-reconnects in the background, so transient
/// blips bubble up as `RevocationError::Backend` for one or two requests
/// rather than tearing down the whole API process.
#[derive(Clone)]
pub struct RedisRevocationChecker {
    manager: redis::aio::ConnectionManager,
    key: String,
}

impl Debug for RedisRevocationChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisRevocationChecker").field("key", &self.key).finish()
    }
}

impl RedisRevocationChecker {
    /// Open a connection manager against `redis_url` (`redis://host:port` or
    /// `rediss://...`). `key` is the set name to query — pass
    /// [`DEFAULT_REVOKED_SET_KEY`] if the strategy doesn't override it.
    pub async fn connect(redis_url: &str, key: impl Into<String>) -> Result<Self, RevocationError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| RevocationError::Backend(format!("client open: {e}")))?;
        let manager = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| RevocationError::Backend(format!("connect: {e}")))?;
        Ok(Self { manager, key: key.into() })
    }
}

#[async_trait]
impl RevocationChecker for RedisRevocationChecker {
    async fn is_revoked(&self, actor_id: &str) -> Result<bool, RevocationError> {
        // Cheaply clone the manager — under the hood it's an Arc'd connection.
        let mut conn = self.manager.clone();
        let revoked: bool = redis::cmd("SISMEMBER")
            .arg(&self.key)
            .arg(actor_id)
            .query_async(&mut conn)
            .await
            .map_err(|e| RevocationError::Backend(format!("SISMEMBER: {e}")))?;
        Ok(revoked)
    }
}

// ─── Mock impl ─────────────────────────────────────────────────────────────

/// In-memory [`RevocationChecker`] for tests.
///
/// Two knobs:
/// - `revoked`: explicit set of actor ids to return as revoked.
/// - `fail`: when `true`, every call returns `Err(RevocationError::Backend)`
///   so middleware-side fail-open/fail-closed behaviour can be exercised
///   without a network failure injector.
#[derive(Debug, Default, Clone)]
pub struct MockChecker {
    inner: Arc<Mutex<MockInner>>,
}

#[derive(Debug, Default)]
struct MockInner {
    revoked: HashSet<String>,
    fail: bool,
}

impl MockChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revoke(&self, actor_id: impl Into<String>) {
        self.inner.lock().revoked.insert(actor_id.into());
    }

    pub fn unrevoke(&self, actor_id: &str) {
        self.inner.lock().revoked.remove(actor_id);
    }

    pub fn set_failing(&self, fail: bool) {
        self.inner.lock().fail = fail;
    }
}

#[async_trait]
impl RevocationChecker for MockChecker {
    async fn is_revoked(&self, actor_id: &str) -> Result<bool, RevocationError> {
        let guard = self.inner.lock();
        if guard.fail {
            return Err(RevocationError::Backend("mock configured to fail".into()));
        }
        Ok(guard.revoked.contains(actor_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_false_when_actor_not_in_set() {
        let m = MockChecker::new();
        assert!(!m.is_revoked("alice").await.unwrap());
    }

    #[tokio::test]
    async fn mock_returns_true_after_revoke() {
        let m = MockChecker::new();
        m.revoke("alice");
        assert!(m.is_revoked("alice").await.unwrap());
        assert!(!m.is_revoked("bob").await.unwrap());
    }

    #[tokio::test]
    async fn mock_unrevoke_clears_flag() {
        let m = MockChecker::new();
        m.revoke("alice");
        m.unrevoke("alice");
        assert!(!m.is_revoked("alice").await.unwrap());
    }

    #[tokio::test]
    async fn mock_set_failing_surfaces_error() {
        let m = MockChecker::new();
        m.set_failing(true);
        let err = m.is_revoked("alice").await.unwrap_err();
        assert!(matches!(err, RevocationError::Backend(_)));
    }

    #[tokio::test]
    async fn redis_checker_rejects_malformed_url() {
        let err = RedisRevocationChecker::connect("not-a-url", "revoked_actors").await.unwrap_err();
        assert!(matches!(err, RevocationError::Backend(_)));
    }

    #[test]
    fn decision_audit_strings_are_stable() {
        // Audit consumers (Grafana queries, alerts) key off these strings —
        // changing them is a breaking change for dashboards.
        assert_eq!(RevocationDecision::Allowed.as_audit_str(), "allowed");
        assert_eq!(RevocationDecision::RevokedActor.as_audit_str(), "revoked");
        assert_eq!(RevocationDecision::BackendDownDenied.as_audit_str(), "backend_down_denied");
        assert_eq!(RevocationDecision::BackendDownAdmitted.as_audit_str(), "backend_down_admitted");
    }
}
