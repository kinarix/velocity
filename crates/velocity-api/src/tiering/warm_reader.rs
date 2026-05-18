//! Warm-tier `EventReader` — HTTP client to `velocity-warm-reader`.
//!
//! Mirrors `velocity-warm-reader::types` on the wire but deliberately
//! does NOT depend on that crate. Service-to-service contracts cross a
//! process boundary; coupling them at the type-system level would make
//! independent versioning harder. If the wire format changes, version
//! the URL (`/v2/warm/events`) rather than the struct.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::event_reader::{EventReader, EventRow, TierError};

pub struct WarmEventReader {
    client: reqwest::Client,
    base_url: String,
    bearer: String,
}

impl std::fmt::Debug for WarmEventReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WarmEventReader")
            .field("base_url", &self.base_url)
            .field("bearer", &"<redacted>")
            .finish()
    }
}

impl WarmEventReader {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>, timeout: Duration) -> Result<Self, TierError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| TierError::WarmUnavailable(format!("client build: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            bearer: bearer.into(),
        })
    }
}

#[derive(Debug, Serialize)]
struct EventsRequestWire<'a> {
    path: &'a str,
    entity_id: Uuid,
    until: DateTime<Utc>,
    limit: u32,
}

#[derive(Debug, Deserialize)]
struct EventsResponseWire {
    events: Vec<EventRowWire>,
    #[serde(default)]
    #[allow(dead_code)]
    objects_scanned: u32,
}

#[derive(Debug, Deserialize)]
struct EventRowWire {
    occurred_at: DateTime<Utc>,
    operation: String,
    diff: Option<serde_json::Value>,
    payload: Option<serde_json::Value>,
}

#[async_trait]
impl EventReader for WarmEventReader {
    async fn events_for(
        &self,
        path: &str,
        entity_id: Uuid,
        until: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventRow>, TierError> {
        let url = format!("{}/v1/warm/events", self.base_url.trim_end_matches('/'));
        let req = EventsRequestWire { path, entity_id, until, limit };
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.bearer)
            .json(&req)
            .send()
            .await
            .map_err(|e| TierError::WarmUnavailable(format!("send {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            // Try to parse the structured error envelope; fall back to raw.
            let body = resp.text().await.unwrap_or_default();
            return Err(TierError::WarmUnavailable(format!(
                "warm-reader returned {}: {}",
                status, truncate(&body, 500)
            )));
        }

        let body: EventsResponseWire = resp
            .json()
            .await
            .map_err(|e| TierError::WarmUnavailable(format!("decode {url}: {e}")))?;
        Ok(body
            .events
            .into_iter()
            .map(|r| EventRow {
                occurred_at: r.occurred_at,
                operation: r.operation,
                diff: r.diff,
                payload: r.payload,
            })
            .collect())
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        // Hopefully on a char boundary; if not, walk back.
        let mut cut = n;
        while !s.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        &s[..cut]
    }
}
