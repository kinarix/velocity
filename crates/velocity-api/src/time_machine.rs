//! Phase 3 — Time Machine HTTP handlers. Implements the read-only views
//! over `platform.event_log` that let callers see entity history,
//! reconstruct point-in-time state, and diff between two timestamps. The
//! restore endpoint (a *write* against the underlying table) lives here
//! too because it shares the event-log query shape, even though it
//! ultimately produces a new event rather than mutating history.
//!
//! ## Access layers
//!
//! All routes are gated by Layer-1 RBAC (`op::READ`, or `op::RESTORE` on
//! POST /restore). Layers 4–6 are applied here too, but adapted to the
//! event-log shape:
//!
//! - **Layer 4 (row filter)**: `platform.event_log` has no RLS, so the
//!   gate is per-entity in app code. For each entity surface the
//!   handler emits, we fetch the *latest non-delete payload* and
//!   evaluate the actor's `rowFilter[]` against it
//!   ([`crate::row_filter::payload_visible`]). One decision per
//!   entity, applies to its full history — so an actor entitled to
//!   the entity sees all of its events including pre-visibility
//!   transitions, and an actor who isn't entitled gets 404 (or has the
//!   entity skipped in cross-entity snapshots).
//!
//! - **Layer 5 (field filter)**: every `payload` emitted runs through
//!   [`crate::field_filter::FieldFilterIndex::strip_for_read`]; every
//!   `diff` runs through
//!   [`crate::field_filter::FieldFilterIndex::strip_diff_for_read`]
//!   so a JSON-Patch op against a stripped field can't leak the value.
//!
//! - **Layer 6 (masking)**: applied to every payload right after the
//!   strip, same order as the live GET handler enforces.

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;

use crate::error::ApiError;
use crate::handlers::{identity_from_ext, resolve_schema, SchemaPathParts};
use crate::rbac::{self, op};
use crate::registry::registry_key;
use crate::state::AppState;
use crate::tiering::EventReader;
use crate::Identity;

/// Fetch the latest non-delete event payload for `(schema_org, entity_uuid)`
/// from `platform.event_log`. Used as the per-entity row-filter gate: an
/// actor whose `rowFilter[]` doesn't admit this payload doesn't see the
/// entity in *any* time-machine endpoint, regardless of the requested T.
///
/// Returns `Ok(None)` when no non-delete event exists (entity never
/// created, or all writes have been compacted out — the latter is a
/// theoretical future concern). Callers treat `None` as "deny" since
/// there's no payload to gate against.
async fn latest_non_delete_payload(
    pool: &sqlx::PgPool,
    schema_org: &str,
    entity_uuid: uuid::Uuid,
) -> Result<Option<Value>, ApiError> {
    let row: Option<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT payload FROM platform.event_log \
         WHERE schema_org = $1 AND entity_id = $2 AND operation <> 'delete' \
         ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(schema_org)
    .bind(entity_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_pg_err)?;
    Ok(row.and_then(|r| r.get::<Option<Value>, _>("payload")))
}

/// Per-entity row-filter gate. Returns `Ok(())` if the actor is entitled
/// to the entity, `Err(ApiError::NotFound)` otherwise. A broken
/// rowFilter on the schema surfaces as `ApiError::Internal` rather
/// than a silent deny.
async fn ensure_entity_visible(
    pool: &sqlx::PgPool,
    schema: &crate::registry::ResolvedSchema,
    schema_org: &str,
    entity_uuid: uuid::Uuid,
    identity: &Identity,
) -> Result<(), ApiError> {
    // Empty rowFilter ⇒ every actor sees every entity; skip the
    // round-trip entirely.
    if schema.row_filter.is_empty() {
        return Ok(());
    }
    let Some(payload) = latest_non_delete_payload(pool, schema_org, entity_uuid).await? else {
        return Err(ApiError::NotFound);
    };
    if !crate::row_filter::payload_visible(schema, identity, &payload)? {
        return Err(ApiError::NotFound);
    }
    Ok(())
}

/// Apply the per-reader strip + mask pipeline to a payload in place.
/// Same order the GET / LIST handlers use: strip first (so a stripped
/// field has nothing to mask), then mask.
fn strip_mask_payload(
    schema: &crate::registry::ResolvedSchema,
    identity: &Identity,
    payload: &mut Value,
) {
    schema.field_filter.strip_for_read(payload, &identity.roles);
    schema.masking.apply_for_read(payload, &identity.roles);
}

/// Apply the per-reader diff strip in place. Removes JSON-Patch ops on
/// fields the caller cannot read. No-op if `diff` is `None` or not an
/// array. Diffs aren't masked — a `replace` op carrying a sensitive
/// value would also be visible on a plain GET, so the field strip is
/// the right gate.
fn strip_diff(
    schema: &crate::registry::ResolvedSchema,
    identity: &Identity,
    diff: &mut Option<Value>,
) {
    if let Some(d) = diff {
        schema.field_filter.strip_diff_for_read(d, &identity.roles);
    }
}

/// Path tuple for routes that include an entity id, e.g.
/// `/api/{org}/{app}/{domain}/{object}/{version}/{id}/history`.
pub(crate) type EntityPathParts = (String, String, String, String, String, String);

/// Split an entity path into the schema parts + id so the existing
/// `resolve_schema` helper can be reused unchanged.
fn split_entity(parts: EntityPathParts) -> (SchemaPathParts, String) {
    let (org, app, domain, object, version, id) = parts;
    ((org, app, domain, object, version), id)
}

