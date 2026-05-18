//! The single trait time_machine reconstruction calls against.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

/// One row from the event log, projected to the columns the
/// JSON-Patch fold actually consumes. Audit-only columns (id, actor,
/// source, request_id, reason) intentionally absent — they're not
/// required for reconstruction and the warm Parquet schema doesn't
/// carry them in the Phase 4 MVP.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub occurred_at: DateTime<Utc>,
    pub operation: String,
    pub diff: Option<serde_json::Value>,
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Error)]
pub enum TierError {
    #[error("hot tier database error: {0}")]
    Hot(String),
    #[error("warm tier unavailable: {0}")]
    WarmUnavailable(String),
    #[error("warm tier not configured for this deployment")]
    WarmNotConfigured,
    #[error("cold tier not yet supported (use the async retrieval endpoint)")]
    ColdNotSupported,
    #[error("invalid request: {0}")]
    BadRequest(String),
}

#[async_trait]
pub trait EventReader: Send + Sync {
    /// Fetch the entity's events at or before `until`, newest-first,
    /// capped at `limit`. The caller folds the returned slice into
    /// reconstructed state.
    ///
    /// `path` is the canonical `schema_org` (`org/app/domain`).
    /// Implementations validate it; callers shouldn't pre-sanitize.
    async fn events_for(
        &self,
        path: &str,
        entity_id: Uuid,
        until: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventRow>, TierError>;
}
