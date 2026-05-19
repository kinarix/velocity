//! Read-side of the audit chain — query + verify.
//!
//! Phase 6a-2. Public surface lives behind `/api/platform/audit*`
//! handlers in [`crate::handlers`]; this module owns the SQL,
//! cursor pagination, and verify shape so the handler stays a thin
//! HTTP adapter.
//!
//! ## Pagination
//!
//! Keyset on `(occurred_at, id)` — matches `idx_audit_log_actor_time`
//! and friends. Cursor is HMAC-signed using the same `CursorSigner` the
//! POST `/query` DSL uses; one key, one rotation knob (ADR-009 wins for
//! every unbounded-result endpoint, not just user data).
//!
//! Default page size is small (50). Hard cap is 200 — audit rows can be
//! megabytes when payloads include long records, and the chain's
//! ~5k/sec write ceiling (ADR-005) means a runaway query is far more
//! likely to swamp the audit log itself than to be paginated through.
//!
//! ## Tenancy scoping
//!
//! [`AuditListParams::schema_org`] is **required**. Without it,
//! `/audit` would be a cross-tenant skeleton key — any caller with the
//! platform-internal bearer token could pull every org's data
//! in one request. The bearer token *is* a global authorisation, but
//! that's not the same thing as a no-filter default. Fail-loud at the
//! request level so it's clear what the operator asked for.
//!
//! A future `platform-audit-reader-global` extension can relax this;
//! adding it later is cheap, removing a permissive default after the
//! fact is not.
//!
//! ## Verify
//!
//! [`verify_window`] wraps `platform.audit_verify_window(from, to)`
//! and filters server-side to rows where `stored_hash != computed_hash`.
//! Operators care about *tamper detection*, not the full row dump — a
//! healthy chain returns zero rows, which is the answer in 99.9% of
//! cases. Window capped to [`MAX_VERIFY_WINDOW_HOURS`].

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::dsl::CursorSigner;
use crate::error::ApiError;

/// Default page size for `/audit` listing. Small on purpose — audit
/// rows can be large and operators almost always want the latest few.
pub const DEFAULT_AUDIT_PAGE_SIZE: u32 = 50;

/// Hard cap on `limit`. Beyond this, callers must paginate via
/// `next_cursor`. Picked low because the row width is unbounded —
/// `payload` may be a full record snapshot.
pub const MAX_AUDIT_PAGE_SIZE: u32 = 200;

/// Maximum window for `audit_verify_window` in a single call. The
/// stored proc recomputes SHA-256 per row; a multi-day window over
/// busy schemas hangs a worker. Operators chase narrow incidents —
/// the day-shaped cap matches the workflow.
pub const MAX_VERIFY_WINDOW_HOURS: i64 = 24;

/// Filter set for `GET /api/platform/audit`. Every field except
/// `schema_org` is optional. `schema_org` is required to keep the
/// endpoint from acting as a cross-tenant dump (see module docs).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditListParams {
    /// `org/app/domain/object/version` — REQUIRED. The handler enforces
    /// this; deserialize keeps it `Option` so we can return a more
    /// specific error than serde's generic "missing field".
    pub schema_org: Option<String>,
    /// Filter on `actor` exact match.
    pub actor: Option<String>,
    /// Filter on `action` exact match (e.g. `read`, `create`).
    pub action: Option<String>,
    /// Filter on `outcome` exact match (`success`, `denied`, `error`).
    pub outcome: Option<String>,
    /// Filter on `entity_id` (UUID).
    pub entity_id: Option<Uuid>,
    /// Lower bound on `occurred_at` (inclusive).
    pub from: Option<DateTime<Utc>>,
    /// Upper bound on `occurred_at` (exclusive).
    pub to: Option<DateTime<Utc>>,
    /// Page size; `None` => [`DEFAULT_AUDIT_PAGE_SIZE`]. Clamped to
    /// [`MAX_AUDIT_PAGE_SIZE`].
    pub limit: Option<u32>,
    /// Opaque cursor minted by a previous call. When present, fetches
    /// the next page from the cursor's `(occurred_at, id)` boundary.
    pub cursor: Option<String>,
}

