//! `Idempotency-Key` replay cache (CLAUDE.md › Idempotency).
//!
//! POSTs that carry an `Idempotency-Key: <token>` header are deduplicated
//! against `platform.idempotency_keys`: a second arrival with the *same*
//! key and the *same* request body hash gets the first response replayed
//! verbatim. A second arrival with the same key but a *different* body
//! hash is a programming error and returns 409 `IDEMPOTENCY_CONFLICT`.
//!
//! All idempotency reads/writes run as the `velocity_api` connection role
//! (NOT inside a `SET LOCAL ROLE <domain>` transaction). The platform
//! table is shared infrastructure; per-domain roles do not have grants on
//! it. See `migrations/0003_grants.sql`.
//!
//! Phase 1 limitations:
//!   - Lookup → work → insert is *not* atomic. Two concurrent requests with
//!     the same key both run their work; the second `INSERT` loses on the
//!     primary-key constraint and we treat that as "already cached" by
//!     re-reading the row. Functionally correct, slightly wasteful.
//!   - No row-level lock around in-flight requests; replays during the
//!     work window may return 409 instead of the eventual cached body.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

use crate::error::ApiError;

/// Maximum length of the caller-supplied `Idempotency-Key` value. The
/// table column is `TEXT` so this is a soft limit — we reject obviously
/// abusive keys early.
const MAX_KEY_LEN: usize = 256;

#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Debug)]
pub enum Lookup {
    /// First time we've seen this key.
    Miss,
    /// Same key, same body — replay the stored response.
    Replay(CachedResponse),
    /// Same key, different body — the caller made a mistake.
    Conflict,
}

/// Stable hash of the request payload — same bytes in, same hash out.
/// We hash the canonical JSON representation rather than the raw bytes so
/// whitespace / key ordering differences don't trip the cache.
pub fn hash_payload(body: &Value) -> String {
    let canonical = serde_json::to_string(body).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(digest)
}

pub fn validate_key(key: &str) -> Result<(), ApiError> {
    if key.is_empty() {
        return Err(ApiError::BadRequest("Idempotency-Key header is empty".into()));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(ApiError::BadRequest(format!(
            "Idempotency-Key header exceeds {MAX_KEY_LEN} bytes"
        )));
    }
    Ok(())
}

/// Look the key up. The lookup runs outside the per-domain role
/// transaction — see module docstring.
pub async fn lookup(pool: &PgPool, key: &str, request_hash: &str) -> Result<Lookup, ApiError> {
    let row = sqlx::query(
        "SELECT request_hash, response_body, response_code
         FROM platform.idempotency_keys
         WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(Lookup::Miss);
    };

    let stored_hash: String = row.try_get("request_hash")?;
    if stored_hash != request_hash {
        return Ok(Lookup::Conflict);
    }
    let body: Option<Value> = row.try_get("response_body")?;
    let code: i32 = row.try_get("response_code")?;
    Ok(Lookup::Replay(CachedResponse { status: code as u16, body: body.unwrap_or(Value::Null) }))
}

/// Record the response. Run after the work has committed. A unique-key
/// race loser logs at debug and returns Ok — the winning row is already
/// in place with the same body.
pub async fn record(
    pool: &PgPool,
    key: &str,
    request_hash: &str,
    response: &CachedResponse,
) -> Result<(), ApiError> {
    let result = sqlx::query(
        "INSERT INTO platform.idempotency_keys (key, request_hash, response_body, response_code)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .bind(request_hash)
    .bind(&response.body)
    .bind(response.status as i32)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        tracing::debug!(key, "idempotency record lost the race — leaving the existing row");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_is_stable_across_equivalent_json() {
        let a = hash_payload(&json!({ "a": 1, "b": 2 }));
        let b = hash_payload(&json!({ "a": 1, "b": 2 }));
        assert_eq!(a, b);
        let c = hash_payload(&json!({ "a": 1, "b": 3 }));
        assert_ne!(a, c);
    }

    #[test]
    fn key_validation() {
        assert!(validate_key("k1").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key(&"x".repeat(MAX_KEY_LEN + 1)).is_err());
    }
}
