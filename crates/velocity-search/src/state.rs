//! Application state for the search tier.
//!
//! Deliberately minimal: the search handlers need the schema registry
//! (to resolve paths + enumerate cross-search schemas), the Postgres pool
//! (for the standalone search-audit write — ADR-006/Phase 6a-1), and the
//! Typesense client. No tiering, no cursor signer, no platform token —
//! those belong to the data and platform tiers.

use std::sync::Arc;

use sqlx::PgPool;
use velocity_core::registry::SchemaRegistry;
use velocity_typesense::TypesenseClient;

#[derive(Debug, Clone)]
pub struct SearchState {
    pub registry: Arc<SchemaRegistry>,
    pub pool: PgPool,
    /// Shared Typesense client. `None` when the tier isn't configured for
    /// Tier-3 — `/search` then returns `SEARCH_NOT_CONFIGURED` so the
    /// missing config is loud rather than silent.
    pub typesense: Option<Arc<TypesenseClient>>,
}

impl SearchState {
    pub fn new(registry: Arc<SchemaRegistry>, pool: PgPool) -> Self {
        Self { registry, pool, typesense: None }
    }

    /// Inject the Typesense client. Wired by `main.rs` when
    /// `VELOCITY_API_TYPESENSE_URL`/`_KEY` are configured.
    pub fn with_typesense(mut self, ts: Arc<TypesenseClient>) -> Self {
        self.typesense = Some(ts);
        self
    }
}
