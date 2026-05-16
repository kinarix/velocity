//! Shared application state injected into Axum handlers.

use std::sync::Arc;

use sqlx::PgPool;

use crate::registry::SchemaRegistry;

#[derive(Debug, Clone)]
pub struct AppState {
    pub registry: Arc<SchemaRegistry>,
    pub pool: PgPool,
}

impl AppState {
    pub fn new(registry: Arc<SchemaRegistry>, pool: PgPool) -> Self {
        Self { registry, pool }
    }
}
