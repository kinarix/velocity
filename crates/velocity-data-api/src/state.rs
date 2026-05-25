//! Application state for the data plane.
//!
//! Carries the schema registry, the Postgres pool, the tier-routing
//! event reader (time-machine reads), and the optional `/query` cursor
//! signer. No platform-audit token (that's the platform tier), no
//! Typesense (that's the search tier).

use std::sync::Arc;

use sqlx::PgPool;
use velocity_core::registry::SchemaRegistry;
use velocity_core::CursorSigner;

use crate::tiering::{cold_stub::ColdJobStore, PostgresEventReader, TieredEventReader};

#[derive(Debug, Clone)]
pub struct DataState {
    pub registry: Arc<SchemaRegistry>,
    pub pool: PgPool,
    /// Tier-routing event reader for time-machine reads. Always present;
    /// the warm impl is `None` until `with_tiering` injects one.
    pub tiered_reader: Arc<TieredEventReader>,
    pub cold_jobs: Arc<ColdJobStore>,
    /// Phase 5: HMAC signer for POST /query cursors. `None` when
    /// `VELOCITY_API_CURSOR_SIGNING_KEY` is unset — pagination still works
    /// for the first page, but cursor-bearing requests fail with a clear
    /// 400 instead of being silently misinterpreted.
    pub cursor_signer: Option<Arc<CursorSigner>>,
}

impl DataState {
    /// Build state with default tiering wiring — hot-only, no warm reader.
    /// Production wiring (`main.rs`) injects warm-tier separately via
    /// [`Self::with_tiering`].
    pub fn new(registry: Arc<SchemaRegistry>, pool: PgPool) -> Self {
        let hot: Arc<dyn crate::tiering::EventReader> =
            Arc::new(PostgresEventReader::new(pool.clone()));
        let tiered_reader = Arc::new(TieredEventReader::new(hot, None));
        let cold_jobs = ColdJobStore::new();
        Self { registry, pool, tiered_reader, cold_jobs, cursor_signer: None }
    }

    /// Override the tiered reader. Used by `main.rs` to inject a real
    /// warm-tier impl when configured.
    pub fn with_tiering(
        mut self,
        tiered_reader: Arc<TieredEventReader>,
        cold_jobs: Arc<ColdJobStore>,
    ) -> Self {
        self.tiered_reader = tiered_reader;
        self.cold_jobs = cold_jobs;
        self
    }

    /// Inject a cursor signer. Used by `main.rs` when
    /// `VELOCITY_API_CURSOR_SIGNING_KEY` is configured.
    pub fn with_cursor_signer(mut self, signer: Arc<CursorSigner>) -> Self {
        self.cursor_signer = Some(signer);
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
        let state = DataState::new(registry, empty_pool()).with_cursor_signer(signer.clone());
        assert!(state.cursor_signer.is_some());
    }
}