/// Query parameters for `GET /{id}/history`.
///
/// Two modes:
/// 1. **List mode** (no `?at`): paginated event listing, newest-first.
///    `?limit=N` (default 50, max 1000), `?before=<ISO8601>` (exclusive
///    cursor) shape the page.
/// 2. **Point-in-time mode** (`?at=<ISO8601>`): returns the reconstructed
///    entity state as of T, or 404 if the entity didn't exist (or was
///    deleted) at T. `limit` / `before` are ignored in this mode.
#[derive(Debug, Deserialize, Default)]
pub struct HistoryQuery {
    pub limit: Option<u32>,
    pub before: Option<DateTime<Utc>>,
    pub at: Option<DateTime<Utc>>,
}

/// One row in the history listing — projection of `platform.event_log`
/// with the columns a UI / SDK actually wants. `payload` and `diff` are
/// emitted as raw JSON; consumers that want only metadata can request the
/// projection later (omitted for v1 — premature feature).
#[derive(Debug, Serialize)]
pub struct HistoryEvent {
    pub id: String,
    pub occurred_at: DateTime<Utc>,
    pub operation: String,
    pub source: String,
    pub actor: String,
    pub request_id: Option<String>,
    pub diff: Option<Value>,
    pub payload: Option<Value>,
}

/// Default `?limit` when none specified. Matched to CLAUDE.md §QueryBuilder
/// invariant #9 (max 1000 without cursor). 50 is the same default the
/// audit endpoint uses, so SDKs can share pagination logic.
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 1000;

/// `GET /api/{org}/{app}/{domain}/{object}/{version}/{id}/history`
/// Paginated event-log listing for a single entity, newest-first.
///
/// Filters:
/// - `?limit=N` (default 50, max 1000)
/// - `?before=<ISO8601>` (exclusive; returns events strictly older)
///
/// Returns `{ "items": [HistoryEvent...] }`. Empty array is a valid
/// response — the entity may exist but have only one event (the create),
/// in which case the second page is empty.
pub async fn history(
    State(state): State<AppState>,
    _decision_ext: Option<Extension<crate::auth::AuthDecision>>,
    identity: Option<Extension<Identity>>,
    Path(parts): Path<EntityPathParts>,
    Query(q): Query<HistoryQuery>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (schema_parts, entity_id) = split_entity(parts);
    let schema = resolve_schema(&state, schema_parts)?;
    let identity = identity_from_ext(identity);
    rbac::check_access(&schema, &identity, op::READ)?;

    let schema_org = registry_key(&schema.path);
    let entity_uuid = uuid::Uuid::parse_str(&entity_id)
        .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;

    // Layer-4: per-entity gate. A scoped reader who isn't entitled to
    // the entity sees the same 404 they'd get for a never-existed id —
    // no listing, no point-in-time, no replay. Empty rowFilter
    // short-circuits the round-trip.
    ensure_entity_visible(&state.pool, &schema, &schema_org, entity_uuid, &identity).await?;

    // Point-in-time mode: branch BEFORE pagination so a caller sending
    // `?at=&limit=` doesn't get a confusing combo of both. `limit` and
    // `before` are silently ignored when `at` is present — documented on
    // `HistoryQuery`.
    if let Some(at) = q.at {
        return point_in_time(&state, &schema, &identity, &schema_org, &entity_id, at).await;
    }

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as i64;

    // Build the SQL with a static WHERE — `before` is bound, not
    // interpolated, so even though the column is a partition key the
    // planner still gets to prune.
    let rows = match q.before {
        Some(before) => sqlx::query(
            "SELECT id, occurred_at, operation, source, actor, request_id, diff, payload \
             FROM platform.event_log \
             WHERE schema_org = $1 AND entity_id = $2::uuid AND occurred_at < $3 \
             ORDER BY occurred_at DESC \
             LIMIT $4",
        )
        .bind(&schema_org)
        .bind(&entity_id)
        .bind(before)
        .bind(limit)
        .fetch_all(&state.pool)
        .await
        .map_err(map_pg_err)?,
        None => sqlx::query(
            "SELECT id, occurred_at, operation, source, actor, request_id, diff, payload \
             FROM platform.event_log \
             WHERE schema_org = $1 AND entity_id = $2::uuid \
             ORDER BY occurred_at DESC \
             LIMIT $3",
        )
        .bind(&schema_org)
        .bind(&entity_id)
        .bind(limit)
        .fetch_all(&state.pool)
        .await
        .map_err(map_pg_err)?,
    };

    let items: Vec<HistoryEvent> = rows
        .into_iter()
        .map(|r| {
            // Per-event strip + mask. The stored payload/diff are the
            // raw post-SQL row (the write path doesn't pre-strip because
            // event_log is the authoritative audit stream); we apply the
            // per-reader transforms here so a restricted role doesn't
            // see fields they can't read on GET via the history channel.
            let mut payload: Option<Value> = r.get("payload");
            if let Some(p) = payload.as_mut() {
                strip_mask_payload(&schema, &identity, p);
            }
            let mut diff: Option<Value> = r.get("diff");
            strip_diff(&schema, &identity, &mut diff);
            HistoryEvent {
                id: r.get::<uuid::Uuid, _>("id").to_string(),
                occurred_at: r.get("occurred_at"),
                operation: r.get("operation"),
                source: r.get("source"),
                actor: r.get("actor"),
                request_id: r.get("request_id"),
                diff,
                payload,
            }
        })
        .collect();

    Ok((StatusCode::OK, Json(json!({ "items": items }))))
}

