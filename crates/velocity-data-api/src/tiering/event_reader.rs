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
    /// `path` is the canonical `schema_org` —
    /// `org/app/domain/object/version` as produced by
    /// `velocity_core::registry::registry_key`. Implementations validate it;
    /// callers shouldn't pre-sanitize.
    async fn events_for(
        &self,
        path: &str,
        entity_id: Uuid,
        until: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventRow>, TierError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_error_display_covers_every_variant() {
        // Each Display string is part of the public error envelope —
        // callers grep on these messages. A drift test catches accidental
        // renames + keeps the Display impls in coverage.
        assert!(TierError::Hot("x".into()).to_string().contains("hot tier"));
        assert!(TierError::WarmUnavailable("x".into()).to_string().contains("warm tier"));
        assert!(TierError::WarmNotConfigured.to_string().contains("not configured"));
        assert!(TierError::ColdNotSupported.to_string().contains("cold tier"));
        assert!(TierError::BadRequest("x".into()).to_string().contains("invalid request"));
    }

    #[test]
    fn event_row_clone_preserves_fields() {
        let row = EventRow {
            occurred_at: Utc::now(),
            operation: "create".into(),
            diff: Some(serde_json::json!({ "a": 1 })),
            payload: Some(serde_json::json!({ "b": 2 })),
        };
        let cloned = row.clone();
        assert_eq!(cloned.operation, row.operation);
        assert_eq!(cloned.diff, row.diff);
        assert_eq!(cloned.payload, row.payload);
    }
}
