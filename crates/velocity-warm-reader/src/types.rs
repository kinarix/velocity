//! Wire types for the warm-reader HTTP API.
//!
//! These structs are duplicated on the API side (`velocity-api`'s
//! `tiering::warm_reader` module) — they cross a process boundary and
//! we deliberately do NOT share them via a cargo dependency to keep the
//! services independently versionable. If the contract grows, version
//! it explicitly (`/v2/warm/events`), don't rely on shared structs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `POST /v1/warm/events` request body.
///
/// `path` is `org/app/domain` (no leading or trailing slash) — the
/// canonical form used by `platform.event_log.schema_org`.
#[derive(Debug, Serialize, Deserialize)]
pub struct EventsRequest {
    pub path: String,
    pub entity_id: Uuid,
    pub until: DateTime<Utc>,
    /// Cap on rows returned. Server enforces its own ceiling
    /// (`MAX_LIMIT`) on top of this — the smaller of the two wins.
    pub limit: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EventsResponse {
    pub events: Vec<EventRow>,
    /// How many Parquet objects were consulted to answer this query.
    /// Useful for SLO dashboards (a request that fans out to many
    /// month-objects is structurally slower).
    pub objects_scanned: u32,
}

/// One event-log row, projected to the columns the API actually needs
/// for the JSON-Patch fold. We deliberately do NOT return raw payload
/// JSON blobs from non-target entities — the warm reader already
/// filtered by `(schema_org, entity_id)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub occurred_at: DateTime<Utc>,
    pub operation: String,
    pub diff: Option<serde_json::Value>,
    pub payload: Option<serde_json::Value>,
}

/// Server-side ceiling on rows returned per request. Matches
/// `SNAPSHOT_MAX_ITEMS` on the API side so the two limits don't drift.
/// A single entity's history is bounded — 10K events for one entity is
/// already pathological and worth refusing.
pub const MAX_LIMIT: u32 = 10_000;