/// `?from=<ISO8601>&to=<ISO8601>` for the diff endpoint. Both are
/// required; the handler 400s if either is missing.
#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// `GET /api/{org}/{app}/{domain}/{object}/{version}/{id}/diff?from=T1&to=T2`
///
/// Returns the JSON-Patch (RFC 6902) shape describing how to transform
/// state-at-T1 into state-at-T2. Symmetric to point-in-time: each side
/// is independently looked up via the same payload-walking strategy as
/// `point_in_time`, then `event_log::diff` produces the patch.
///
/// Error cases:
/// - `from > to` → 400 (the diff semantics depend on direction; we
///   refuse to "guess" by swapping)
/// - entity didn't exist at T1 OR T2 → 404 (a diff is undefined if either
///   side is missing; clients can ask `?at=` for each timestamp
///   separately to distinguish "didn't exist" from "no change")
pub async fn diff_endpoint(
    State(state): State<AppState>,
    _decision_ext: Option<Extension<crate::auth::AuthDecision>>,
    identity: Option<Extension<Identity>>,
    Path(parts): Path<EntityPathParts>,
    Query(q): Query<DiffQuery>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if q.from > q.to {
        return Err(ApiError::BadRequest("`from` must be earlier than or equal to `to`".into()));
    }

    let (schema_parts, entity_id) = split_entity(parts);
    let schema = resolve_schema(&state, schema_parts)?;
    let identity = identity_from_ext(identity);
    rbac::check_access(&schema, &identity, op::READ)?;
    let schema_org = registry_key(&schema.path);
    let entity_uuid = uuid::Uuid::parse_str(&entity_id)
        .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;

    // Layer-4: per-entity gate against the latest non-delete payload.
    // A scoped reader who isn't entitled to the entity gets 404 — same
    // 404 the listing path returns.
    ensure_entity_visible(&state.pool, &schema, &schema_org, entity_uuid, &identity).await?;

    // Walk to each timestamp's payload. `state_at` returns the raw
    // event-log payload (so the restore write path can use it); we
    // strip + mask each side here BEFORE feeding `event_log::diff` so
    // the JSON-Patch shape can't leak a field a plain GET would have
    // stripped. Stripping identically on both sides keeps the patch
    // well-formed — a field removed from before-and-after produces no
    // op, not a spurious replace.
    let mut before = state_at(&state, &schema_org, &entity_id, q.from).await?;
    let mut after = state_at(&state, &schema_org, &entity_id, q.to).await?;
    strip_mask_payload(&schema, &identity, &mut before);
    strip_mask_payload(&schema, &identity, &mut after);

    let patch = crate::event_log::diff(&before, &after);
    Ok((StatusCode::OK, Json(patch)))
}

/// Helper for diff: returns the entity's state at T, or `ApiError::NotFound`
/// if the entity didn't exist or was deleted at T. Mirrors `point_in_time`
/// but returns the raw payload value so the caller can compose with
/// other states.
/// Reconstruct an entity's state at T. Returns the RAW payload exactly
/// as it was stored in `platform.event_log` — no field strip, no mask.
/// Callers that hand the value back to a reader MUST run it through
/// [`strip_mask_payload`] first; the `restore` write path uses the raw
/// shape so it can rewrite stripped fields into the live row.
async fn state_at(
    state: &AppState,
    schema_org: &str,
    entity_id: &str,
    at: DateTime<Utc>,
) -> Result<Value, ApiError> {
    let entity_uuid = uuid::Uuid::parse_str(entity_id)
        .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;
    let events = state
        .tiered_reader
        .events_for(schema_org, entity_uuid, at, 1)
        .await
        .map_err(tier_err_to_api)?;
    let latest = events.into_iter().next().ok_or(ApiError::NotFound)?;
    if latest.operation == "delete" {
        return Err(ApiError::NotFound);
    }
    latest.payload.ok_or_else(|| ApiError::Internal("history reconstruction failed".into()))
}

/// Translate tiered-reader errors into API errors. Crucially, this is
/// the ONE place tier failures get a status code — keeps the
/// fail-closed default (ADR-003) honest: we never paper over a warm
/// outage by returning hot results.
fn tier_err_to_api(e: crate::tiering::TierError) -> ApiError {
    use crate::tiering::TierError;
    match e {
        // Hot-tier errors arrive as strings from the trait boundary
        // (we don't re-wrap sqlx::Error through the trait), so they
        // become `Internal` to preserve the message.
        TierError::Hot(msg) => ApiError::Internal(format!("hot tier read: {msg}")),
        TierError::WarmUnavailable(msg) => ApiError::WarmTierUnavailable(msg),
        TierError::WarmNotConfigured => ApiError::WarmTierNotConfigured,
        TierError::ColdNotSupported => ApiError::RestoreTierUnsupported,
        TierError::BadRequest(msg) => ApiError::BadRequest(msg),
    }
}