/// Wire shape of an audit row. Matches `platform.audit_log` columns;
/// we expose `prev_hash` + `hash` so a caller running their own chain
/// check can recompute without re-querying.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    pub id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub outcome: String,
    pub schema_org: Option<String>,
    pub entity_id: Option<Uuid>,
    pub payload: Option<Value>,
    pub prev_hash: Option<String>,
    pub hash: String,
    pub fail_modes: Option<Value>,
    pub request_id: Option<String>,
    pub reason: Option<String>,
    pub ticket_ref: Option<String>,
}

/// Result envelope: a page of rows + optional cursor for the next.
#[derive(Debug, Clone, Serialize)]
pub struct AuditPage {
    pub rows: Vec<AuditRow>,
    pub next_cursor: Option<String>,
    pub count: usize,
}

/// Cursor envelope. `(occurred_at, id)` is the natural keyset given
/// the existing `idx_audit_log_actor_time` / `idx_audit_log_schema_time`
/// indexes — DESC ordering puts the newest rows first, so the cursor
/// remembers the last row we already returned and the next page
/// starts strictly *older*.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditCursor {
    /// Pin the cursor to the filter set it was minted against — a
    /// caller who reuses a cursor with a different `schema_org` would
    /// otherwise silently get the wrong page. Tampering this segment
    /// is caught by the HMAC; we still bind it as a defensive
    /// shape-pin.
    filter_sig: String,
    /// Last-seen `occurred_at`.
    occurred_at: DateTime<Utc>,
    /// Last-seen `id` — tiebreaker when two rows share the same
    /// `occurred_at` (possible with sub-microsecond clocks).
    id: Uuid,
}

/// Stable digest of the filters the cursor was minted against. Any
/// change to a filter invalidates the cursor — fail-loud rather than
/// silently re-order results.
fn filter_signature(p: &AuditListParams) -> String {
    // `schema_org` is required at handler-level; default `""` here
    // keeps the function pure for tests that don't enforce it.
    let parts = [
        ("schema_org", p.schema_org.as_deref().unwrap_or("")),
        ("actor", p.actor.as_deref().unwrap_or("")),
        ("action", p.action.as_deref().unwrap_or("")),
        ("outcome", p.outcome.as_deref().unwrap_or("")),
    ];
    let mut s = String::with_capacity(64);
    for (k, v) in parts {
        s.push_str(k);
        s.push('=');
        s.push_str(v);
        s.push(';');
    }
    if let Some(eid) = p.entity_id {
        s.push_str("entity=");
        s.push_str(&eid.to_string());
        s.push(';');
    }
    s
}

/// Encode a cursor envelope as an HMAC-signed opaque string.
fn mint_cursor(
    signer: &CursorSigner,
    params: &AuditListParams,
    last: &AuditRow,
) -> Result<String, ApiError> {
    let env = AuditCursor {
        filter_sig: filter_signature(params),
        occurred_at: last.occurred_at,
        id: last.id,
    };
    let payload =
        serde_json::to_vec(&env).map_err(|e| ApiError::Internal(format!("audit cursor: {e}")))?;
    signer.sign_bytes(&payload)
}

/// Decode + verify a cursor; reject if it was minted against a
/// different filter set.
fn open_cursor(
    signer: &CursorSigner,
    params: &AuditListParams,
    cursor: &str,
) -> Result<AuditCursor, ApiError> {
    let payload = signer.verify_bytes(cursor)?;
    let env: AuditCursor = serde_json::from_slice(&payload)
        .map_err(|_| ApiError::BadRequest("audit cursor: bad payload json".into()))?;
    if env.filter_sig != filter_signature(params) {
        return Err(ApiError::BadRequest(
            "audit cursor: filter set differs from cursor's; mint a new cursor".into(),
        ));
    }
    Ok(env)
}

