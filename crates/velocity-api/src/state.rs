//! Shared application state injected into Axum handlers.

use std::sync::Arc;

use sqlx::PgPool;

use crate::registry::SchemaRegistry;
use crate::tiering::{cold_stub::ColdJobStore, PostgresEventReader, TieredEventReader};

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
        Self { registry, pool, tiered_reader, cold_jobs }
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
}