/// Point-in-time reconstruction. The simplest correct algorithm: find
/// the most recent event at-or-before T. If it's a CREATE/UPDATE/RESTORE,
/// its `payload` IS the state at T (each write event stores the full
/// post-image, not just the diff). If it's a DELETE, the entity was
/// tombstoned by T → 404. If no event exists at-or-before T, the entity
/// didn't exist yet → also 404.
///
/// Why payload-walking instead of patch-replay: each write event already
/// carries the full payload, so a single SELECT-with-LIMIT-1 reconstructs
/// the state without applying any patches. Replay-from-patches would only
/// be needed if we stored deltas instead of payloads, which we explicitly
/// chose not to (see `event_log::write` — payload is `Some(...)` on
/// create/update/restore).
async fn point_in_time(
    state: &AppState,
    schema: &crate::registry::ResolvedSchema,
    identity: &Identity,
    schema_org: &str,
    entity_id: &str,
    at: DateTime<Utc>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // Phase 4.7 — if `at` lands in cold-tier, short-circuit before
    // touching any reader: open a cold job, return 202 + job_id. Real
    // Glacier retrieval is deferred; this just lets clients receive a
    // stable ID and demonstrates the async contract.
    if matches!(state.tiered_reader.classify(at), crate::tiering::Tier::Cold) {
        let entity_uuid = uuid::Uuid::parse_str(entity_id)
            .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;
        let job = state.cold_jobs.create(schema_org.to_string(), entity_uuid, at);
        let body = serde_json::json!({
            "code": "TIME_MACHINE_COLD_RETRIEVAL_ACCEPTED",
            "tier": "cold",
            "job_id": job.id,
            "created_at": job.created_at,
            "message": "cold-tier retrieval is async; poll job_id later (Glacier integration pending)",
        });
        return Ok((StatusCode::ACCEPTED, Json(body)));
    }

    // Hot or warm — both go through the tier router, which uses the
    // shared JSON-Patch fold above (or, in the MVP, takes the latest
    // payload via LIMIT 1). We ask for limit=1 because the at-endpoint
    // returns last-write-at-or-before semantics — the older history
    // is not needed for this reconstruction.
    let entity_uuid = uuid::Uuid::parse_str(entity_id)
        .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;
    let events = state
        .tiered_reader
        .events_for(schema_org, entity_uuid, at, 1)
        .await
        .map_err(tier_err_to_api)?;
    let latest = events.into_iter().next().ok_or(ApiError::NotFound)?;

    if latest.operation == "delete" {
        // Entity was deleted at-or-before T → not visible from T's
        // perspective. 404 mirrors what GET /{id} returns for a
        // soft-deleted entity today.
        return Err(ApiError::NotFound);
    }
    let Some(mut payload) = latest.payload else {
        // create/update/restore *should* always carry a payload — if
        // not, either the event_log writer is broken (hot tier) or
        // the warm exporter dropped a column (warm tier). Either way,
        // surface as 500 rather than hand the client an empty object.
        tracing::error!(
            schema_org = schema_org,
            entity_id = entity_id,
            operation = %latest.operation,
            "write event with NULL payload — event_log writer regression",
        );
        return Err(ApiError::Internal("history reconstruction failed".into()));
    };

    strip_mask_payload(schema, identity, &mut payload);
    Ok((StatusCode::OK, Json(payload)))
}

