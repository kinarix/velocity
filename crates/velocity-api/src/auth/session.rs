//! Browser-session storage for the OIDC flow.
//!
//! After `/auth/callback` verifies the ID token and maps its claims to an
//! [`Identity`], the API server persists a row in `platform.sessions` and
//! hands the user a `velocity_session=<session_id>` cookie. On every
//! subsequent request the middleware reads the cookie, calls
//! [`SessionStore::lookup`], and reconstructs the identity from the
//! stored claims.
//!
//! ## Why a trait
//!
//! Same reason [`crate::auth::revocation::RevocationChecker`] is a trait:
//! integration tests need a deterministic in-memory implementation, and
//! a future backend swap (Vault session store, signed-cookie-only mode)
//! shouldn't ripple through the middleware. [`MockSessionStore`] gives
//! tests a deterministic seam; [`PgSessionStore`] is the production impl.
//!
//! ## Not the same `session` as `crate::session`
//!
//! `crate::session` is the per-transaction Postgres prelude (ADR-007).
//! This is the browser session — the user-facing artifact behind a
//! cookie. They are unrelated; the namespace prefix
//! `crate::auth::session` keeps them visibly distinct at every call
//! site.
//!
//! ## Refresh-token at-rest
//!
//! `platform.sessions.refresh_token` is documented as "encrypted at-rest
//! by app." This implementation stores **NULL** for `refresh_token` —
//! the OIDC flow runs without refresh, and id_token_claims are sufficient
//! to reconstruct an identity until session expiry. Wiring envelope
//! encryption (with a key from `VELOCITY_API_SESSION_KEY`) is tracked as
//! a follow-up; storing plaintext would silently break the schema's
//! contract, so refreshing access tokens is deferred rather than insecure.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::Value;
use sqlx::{PgPool, Row};
use thiserror::Error;
use uuid::Uuid;

/// Default browser-session lifetime — 8 hours.
///
/// Sized to comfortably outlive a working day's worth of interaction
/// without forcing a re-auth, but short enough that a stolen cookie
/// stops working before the user notices.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(8 * 3600);

/// Cookie name on the wire. Lowercase + underscore so it doesn't collide
/// with any well-known framework-specific cookie a portal app might also
/// set on the same domain.
pub const SESSION_COOKIE_NAME: &str = "velocity_session";

/// A row in `platform.sessions` — the user-facing browser session.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: Uuid,
    pub actor_id: String,
    pub issuer: String,
    /// ID-token + (optional) userinfo claims, merged. The middleware
    /// hands this to the strategy's [`crate::auth::CompiledClaimMapping`]
    /// on every request to reproduce the [`crate::Identity`].
    pub id_token_claims: Value,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session backend unavailable: {0}")]
    Backend(String),
    #[error("session expired or revoked")]
    Expired,
}

#[async_trait]
pub trait SessionStore: Send + Sync + std::fmt::Debug {
    /// Persist a new session row and return it. `id_token_claims` is the
    /// merged claim set (ID token ∪ userinfo) that
    /// [`crate::auth::CompiledClaimMapping`] will read from on each
    /// request. `expires_at` is computed by the caller from the strategy's
    /// `session_ttl`.
    async fn create(
        &self,
        actor_id: &str,
        issuer: &str,
        id_token_claims: Value,
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, SessionError>;

    /// Resolve a cookie session id to its record. Returns
    /// [`SessionError::Expired`] when the row is missing, past
    /// `expires_at`, or has `revoked_at IS NOT NULL` — the middleware
    /// translates that to a 401 so a stale cookie can't admit a request.
    async fn lookup(&self, id: Uuid) -> Result<SessionRecord, SessionError>;

    /// Mark a session as revoked (sets `revoked_at = now()`). Called
    /// from `/auth/logout`.
    async fn revoke(&self, id: Uuid) -> Result<(), SessionError>;
}

// ─── Postgres impl ─────────────────────────────────────────────────────────

/// Production [`SessionStore`] backed by `platform.sessions`.
///
/// Reads and writes go through the same `velocity_api` pool the rest of
/// the API uses. `platform.sessions` lives outside the per-domain RLS
/// surface; ADR-007 still applies (NOBYPASSRLS) but there is no
/// `SET LOCAL ROLE` because the schema isn't tied to a domain.
#[derive(Debug, Clone)]
pub struct PgSessionStore {
    pool: PgPool,
}

impl PgSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionStore for PgSessionStore {
    async fn create(
        &self,
        actor_id: &str,
        issuer: &str,
        id_token_claims: Value,
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, SessionError> {
        // `refresh_token` is left NULL — see module docs.
        let row = sqlx::query(
            "INSERT INTO platform.sessions (actor_id, issuer, id_token_claims, expires_at)
             VALUES ($1, $2, $3, $4)
             RETURNING id, created_at",
        )
        .bind(actor_id)
        .bind(issuer)
        .bind(&id_token_claims)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| SessionError::Backend(format!("insert: {e}")))?;

        let id: Uuid = row.try_get("id").map_err(|e| SessionError::Backend(e.to_string()))?;
        let created_at: DateTime<Utc> =
            row.try_get("created_at").map_err(|e| SessionError::Backend(e.to_string()))?;

        Ok(SessionRecord {
            id,
            actor_id: actor_id.to_string(),
            issuer: issuer.to_string(),
            id_token_claims,
            created_at,
            expires_at,
        })
    }