/// Query `platform.audit_log` with the given filters. Returns one page
/// + optional `next_cursor`. Caller must enforce authorization
///   before calling this; `schema_org`-required is enforced in here.
pub async fn list_audit(
    pool: &PgPool,
    signer: Option<&Arc<CursorSigner>>,
    params: &AuditListParams,
) -> Result<AuditPage, ApiError> {
    let schema_org =
        params.schema_org.as_deref().ok_or(ApiError::AuditFilterRequired("schema_org"))?;

    let limit = params.limit.unwrap_or(DEFAULT_AUDIT_PAGE_SIZE).clamp(1, MAX_AUDIT_PAGE_SIZE);

    // Build the WHERE clause incrementally. Every value is bound; the
    // strings we splice are static SQL fragments.
    //
    // Param positions track in lock-step with `binds.push(...)`. Putting
    // both behind a `next_param!` closure would be cleaner but doesn't
    // fit cleanly with sqlx's typed bind chain — explicit is fine here.
    let mut clauses: Vec<String> = Vec::with_capacity(8);
    // Boxed dyn binds: rebuilt per param so we can hold mixed types
    // (String, Uuid, DateTime) in one Vec.
    enum Bind {
        Str(String),
        Uuid(Uuid),
        Ts(DateTime<Utc>),
    }
    let mut binds: Vec<Bind> = Vec::with_capacity(8);

    binds.push(Bind::Str(schema_org.to_string()));
    clauses.push(format!("schema_org = ${}", binds.len()));

    if let Some(actor) = &params.actor {
        binds.push(Bind::Str(actor.clone()));
        clauses.push(format!("actor = ${}", binds.len()));
    }
    if let Some(action) = &params.action {
        binds.push(Bind::Str(action.clone()));
        clauses.push(format!("action = ${}", binds.len()));
    }
    if let Some(outcome) = &params.outcome {
        binds.push(Bind::Str(outcome.clone()));
        clauses.push(format!("outcome = ${}", binds.len()));
    }
    if let Some(eid) = params.entity_id {
        binds.push(Bind::Uuid(eid));
        clauses.push(format!("entity_id = ${}", binds.len()));
    }
    if let Some(from) = params.from {
        binds.push(Bind::Ts(from));
        clauses.push(format!("occurred_at >= ${}", binds.len()));
    }
    if let Some(to) = params.to {
        binds.push(Bind::Ts(to));
        clauses.push(format!("occurred_at < ${}", binds.len()));
    }

    // Keyset cursor: rows STRICTLY older than (cursor.occurred_at, cursor.id)
    // because we order DESC. The tuple comparison
    //   (occurred_at, id) < (cursor.occurred_at, cursor.id)
    // is what Postgres consumes most cleanly with an index on
    // (occurred_at DESC, id DESC) — though we currently rely on the
    // schema_org index + a sort. Good enough for v1.
    if let Some(cursor) = &params.cursor {
        let signer = signer.ok_or_else(|| {
            ApiError::BadRequest(
                "audit cursor presented but cursor signing key not configured".into(),
            )
        })?;
        let env = open_cursor(signer, params, cursor)?;
        binds.push(Bind::Ts(env.occurred_at));
        let p1 = binds.len();
        binds.push(Bind::Uuid(env.id));
        let p2 = binds.len();
        clauses.push(format!("(occurred_at, id) < (${p1}, ${p2})"));
    }

    // Fetch limit+1 to detect "more pages exist" without a separate COUNT.
    let fetch_limit = (limit + 1) as i64;

    let sql = format!(
        "SELECT id, occurred_at, actor, action, outcome, schema_org, \
                entity_id, payload, prev_hash, hash, fail_modes, request_id, \
                reason, ticket_ref \
         FROM platform.audit_log \
         WHERE {} \
         ORDER BY occurred_at DESC, id DESC \
         LIMIT {}",
        clauses.join(" AND "),
        fetch_limit
    );

    let mut q = sqlx::query_as::<_, AuditRowDb>(&sql);
    for b in binds {
        q = match b {
            Bind::Str(s) => q.bind(s),
            Bind::Uuid(u) => q.bind(u),
            Bind::Ts(t) => q.bind(t),
        };
    }
    let mut rows: Vec<AuditRow> =
        q.fetch_all(pool).await?.into_iter().map(AuditRow::from).collect();

    let has_more = rows.len() as u32 > limit;
    if has_more {
        rows.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        match (signer, rows.last()) {
            (Some(s), Some(last)) => Some(mint_cursor(s.as_ref(), params, last)?),
            _ => None,
        }
    } else {
        None
    };
    let count = rows.len();
    Ok(AuditPage { rows, next_cursor, count })
}