/// `GET /api/{org}/{app}/{domain}/{object}/{version}/{id}/replay`
///
/// Server-sent events stream of every historical event for one entity,
/// oldest-first. Emits one frame per event, then closes. (Live tailing of
/// future events — LISTEN/NOTIFY hook — is deferred; the client can
/// re-open the stream to pick up new events, and the `Last-Event-ID`
/// header support means it can resume without dupes.)
///
/// Each SSE frame:
/// - `event: <operation>` (`create` / `update` / `delete` / `restore`)
/// - `id: <event-id>` so a reconnecting client can pass it back as
///   `Last-Event-ID`
/// - `data: <HistoryEvent as JSON>` — same shape the /history endpoint
///   returns, so consumers can share a parser between the two
///
/// Why one-shot (no live tail in v1): adding a real-time tail needs
/// `LISTEN/NOTIFY` plumbing in the connection layer and a per-stream
/// channel — meaningful infra. The vast majority of replay consumers
/// (UI timeline, debug tooling, archival exports) only need the historical
/// stream. Live tail is a follow-up.
pub async fn replay(
    State(state): State<AppState>,
    _decision_ext: Option<Extension<crate::auth::AuthDecision>>,
    identity: Option<Extension<Identity>>,
    headers: axum::http::HeaderMap,
    Path(parts): Path<EntityPathParts>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (schema_parts, entity_id) = split_entity(parts);
    let schema = resolve_schema(&state, schema_parts)?;
    let identity = identity_from_ext(identity);
    rbac::check_access(&schema, &identity, op::READ)?;
    let schema_org = registry_key(&schema.path);
    let entity_uuid = uuid::Uuid::parse_str(&entity_id)
        .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;

    // Layer-4: per-entity gate. SSE replay reveals one event per frame;
    // gating once at the start means a scoped reader who isn't entitled
    // gets a clean 404 instead of an empty stream. The strip+mask still
    // runs per frame in case the payload contains gated fields.
    ensure_entity_visible(&state.pool, &schema, &schema_org, entity_uuid, &identity).await?;

    // `Last-Event-ID` (per EventSource spec) — the value MUST be the `id`
    // we emitted on the last frame the client received. We resume by
    // looking up that event's `occurred_at` and filtering rows strictly
    // after it. If the client lies (an id that doesn't exist in this
    // entity's history), we silently fall back to a full replay rather
    // than 400 — the worst case is the client sees old events again.
    let after_occurred_at: Option<DateTime<Utc>> =
        if let Some(last_id) = headers.get("last-event-id").and_then(|v| v.to_str().ok()) {
            sqlx::query("SELECT occurred_at FROM platform.event_log WHERE id = $1::uuid")
                .bind(last_id)
                .fetch_optional(&state.pool)
                .await
                .map_err(map_pg_err)?
                .map(|r| r.get::<DateTime<Utc>, _>("occurred_at"))
        } else {
            None
        };

    // Pull all rows up-front. For an entity with a 5-year history this
    // could be tens of thousands of rows; the streaming response holds
    // memory until the client drains it, so a future iteration should
    // chunk the SELECT. For Phase 3 v1 the simplicity wins.
    let rows = match after_occurred_at {
        Some(after) => sqlx::query(
            "SELECT id, occurred_at, operation, source, actor, request_id, diff, payload \
             FROM platform.event_log \
             WHERE schema_org = $1 AND entity_id = $2::uuid AND occurred_at > $3 \
             ORDER BY occurred_at ASC",
        )
        .bind(&schema_org)
        .bind(&entity_id)
        .bind(after)
        .fetch_all(&state.pool)
        .await
        .map_err(map_pg_err)?,
        None => sqlx::query(
            "SELECT id, occurred_at, operation, source, actor, request_id, diff, payload \
             FROM platform.event_log \
             WHERE schema_org = $1 AND entity_id = $2::uuid \
             ORDER BY occurred_at ASC",
        )
        .bind(&schema_org)
        .bind(&entity_id)
        .fetch_all(&state.pool)
        .await
        .map_err(map_pg_err)?,
    };

    let events: Vec<Result<Event, Infallible>> = rows
        .into_iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            // Strip+mask payload, strip diff — same per-reader transforms
            // the /history endpoint applies. Without these, an SSE
            // consumer could read a field via the stream that a plain
            // GET would have stripped.
            let mut payload: Option<Value> = r.get("payload");
            if let Some(p) = payload.as_mut() {
                strip_mask_payload(&schema, &identity, p);
            }
            let mut diff: Option<Value> = r.get("diff");
            strip_diff(&schema, &identity, &mut diff);
            let event = HistoryEvent {
                id: id.to_string(),
                occurred_at: r.get("occurred_at"),
                operation: r.get("operation"),
                source: r.get("source"),
                actor: r.get("actor"),
                request_id: r.get("request_id"),
                diff,
                payload,
            };
            let frame = Event::default()
                .id(event.id.clone())
                .event(event.operation.clone())
                .json_data(&event)
                .unwrap_or_else(|_| Event::default().event("error").data("frame-serialise-failed"));
            Ok(frame)
        })
        .collect();

    Ok(Sse::new(stream::iter(events)).keep_alive(KeepAlive::default()))
}

/// Body for `POST /{id}/restore`. Both fields optional in the body
/// itself — the handler also accepts `X-Reason` and falls back to that
/// when the JSON field is missing.
#[derive(Debug, Deserialize)]
pub struct RestoreBody {
    pub at: DateTime<Utc>,
    pub reason: Option<String>,
}

