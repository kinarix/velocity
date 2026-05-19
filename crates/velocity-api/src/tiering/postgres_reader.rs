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

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_pool() -> PgPool {
        // Lazy pool — never opens a TCP connection. Sufficient to
        // construct the reader and exercise pre-DB validation. Use
        // sqlx::pool::PoolOptions to override the default 30-second
        // connect timeout so unreachable-DB tests fail fast.
        use sqlx::postgres::PgConnectOptions;
        use sqlx::pool::PoolOptions;
        use std::str::FromStr;
        use std::time::Duration;
        let opts = PgConnectOptions::from_str("postgres://stub:stub@127.0.0.1:1/stub").unwrap();
        PoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy_with(opts)
    }

    #[tokio::test]
    async fn empty_path_returns_bad_request_without_touching_db() {
        let reader = PostgresEventReader::new(lazy_pool());
        let err = reader.events_for("", Uuid::nil(), Utc::now(), 10).await.unwrap_err();
        assert!(matches!(err, TierError::BadRequest(_)));
    }

    #[tokio::test]
    async fn unreachable_db_maps_to_hot_tier_error() {
        // Driving past the empty-path guard with a lazy pool that points
        // at an unbound port forces the sqlx call to fail, which exercises
        // the `map_err(|e| TierError::Hot(...))` branch.
        let reader = PostgresEventReader::new(lazy_pool());
        let err = reader.events_for("a/b/c", Uuid::nil(), Utc::now(), 10).await.unwrap_err();
        assert!(matches!(err, TierError::Hot(_)), "expected Hot, got {err:?}");
    }

    #[tokio::test]
    async fn zero_limit_is_normalized_to_one() {
        // Covers the `limit.max(1)` branch. We can't observe the bound
        // directly without a real DB; the smoke-test is that the call
        // doesn't panic on limit=0.
        let reader = PostgresEventReader::new(lazy_pool());
        let _ = reader.events_for("a/b/c", Uuid::nil(), Utc::now(), 0).await;
    }

    #[tokio::test]
    async fn postgres_event_reader_debug_does_not_expose_pool_secrets() {
        let reader = PostgresEventReader::new(lazy_pool());
        let dbg = format!("{reader:?}");
        assert!(dbg.contains("PostgresEventReader"));
        assert!(!dbg.contains("stub:stub"), "credentials leaked into Debug: {dbg}");
    }
}