    async fn lookup(&self, id: Uuid) -> Result<SessionRecord, SessionError> {
        let row = sqlx::query(
            "SELECT id, actor_id, issuer, id_token_claims, created_at, expires_at
             FROM platform.sessions
             WHERE id = $1
               AND revoked_at IS NULL
               AND expires_at > now()",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| SessionError::Backend(format!("select: {e}")))?;

        let row = row.ok_or(SessionError::Expired)?;

        Ok(SessionRecord {
            id: row.try_get("id").map_err(|e| SessionError::Backend(e.to_string()))?,
            actor_id: row.try_get("actor_id").map_err(|e| SessionError::Backend(e.to_string()))?,
            issuer: row.try_get("issuer").map_err(|e| SessionError::Backend(e.to_string()))?,
            id_token_claims: row
                .try_get("id_token_claims")
                .map_err(|e| SessionError::Backend(e.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| SessionError::Backend(e.to_string()))?,
            expires_at: row
                .try_get("expires_at")
                .map_err(|e| SessionError::Backend(e.to_string()))?,
        })
    }

    async fn revoke(&self, id: Uuid) -> Result<(), SessionError> {
        sqlx::query(
            "UPDATE platform.sessions
             SET revoked_at = now()
             WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| SessionError::Backend(format!("update: {e}")))?;
        Ok(())
    }
}

// ─── Mock impl ─────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct MockSessionStore {
    inner: Arc<Mutex<HashMap<Uuid, SessionRecord>>>,
}

impl MockSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: SessionRecord) {
        self.inner.lock().insert(record.id, record);
    }
}

#[async_trait]
impl SessionStore for MockSessionStore {
    async fn create(
        &self,
        actor_id: &str,
        issuer: &str,
        id_token_claims: Value,
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, SessionError> {
        let record = SessionRecord {
            id: Uuid::new_v4(),
            actor_id: actor_id.to_string(),
            issuer: issuer.to_string(),
            id_token_claims,
            created_at: Utc::now(),
            expires_at,
        };
        self.inner.lock().insert(record.id, record.clone());
        Ok(record)
    }

    async fn lookup(&self, id: Uuid) -> Result<SessionRecord, SessionError> {
        let guard = self.inner.lock();
        let r = guard.get(&id).cloned().ok_or(SessionError::Expired)?;
        if r.expires_at <= Utc::now() {
            return Err(SessionError::Expired);
        }
        Ok(r)
    }

    async fn revoke(&self, id: Uuid) -> Result<(), SessionError> {
        // Simulate revoke by dropping — `lookup` returns Expired afterwards.
        self.inner.lock().remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn mock_create_then_lookup() {
        let store = MockSessionStore::new();
        let record = store
            .create(
                "ravi",
                "https://idp.acme.test",
                json!({"sub": "ravi"}),
                Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        let fetched = store.lookup(record.id).await.unwrap();
        assert_eq!(fetched.actor_id, "ravi");
    }

    #[tokio::test]
    async fn mock_returns_expired_for_unknown() {
        let store = MockSessionStore::new();
        let err = store.lookup(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }

    #[tokio::test]
    async fn mock_revoke_makes_lookup_fail() {
        let store = MockSessionStore::new();
        let record = store
            .create("ravi", "https://idp", json!({}), Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        store.revoke(record.id).await.unwrap();
        let err = store.lookup(record.id).await.unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }

    #[tokio::test]
    async fn mock_returns_expired_past_ttl() {
        let store = MockSessionStore::new();
        let record = store
            .create("ravi", "https://idp", json!({}), Utc::now() - chrono::Duration::seconds(1))
            .await
            .unwrap();
        let err = store.lookup(record.id).await.unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }
}
