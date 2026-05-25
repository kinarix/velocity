//! Application state for the platform/admin control surface.
//!
//! Carries the schema registry (for the `/api` index), the Postgres pool
//! (audit reads/writes), the shared cursor signer (audit keyset cursor),
//! and the platform-audit bearer token. No tiering (data tier), no
//! Typesense (search tier).

use std::sync::Arc;

use sqlx::PgPool;
use velocity_core::registry::SchemaRegistry;
use velocity_core::CursorSigner;

#[derive(Debug, Clone)]
pub struct PlatformState {
    pub registry: Arc<SchemaRegistry>,
    pub pool: PgPool,
    /// HMAC signer shared with the `/audit` keyset cursor. `None` when
    /// `VELOCITY_API_CURSOR_SIGNING_KEY` is unset.
    pub cursor_signer: Option<Arc<CursorSigner>>,
    /// Phase 6a-2: shared secret accepted at `/api/platform/audit*`.
    /// `None` => those endpoints uniformly return 401, never admit a
    /// caller. Wrapped in `Arc` because the comparison happens on every
    /// audit-endpoint request and the string itself never mutates.
    pub platform_audit_token: Option<Arc<String>>,
}

impl PlatformState {
    pub fn new(registry: Arc<SchemaRegistry>, pool: PgPool) -> Self {
        Self { registry, pool, cursor_signer: None, platform_audit_token: None }
    }

    /// Inject a cursor signer. Used by main.rs when
    /// `VELOCITY_API_CURSOR_SIGNING_KEY` is configured.
    pub fn with_cursor_signer(mut self, signer: Arc<CursorSigner>) -> Self {
        self.cursor_signer = Some(signer);
        self
    }

    /// Inject the shared secret accepted at `/api/platform/audit*`. Used
    /// by main.rs when `VELOCITY_API_PLATFORM_AUDIT_TOKEN` is configured.
    pub fn with_platform_audit_token(mut self, token: Arc<String>) -> Self {
        self.platform_audit_token = Some(token);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://x:x@127.0.0.1:1/x")
            .unwrap()
    }

    #[tokio::test]
    async fn with_cursor_signer_attaches_signer() {
        let (registry, _) = SchemaRegistry::new();
        let signer = Arc::new(CursorSigner::new(vec![0u8; 32]).unwrap());
        let state = PlatformState::new(registry, empty_pool()).with_cursor_signer(signer.clone());
        assert!(state.cursor_signer.is_some());
    }

    #[tokio::test]
    async fn with_platform_audit_token_attaches_token() {
        let (registry, _) = SchemaRegistry::new();
        let state = PlatformState::new(registry, empty_pool())
            .with_platform_audit_token(Arc::new("a-valid-token-xxxx".into()));
        assert!(state.platform_audit_token.is_some());
        assert!(state.platform_audit_token.unwrap().len() >= 16);
    }
}