/// `POST /api/{org}/{app}/{domain}/{object}/{version}/{id}/restore`
///
/// Restore is **forward-projecting**, not rewind. The endpoint reads the
/// entity's state at T, writes a NEW event applying that state, and the
/// version increments — the history between T-and-now is preserved
/// verbatim. There is no "undo" semantic: a later /history call shows the
/// restore as its own entry, not a deletion of the intervening events.
///
/// Body: `{ "at": "2026-04-01T00:00:00Z", "reason": "rolled back per INC-123" }`
/// `X-Reason` header is honoured as a fallback when `reason` is missing
/// from the body; the body wins if both are present (per-request precision
/// beats header copy-paste).
///
/// Error shape:
/// - 400 — body fails to parse / `at` in the future
/// - 404 — no event for this entity at-or-before T
/// - 409 — target state matches current state (`RESTORE_NO_OP`)
/// - 412 — entity has been hard-deleted since (no current row to UPDATE)
pub async fn restore(
    State(state): State<AppState>,
    _decision_ext: Option<Extension<crate::auth::AuthDecision>>,
    identity: Option<Extension<Identity>>,
    headers: axum::http::HeaderMap,
    Path(parts): Path<EntityPathParts>,
    Json(body): Json<RestoreBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.at > Utc::now() {
        return Err(ApiError::BadRequest("`at` must not be in the future".into()));
    }

    // Phase 4.6 — restore writes are hot-only. Reconstructing state
    // from warm tier is straightforward (it'd work via the tier
    // router), but the *write* path needs the audit chain, RLS row
    // visibility, and atomicity with `event_log::write`, all of which
    // are hot-tier-only today. Restore-from-warm would either run
    // against a stale snapshot of the warm Parquet or require a
    // pull-warm-event-into-hot step we haven't designed. Surface as
    // 422 RESTORE_TIER_UNSUPPORTED rather than 404 or 500 so clients
    // know the data exists, it just can't be restored from this tier.
    if !matches!(state.tiered_reader.classify(body.at), crate::tiering::Tier::Hot) {
        return Err(ApiError::RestoreTierUnsupported);
    }

    let (schema_parts, entity_id) = split_entity(parts);
    let schema = resolve_schema(&state, schema_parts)?;
    let identity = identity_from_ext(identity);
    rbac::check_access(&schema, &identity, op::RESTORE)?;
    let schema_org = registry_key(&schema.path);
    let entity_uuid = uuid::Uuid::parse_str(&entity_id)
        .map_err(|_| ApiError::BadRequest("entity_id must be a UUID".into()))?;

    // Layer-4: per-entity gate. Restore needs `op::RESTORE` (Layer-1)
    // AND row-filter entitlement on the entity. An actor with restore
    // capability scoped to `region=west` must not be able to restore an
    // entity that now lives in `region=east` — the SQL UPDATE would
    // also fail under RLS, but failing here gives the caller a clean
    // 404 instead of a misleading "currently deleted" message.
    ensure_entity_visible(&state.pool, &schema, &schema_org, entity_uuid, &identity).await?;

    // Resolve reason: body field wins; X-Reason is the fallback. Both
    // empty is fine — restore is allowed without a reason, the column is
    // nullable. Length cap matches a single audit-log entry's text column
    // so a runaway client can't fill the row with multi-MB free text.
    let reason: Option<String> = body
        .reason
        .or_else(|| headers.get("x-reason").and_then(|v| v.to_str().ok()).map(str::to_string))
        .map(|s| {
            const MAX_REASON_LEN: usize = 2_000;
            s.chars().take(MAX_REASON_LEN).collect::<String>()
        });

    // Pull the target state. 404 if it never existed; 404 if it was
    // deleted by T.
    let target = state_at(&state, &schema_org, &entity_id, body.at).await?;

    // Read the current row so we can (a) check no-op, (b) get the
    // version for optimistic locking on the rewrite UPDATE. Done inside
    // `with_session_context` so RLS's `app.current_user` is populated
    // — without that setting, NOBYPASSRLS connections see zero rows and
    // the SELECT would always return None, which would 400 every restore
    // call with the misleading "currently deleted" message. The Writer
    // role is correct here: restore is a write op, and the policy
    // attached to writers is what should decide whether this actor can
    // see the row they're about to mutate.
    let table = schema.pg_qualified.clone();
    let entity_id_for_read = entity_id.clone();
    let current_row: Option<Value> = crate::session::with_session_context(
        &state.pool,
        &schema,
        crate::session::RoleClass::Writer,
        &identity,
        move |tx| {
            Box::pin(async move {
                let sql = format!(
                    "SELECT (to_jsonb({table}.*) - '__fts') AS row FROM {table} \
                     WHERE id = $1::uuid AND deleted_at IS NULL"
                );
                let row: Option<sqlx::postgres::PgRow> =
                    sqlx::query(&sql).bind(&entity_id_for_read).fetch_optional(&mut **tx).await?;
                Ok(row.map(|r| r.get::<Value, _>("row")))
            })
        },
    )
    .await
    .map_err(map_pg_err)?;

    let Some(current) = current_row else {
        // Entity was hard-deleted OR the row is soft-deleted but the
        // target state is not deleted (i.e. the actor is trying to
        // resurrect). Soft-delete restore is a real and valuable case;
        // hard-delete (DROP TABLE etc.) is irrecoverable. We don't
        // distinguish here yet — both come back as "row missing." Phase
        // 4.5 (ops tooling) will add the resurrection path.
        return Err(ApiError::BadRequest(
            "entity is currently deleted; restore-from-deleted is not yet supported".into(),
        ));
    };

    // No-op detection. We diff the user-visible field projection (target
    // state from event_log.payload, current state from row_to_json) and
    // 409 if the patch is empty. `event_log::diff` returns `[]` exactly
    // when the two values are identical, so the length check is cheap
    // and exact.
    let patch = crate::event_log::diff(&strip_managed(&current), &strip_managed(&target));
    if patch.as_array().map(Vec::is_empty).unwrap_or(true) {
        return Err(ApiError::RestoreNoOp);
    }
    let table = &schema.pg_qualified;

    // Apply the restore as a normal UPDATE on the visible fields. The
    // `target` row also carries managed columns (id/version/created_*/
    // updated_*) — we must NOT write those into the table; the UPDATE
    // bumps version itself and rewrites updated_at/updated_by from
    // current_setting.
    let target_obj = target
        .as_object()
        .ok_or_else(|| ApiError::Internal("event_log payload is not an object".into()))?;

    let mut cols = Vec::new();
    let mut casts = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for f in &schema.fields.ordered {
        if let Some(v) = target_obj.get(&f.name) {
            cols.push(f.name.clone());
            casts.push(crate::handlers::cast_placeholder(vals.len() + 1, f.kind));
            vals.push(v.clone());
        }
    }
    let set_clause = cols
        .iter()
        .zip(casts.iter())
        .map(|(c, p)| format!("{c} = {p}"))
        .collect::<Vec<_>>()
        .join(", ");
    let next_param = vals.len() + 1;
    let sql = format!(
        "UPDATE {table} SET {set_clause}, \
         version = version + 1, \
         updated_at = now(), \
         updated_by = current_setting('app.current_user', true) \
         WHERE id = ${next_param}::uuid AND deleted_at IS NULL \
         RETURNING (to_jsonb({table}.*) - '__fts') AS row"
    );

    let event_schema = schema.clone();
    let event_identity = identity.clone();
    let updated = crate::session::with_session_context(
        &state.pool,
        &schema,
        crate::session::RoleClass::Writer,
        &identity,
        move |tx| {
            Box::pin(async move {
                let mut q = sqlx::query(&sql);
                for v in &vals {
                    q = q.bind(v);
                }
                q = q.bind(&entity_id);
                let row: Value = q.fetch_one(&mut **tx).await?.get("row");
                // event_log entry — source=restore so SSE replay /
                // /history rendering can distinguish a rollback from a
                // user-driven edit. payload = new row (post-restore),
                // diff = the patch we just applied, reason = caller text.
                crate::event_log::write(
                    tx,
                    crate::event_log::EventLogRow {
                        schema: &event_schema,
                        entity_id: &entity_id,
                        operation: "restore",
                        source: crate::event_log::EventSource::Restore,
                        identity: &event_identity,
                        request_id: None,
                        diff: Some(patch.clone()),
                        payload: Some(row.clone()),
                        reason: reason.as_deref(),
                    },
                )
                .await?;
                Ok(row)
            })
        },
    )
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => ApiError::BadRequest(
            "entity vanished between read and write — retry the restore".into(),
        ),
        other => {
            tracing::error!(error = %other, "restore UPDATE failed");
            ApiError::Internal("restore failed".into())
        }
    })?;

    // The UPDATE wrote the raw target into the row (writing the
    // stripped shape would erase fields the actor can't read but is
    // still entitled to restore). Strip + mask the RETURNING row so
    // the response shape matches what a plain GET would return.
    let mut body = updated;
    strip_mask_payload(&schema, &identity, &mut body);
    Ok((StatusCode::OK, Json(body)))
}