/// `FromRow` shape mirroring the `audit_log` columns. Distinct from
/// [`AuditRow`] so the wire shape can evolve without touching the
/// SQL-decoding step.
#[derive(Debug, sqlx::FromRow)]
struct AuditRowDb {
    id: Uuid,
    occurred_at: DateTime<Utc>,
    actor: String,
    action: String,
    outcome: String,
    schema_org: Option<String>,
    entity_id: Option<Uuid>,
    payload: Option<Value>,
    prev_hash: Option<String>,
    hash: String,
    fail_modes: Option<Value>,
    request_id: Option<String>,
    reason: Option<String>,
    ticket_ref: Option<String>,
}

impl From<AuditRowDb> for AuditRow {
    fn from(r: AuditRowDb) -> Self {
        Self {
            id: r.id,
            occurred_at: r.occurred_at,
            actor: r.actor,
            action: r.action,
            outcome: r.outcome,
            schema_org: r.schema_org,
            entity_id: r.entity_id,
            payload: r.payload,
            prev_hash: r.prev_hash,
            hash: r.hash,
            fail_modes: r.fail_modes,
            request_id: r.request_id,
            reason: r.reason,
            ticket_ref: r.ticket_ref,
        }
    }
}

/// One row of the verify result: an audit row whose stored hash does
/// not match what we recompute from its visible columns. A healthy
/// chain returns an empty vector.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyMismatch {
    pub id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub stored_hash: String,
    pub computed_hash: String,
}

/// Call `platform.audit_verify_window(from, to)` and filter to rows
/// where `stored_hash != computed_hash`. Window capped at
/// [`MAX_VERIFY_WINDOW_HOURS`] — beyond that the SHA-256 recompute
/// per row is too expensive to do online.
pub async fn verify_window(
    pool: &PgPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<VerifyMismatch>, ApiError> {
    if to <= from {
        return Err(ApiError::BadRequest(
            "audit verify: `to` must be strictly greater than `from`".into(),
        ));
    }
    let window_hours = (to - from).num_hours();
    if window_hours > MAX_VERIFY_WINDOW_HOURS {
        return Err(ApiError::AuditWindowTooWide {
            max_hours: MAX_VERIFY_WINDOW_HOURS as u32,
            requested_hours: window_hours.max(0) as u32,
        });
    }

    let rows: Vec<(Uuid, DateTime<Utc>, String, String)> = sqlx::query_as(
        "SELECT id, occurred_at, stored_hash, computed_hash \
         FROM platform.audit_verify_window($1, $2) \
         WHERE stored_hash IS DISTINCT FROM computed_hash \
         ORDER BY occurred_at",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, occurred_at, stored_hash, computed_hash)| VerifyMismatch {
            id,
            occurred_at,
            stored_hash,
            computed_hash,
        })
        .collect())
}

