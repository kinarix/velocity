//! Operator-side Redis publisher for actor revocations.
//!
//! The API reads the same Redis set via
//! [`velocity_core::auth::RedisRevocationChecker`]. We share only the key
//! name (`revoked_actors` by default) — no shared crate dependency, so the
//! operator can run against a Redis that has *no* API connected yet.
//!
//! ## Why a separate publisher
//!
//! The API's `RedisRevocationChecker` is read-side only; teaching it about
//! writes would let any code path with a checker handle SADD/SREM, which
//! is the wrong shape (publisher is operator-only). Keeping it separate
//! also makes the operator's failure surface explicit: a Redis outage
//! must not block reconcile, just defer the revoke until Redis is back.

use redis::{aio::ConnectionManager, AsyncCommands, Client};
use thiserror::Error;

/// Same default as `velocity_core::auth::DEFAULT_REVOKED_SET_KEY`. Pinned
/// here to avoid the operator depending on `velocity-api`.
pub const DEFAULT_REVOKED_SET_KEY: &str = "revoked_actors";

#[derive(Debug, Error)]
pub enum RedisNotifyError {
    #[error("redis client open: {0}")]
    Open(String),

    #[error("redis connect: {0}")]
    Connect(String),

    #[error("redis command failed: {0}")]
    Command(String),
}

/// Thin wrapper around `redis::aio::ConnectionManager` that publishes
/// actor revocations to a Redis SET. `Clone` is cheap — the manager
/// internally `Arc`s its connection state.
#[derive(Clone)]
pub struct RedisNotify {
    manager: ConnectionManager,
    key: String,
}

impl std::fmt::Debug for RedisNotify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisNotify").field("key", &self.key).finish()
    }
}

impl RedisNotify {
    /// Open a connection manager. `key` is the SET name — pass
    /// [`DEFAULT_REVOKED_SET_KEY`] unless the deployment overrides it.
    pub async fn connect(url: &str, key: impl Into<String>) -> Result<Self, RedisNotifyError> {
        let client = Client::open(url).map_err(|e| RedisNotifyError::Open(e.to_string()))?;
        let manager = ConnectionManager::new(client)
            .await
            .map_err(|e| RedisNotifyError::Connect(e.to_string()))?;
        Ok(Self { manager, key: key.into() })
    }

    /// Add `actor_id` to the revoked set. Idempotent — the API only cares
    /// about membership, not how many times we asked.
    pub async fn revoke(&self, actor_id: &str) -> Result<(), RedisNotifyError> {
        let mut conn = self.manager.clone();
        let _: i64 = conn
            .sadd(&self.key, actor_id)
            .await
            .map_err(|e| RedisNotifyError::Command(format!("SADD: {e}")))?;
        Ok(())
    }

    /// Remove `actor_id` from the revoked set. Used when a binding is
    /// (re-)applied to clear a prior revocation. Also idempotent.
    pub async fn unrevoke(&self, actor_id: &str) -> Result<(), RedisNotifyError> {
        let mut conn = self.manager.clone();
        let _: i64 = conn
            .srem(&self.key, actor_id)
            .await
            .map_err(|e| RedisNotifyError::Command(format!("SREM: {e}")))?;
        Ok(())
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_malformed_url() {
        let err = RedisNotify::connect("not-a-url", "revoked_actors").await.unwrap_err();
        assert!(matches!(err, RedisNotifyError::Open(_)));
    }
}