/// Strip the management columns we maintain (id, version, created_*,
/// updated_*, deleted_at) from a `row_to_json` result before diffing for
/// no-op detection. Without this strip, every restore would look like a
/// no-op only if `version` and timestamps happened to match too, which
/// they never do.
fn strip_managed(row: &Value) -> Value {
    let mut v = row.clone();
    if let Some(obj) = v.as_object_mut() {
        for key in
            ["id", "version", "created_at", "created_by", "updated_at", "updated_by", "deleted_at"]
        {
            obj.remove(key);
        }
    }
    v
}

/// Translate sqlx errors into the right ApiError variant. `RowNotFound`
/// isn't possible on `fetch_all`, but other failures should be 500 with
/// a generic message — we don't surface schema/connection internals to
/// clients.
fn map_pg_err(e: sqlx::Error) -> ApiError {
    tracing::error!(error = %e, "event_log query failed");
    ApiError::Internal("history query failed".into())
}

/// Path tuple for the domain-scoped snapshot endpoint:
/// `/api/{org}/{app}/{domain}/history/snapshot`.
pub(crate) type DomainPathParts = (String, String, String);

/// Query parameters for `POST /{org}/{app}/{domain}/history/snapshot`.
/// Single required `at=<ISO8601>` — the point in time the snapshot is
/// reconstructed from. No body is consumed in v1 (filter shape is a
/// follow-up); using POST anyway because (a) the spec says so and (b) it
/// reserves room for the filter body without a breaking change.
#[derive(Debug, Deserialize)]
pub struct SnapshotQuery {
    pub at: DateTime<Utc>,
}

/// One row in the snapshot response — the reconstructed state for a
/// single entity, tagged with the schema path it belongs to so callers
/// can demux across multiple objects in one domain.
#[derive(Debug, Serialize)]
pub struct SnapshotItem {
    pub schema: String,
    pub entity_id: String,
    pub state: Value,
}

/// Hard ceiling on entities returned in one snapshot call. 10k matches
/// the QueryBuilder invariant #9 spirit (no unbounded result sets) and
/// keeps a single snapshot's memory footprint to ~tens of MB worst case.
/// If a domain holds more entities than this, the caller needs to fall
/// back to per-object `?at=` queries — we refuse rather than silently
/// truncating.
const SNAPSHOT_MAX_ITEMS: usize = 10_000;

