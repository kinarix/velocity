//! Shared application state injected into Axum handlers.

use std::sync::Arc;

use sqlx::PgPool;

use crate::dsl::CursorSigner;
use crate::registry::SchemaRegistry;
use crate::tiering::{cold_stub::ColdJobStore, PostgresEventReader, TieredEventReader};
use crate::typesense::TypesenseClient;

#[derive(Debug, Clone)]
pub struct AppState {
    pub registry: Arc<SchemaRegistry>,
    pub pool: PgPool,
    /// Tier-routing event reader for time-machine reads. Always
    /// present; the warm impl is `None` when the deployment doesn't
    /// configure a warm-reader (config-test scenarios), and the
    /// router responds `WARM_TIER_NOT_CONFIGURED` for warm requests
    /// in that case.
    pub tiered_reader: Arc<TieredEventReader>,
    pub cold_jobs: Arc<ColdJobStore>,
    /// Phase 5: HMAC signer for POST /query cursors. `None` when
    /// `VELOCITY_API_CURSOR_SIGNING_KEY` is unset — pagination still
    /// works for the first page, but cursor-bearing requests fail with
    /// a clear 400 instead of being silently misinterpreted.
    pub cursor_signer: Option<Arc<CursorSigner>>,
    /// Phase 5c: shared Typesense client for the CDC worker and the
    /// /search handlers. `None` when the API isn't configured for
    /// Tier-3 search — /search returns 503 SEARCH_NOT_CONFIGURED so
    /// the missing config is loud rather than silent.
    pub typesense: Option<Arc<TypesenseClient>>,
    /// Phase 6a-2: shared secret accepted at `/api/platform/audit*`.
    /// `None` => those endpoints uniformly return 401, never admit a
    /// caller. Wrapped in `Arc` because the comparison happens on every
    /// audit-endpoint request and the string itself never mutates.
    pub platform_audit_token: Option<Arc<String>>,
}

impl AppState {
    /// Build app state with default tiering wiring — hot-only,
    /// no warm reader. This matches the pre-Phase-4 behaviour and
    /// keeps every existing test and integration harness compiling
    /// without having to thread a tier router through their setup.
    /// Production wiring (main.rs) injects warm-tier separately via
    /// `with_tiering`.
    pub fn new(registry: Arc<SchemaRegistry>, pool: PgPool) -> Self {
        let hot: Arc<dyn crate::tiering::EventReader> =
            Arc::new(PostgresEventReader::new(pool.clone()));
        let tiered_reader = Arc::new(TieredEventReader::new(hot, None));
        let cold_jobs = ColdJobStore::new();
        Self {
            registry,
            pool,
            tiered_reader,
            cold_jobs,
            cursor_signer: None,
            typesense: None,
            platform_audit_token: None,
        }
    }

    /// Override the tiered reader. Used by main.rs to inject a
    /// real warm-tier impl when configured.
    pub fn with_tiering(
        mut self,
        tiered_reader: Arc<TieredEventReader>,
        cold_jobs: Arc<ColdJobStore>,
    ) -> Self {
        self.tiered_reader = tiered_reader;
        self.cold_jobs = cold_jobs;
        self
    }

    /// Inject a cursor signer. Used by main.rs when
    /// `VELOCITY_API_CURSOR_SIGNING_KEY` is configured.
    pub fn with_cursor_signer(mut self, signer: Arc<CursorSigner>) -> Self {
        self.cursor_signer = Some(signer);
        self
    }

    /// Inject a Typesense client. Used by main.rs when
    /// `VELOCITY_API_TYPESENSE_URL` is configured.
    pub fn with_typesense(mut self, ts: Arc<TypesenseClient>) -> Self {
        self.typesense = Some(ts);
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
        // The pool isn't actually connected — we just need a value to
        // satisfy AppState::new. Tests below only exercise builder
        // chaining, not DB I/O.
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://x:x@127.0.0.1:1/x")
            .unwrap()
    }

    #[tokio::test]
    async fn with_cursor_signer_attaches_signer() {
        let (registry, _) = SchemaRegistry::new();
        let signer = Arc::new(CursorSigner::new(vec![0u8; 32]).unwrap());
        let state = AppState::new(registry, empty_pool()).with_cursor_signer(signer.clone());
        assert!(state.cursor_signer.is_some());
    }

    #[tokio::test]
    async fn with_typesense_attaches_client() {
        let (registry, _) = SchemaRegistry::new();
        let ts = Arc::new(TypesenseClient::new("http://localhost:8108", "xyz").unwrap());
        let state = AppState::new(registry, empty_pool()).with_typesense(ts);
        assert!(state.typesense.is_some());
    }

    #[tokio::test]
    async fn with_platform_audit_token_attaches_token() {
        let (registry, _) = SchemaRegistry::new();
        let state = AppState::new(registry, empty_pool())
            .with_platform_audit_token(Arc::new("a-valid-token-xxxx".into()));
        assert!(state.platform_audit_token.is_some());
        assert!(state.platform_audit_token.unwrap().len() >= 16);
    }
}