/// Render a `VerifyMismatch` set into the wire shape returned by
/// `/api/platform/audit/verify`. Pulled out so the handler stays a
/// pure adapter and `chain_intact` doesn't drift between code paths.
pub fn verify_envelope(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    mismatches: &[VerifyMismatch],
) -> Value {
    json!({
        "from": from,
        "to": to,
        "chain_intact": mismatches.is_empty(),
        "mismatches": mismatches,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn key() -> Vec<u8> {
        b"this-is-a-32-byte-test-key-xxxx!".to_vec()
    }

    fn signer() -> CursorSigner {
        CursorSigner::new(key()).unwrap()
    }

    fn params_with_filters() -> AuditListParams {
        AuditListParams {
            schema_org: Some("acme/sc/proc/po/v1".into()),
            actor: Some("alice".into()),
            action: Some("read".into()),
            ..Default::default()
        }
    }

    fn row(occurred_at: DateTime<Utc>, id: Uuid) -> AuditRow {
        AuditRow {
            id,
            occurred_at,
            actor: "alice".into(),
            action: "read".into(),
            outcome: "success".into(),
            schema_org: Some("acme/sc/proc/po/v1".into()),
            entity_id: None,
            payload: Some(json!({})),
            prev_hash: None,
            hash: "h".into(),
            fail_modes: None,
            request_id: None,
            reason: None,
            ticket_ref: None,
        }
    }

    #[test]
    fn filter_signature_is_stable_and_distinguishes_filters() {
        let a = params_with_filters();
        let mut b = params_with_filters();
        assert_eq!(filter_signature(&a), filter_signature(&b));
        b.actor = Some("bob".into());
        assert_ne!(filter_signature(&a), filter_signature(&b));
    }

    #[test]
    fn cursor_roundtrip_preserves_envelope() {
        let s = signer();
        let p = params_with_filters();
        let now = Utc::now();
        let id = Uuid::new_v4();
        let cur = mint_cursor(&s, &p, &row(now, id)).unwrap();
        let env = open_cursor(&s, &p, &cur).unwrap();
        assert_eq!(env.id, id);
        assert_eq!(env.occurred_at, now);
        assert_eq!(env.filter_sig, filter_signature(&p));
    }

    #[test]
    fn cursor_rejects_filter_mutation() {
        // A SOC analyst pages 1 → 2 with `actor=alice`, then naively
        // re-uses the same cursor while changing the filter to
        // `actor=bob`. The page-2 cursor MUST refuse rather than
        // silently re-anchoring on `(occurred_at,id)` and giving a
        // wrong-shape result.
        let s = signer();
        let p1 = params_with_filters();
        let mut p2 = p1.clone();
        p2.actor = Some("bob".into());

        let cur = mint_cursor(&s, &p1, &row(Utc::now(), Uuid::new_v4())).unwrap();
        let err = open_cursor(&s, &p2, &cur).unwrap_err();
        match err {
            ApiError::BadRequest(msg) => {
                assert!(msg.contains("filter set differs"), "got: {msg}");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn cursor_rejects_tampering() {
        let s = signer();
        let p = params_with_filters();
        let cur = mint_cursor(&s, &p, &row(Utc::now(), Uuid::new_v4())).unwrap();
        // Flip a byte in the payload segment — the HMAC won't verify.
        let mut bytes: Vec<u8> = cur.into_bytes();
        bytes[2] ^= 0x01;
        let tampered = String::from_utf8(bytes).unwrap();
        let err = open_cursor(&s, &p, &tampered).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn cursor_rejects_signed_by_different_key() {
        // Two different keys MUST not interoperate. Pin so a refactor
        // that, say, shares one signer across audit + DSL doesn't
        // accidentally hand out cross-resource cursors.
        let s1 = CursorSigner::new(key()).unwrap();
        let alt_key = {
            let mut k = key();
            k[0] ^= 0xff;
            k
        };
        let s2 = CursorSigner::new(alt_key).unwrap();
        let p = params_with_filters();
        let cur = mint_cursor(&s1, &p, &row(Utc::now(), Uuid::new_v4())).unwrap();
        let err = open_cursor(&s2, &p, &cur).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn verify_envelope_chain_intact_when_empty() {
        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now();
        let env = verify_envelope(from, to, &[]);
        assert_eq!(env["chain_intact"], true);
        assert_eq!(env["mismatches"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn verify_envelope_chain_broken_when_mismatches_present() {
        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now();
        let m = vec![VerifyMismatch {
            id: Uuid::new_v4(),
            occurred_at: Utc::now(),
            stored_hash: "stored".into(),
            computed_hash: "computed".into(),
        }];
        let env = verify_envelope(from, to, &m);
        assert_eq!(env["chain_intact"], false);
        assert_eq!(env["mismatches"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn audit_self_schema_org_is_three_underscored() {
        // The const must lead with `__platform` so any CRD validator
        // that rejects double-underscore prefixes never collides with a
        // real tenant org. Pinned because a careless rename to
        // `_platform` or just `platform` would silently allow a tenant
        // to spoof `platform` as their own org segment.
        assert!(
            crate::audit::AUDIT_SELF_SCHEMA_ORG.starts_with("__platform/"),
            "audit self schema_org must reserve the platform namespace"
        );
    }

    #[test]
    fn audit_list_params_unknown_fields_rejected() {
        // serde `deny_unknown_fields` — a typo'd query param should
        // 400 rather than be silently ignored.
        let json = r#"{ "schema_org": "x", "actorr": "alice" }"#;
        let err = serde_json::from_str::<AuditListParams>(json).unwrap_err();
        assert!(err.to_string().contains("actorr") || err.to_string().contains("unknown"));
    }
}
