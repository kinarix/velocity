//! Hot-tier `EventReader` against `platform.event_log`.
//!
//! Implementation lifted verbatim from the inline SQL that lived in
//! `time_machine::at` — same projection, same ordering, same predicate
//! shape. Keeping the SQL central in this impl lets the time-machine
//! handlers stop knowing about table layout.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::event_reader::{EventReader, EventRow, TierError};

#[derive(Debug)]
pub struct PostgresEventReader {
    pool: PgPool,
}

impl PostgresEventReader {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventReader for PostgresEventReader {
    async fn events_for(
        &self,
        path: &str,
        entity_id: Uuid,
        until: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventRow>, TierError> {
        // Empty/malformed paths are caller bugs — surface as bad
        // request rather than letting them hit the DB as no-match.
        if path.is_empty() {
            return Err(TierError::BadRequest("empty schema path".into()));
        }
        let limit = limit.max(1) as i64;
        let rows: Vec<(DateTime<Utc>, String, Option<serde_json::Value>, Option<serde_json::Value>)> =
            sqlx::query_as(
                "SELECT occurred_at, operation, diff, payload \
                 FROM platform.event_log \
                 WHERE schema_org = $1 AND entity_id = $2::uuid AND occurred_at <= $3 \
                 ORDER BY occurred_at DESC \
                 LIMIT $4",
            )
            .bind(path)
            .bind(entity_id)
            .bind(until)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| TierError::Hot(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(occurred_at, operation, diff, payload)| EventRow {
                occurred_at,
                operation,
                diff,
                payload,
            })
            .collect())
    }
}