/// `POST /api/{org}/{app}/{domain}/history/snapshot?at=T`
///
/// Cross-entity, cross-object snapshot scoped to a single domain. For
/// every schema currently registered under `{org}/{app}/{domain}/...`,
/// reconstructs the state of every entity that existed at-or-before T
/// (skipping those whose most recent event-at-or-before-T was a delete).
///
/// RBAC: requires `op::READ` on every schema under the domain. A single
/// denial fails the whole call (`403`) — a partial snapshot would be
/// misleading for the audit/debug use case this endpoint exists for.
///
/// Failure modes:
/// - No schemas registered under the domain prefix → 404 `SCHEMA_NOT_FOUND`
///   (no namespace to snapshot)
/// - More than `SNAPSHOT_MAX_ITEMS` entities match → 400 (loud-fail; the
///   caller should narrow with per-object `?at=` queries)
/// - `at` in the future is allowed (returns current state, same as
///   `point_in_time` — no special-case)
///
/// Response: `{ "at": "<T>", "count": N, "items": [SnapshotItem...] }`
pub async fn snapshot(
    State(state): State<AppState>,
    _decision_ext: Option<Extension<crate::auth::AuthDecision>>,
    identity: Option<Extension<Identity>>,
    Path(parts): Path<DomainPathParts>,
    Query(q): Query<SnapshotQuery>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (org, app, domain) = parts;
    let identity = identity_from_ext(identity);

    // Enumerate schemas under the domain prefix from the registry. The
    // registry is the authoritative list of *currently provisioned*
    // schemas; the event_log may carry rows for schemas since deleted
    // — those are intentionally excluded so the snapshot reflects what
    // the platform considers "live" today.
    let prefix = format!("{}/{}/{}/", org, app, domain);
    let registry_snap = state.registry.snapshot();
    let schemas_in_domain: Vec<_> = registry_snap
        .by_path
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, v)| v.clone())
        .collect();

    if schemas_in_domain.is_empty() {
        return Err(ApiError::SchemaNotFound);
    }

    // RBAC: every schema must permit READ. A single denial 403s the
    // whole call — see doc comment.
    for s in &schemas_in_domain {
        rbac::check_access(s, &identity, op::READ)?;
    }

    // Two-CTE single-query: `latest_at_t` reconstructs each entity's
    // state at T (DISTINCT ON does the per-entity newest pick),
    // `latest_non_delete` produces the gate payload (newest non-delete
    // EVER, regardless of T). The LEFT JOIN brings the gate payload
    // alongside the at-T state so Layer-4 row-filter can be applied in
    // app code without N+1 round trips.
    //
    // We fetch SNAPSHOT_MAX_ITEMS + 1 to detect overflow without two
    // queries: if N+1 rows come back, the dataset is larger than the
    // cap and we 400.
    let prefix_like = format!("{}/{}/{}/%", org, app, domain);
    let limit = (SNAPSHOT_MAX_ITEMS + 1) as i64;
    let rows = sqlx::query(
        "WITH latest_at_t AS ( \
             SELECT DISTINCT ON (schema_org, entity_id) \
                 schema_org, entity_id, operation, payload, occurred_at \
             FROM platform.event_log \
             WHERE schema_org LIKE $1 AND occurred_at <= $2 \
             ORDER BY schema_org, entity_id, occurred_at DESC \
         ), latest_non_delete AS ( \
             SELECT DISTINCT ON (schema_org, entity_id) \
                 schema_org, entity_id, payload AS gate_payload \
             FROM platform.event_log \
             WHERE schema_org LIKE $1 AND operation <> 'delete' \
             ORDER BY schema_org, entity_id, occurred_at DESC \
         ) \
         SELECT t.schema_org, t.entity_id::text AS entity_id, t.operation, \
                t.payload, n.gate_payload \
         FROM latest_at_t t \
         LEFT JOIN latest_non_delete n \
             ON n.schema_org = t.schema_org AND n.entity_id = t.entity_id \
         WHERE t.operation <> 'delete' \
         ORDER BY t.schema_org, t.entity_id \
         LIMIT $3",
    )
    .bind(&prefix_like)
    .bind(q.at)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(map_pg_err)?;

    if rows.len() > SNAPSHOT_MAX_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "snapshot exceeds {SNAPSHOT_MAX_ITEMS}-entity cap — narrow the scope with per-object ?at= queries"
        )));
    }

    let mut items: Vec<SnapshotItem> = Vec::with_capacity(rows.len());
    for row in rows {
        let schema_key: String = row.get("schema_org");
        let payload: Option<Value> = row.get("payload");
        let Some(mut state_val) = payload else {
            // Write event with NULL payload — same regression signal as
            // point_in_time. Skip silently here (instead of 500ing the
            // whole snapshot) so one corrupt row doesn't poison the
            // result, but log loudly so it's findable.
            tracing::error!(
                schema_org = %schema_key,
                entity_id = %row.get::<String, _>("entity_id"),
                "write event with NULL payload in snapshot — event_log writer regression",
            );
            continue;
        };

        // Look up the resolved schema for this row's path so we can
        // run the per-schema row-filter / field-filter / mask. The
        // snapshot query joined across every schema under the domain;
        // they all share the domain's `_reader` access decision (Layer 1
        // RBAC already gated the call) but the row-filter and field
        // configurations are per-schema.
        let row_schema = match registry_snap.by_path.get(&schema_key) {
            Some(s) => s,
            None => {
                // The schema was removed from the registry between the
                // outer enumeration and this loop iteration — vanishingly
                // rare but treat conservatively: skip the entity rather
                // than apply a fallback strip configuration that might
                // be wrong for the data.
                continue;
            }
        };

        // Layer-4: per-entity gate. Use the gate_payload from
        // `latest_non_delete` (newest non-delete EVER). If the entity
        // has only delete events, gate_payload is NULL and the entity
        // is dropped — consistent with "you need at least one
        // non-delete state to be entitled."
        let gate: Option<Value> = row.get("gate_payload");
        let Some(gate) = gate else { continue };
        let visible = crate::row_filter::payload_visible(row_schema, &identity, &gate)?;
        if !visible {
            continue;
        }

        // Layer-5 + Layer-6: strip + mask the state-at-T payload before
        // emitting. We mirror the live GET pipeline so a scoped reader
        // sees the same fields they'd get from a per-entity lookup.
        strip_mask_payload(row_schema, &identity, &mut state_val);

        items.push(SnapshotItem {
            schema: schema_key,
            entity_id: row.get("entity_id"),
            state: state_val,
        });
    }

    let count = items.len();
    Ok((
        StatusCode::OK,
        Json(json!({
            "at": q.at,
            "count": count,
            "items": items,
        })),
    ))
}
