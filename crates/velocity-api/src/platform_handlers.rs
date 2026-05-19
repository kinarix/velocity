//! HTTP adapters for `/api/platform/audit*` (Phase 6a-2).
//!
//! Why a separate module from [`crate::handlers`]:
//!
//! - These routes are NOT bound to a tenant schema, so the
//!   [`ResolvedSchema`]-based plumbing in `handlers.rs` (RBAC, RLS
//!   role-class, field-filter, masking) doesn't apply.
//! - They authenticate via a single platform-shared service token
//!   (`VELOCITY_API_PLATFORM_AUDIT_TOKEN`), constant-time-compared.
//!   Tenant identity is irrelevant — there is no per-actor RBAC on
//!   audit reads in v1.
//! - Every call self-audits: who pulled which window of which org's
//!   audit log. Without this, an audit endpoint is a surveillance
//!   backdoor.
//!
//! ## Authorization
//!
//! Two gates, in order:
//! 1. `Authorization: Bearer <token>` must equal
//!    `AppState::platform_audit_token` (constant-time).
//! 2. The handler-level [`crate::audit_query::list_audit`] enforces
//!    `schema_org` is present.
//!
//! When the token is unset at startup the endpoint uniformly 401s
//! every caller — explicit failure over silent admission.
//!
//! [`ResolvedSchema`]: crate::registry::ResolvedSchema

use axum::extract::{Query, State};
use axum::http::{HeaderMap, header::AUTHORIZATION};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::audit::{self, action, outcome, AUDIT_SELF_SCHEMA_ORG};
use crate::audit_query::{self, AuditListParams};
use crate::error::ApiError;
use crate::state::AppState;

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Actor string recorded in the self-audit row. Pinned so dashboards
/// can pivot on `actor = 'platform:audit-reader'` to find every
/// audit-of-audit row in one query.
const PLATFORM_AUDIT_ACTOR: &str = "platform:audit-reader";

/// Default verify window when the caller doesn't supply `from`/`to`.
/// One hour is the typical "is something happening right now" window
/// — wider is opt-in via explicit timestamps.
const DEFAULT_VERIFY_WINDOW_HOURS: i64 = 1;

/// Extract the request id the [`tower_http::request_id::SetRequestIdLayer`]
/// attached, or `None` if absent.
fn request_id_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers.get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok())
}

/// Constant-time bearer-token check against the configured platform
/// audit token. Returns `Ok(())` when the header is present, well-formed,
/// and matches; [`ApiError::AuditUnauthorized`] otherwise. The error is
/// uniform — we never leak *which* part failed, so a probing client
/// can't distinguish "no header" from "wrong scheme" from "wrong token".
pub fn verify_platform_token(
    headers: &HeaderMap,
    expected: Option<&str>,
) -> Result<(), ApiError> {
    let Some(expected) = expected else {
        // Unset at startup => deny everyone. The startup banner already
        // warned the operator; we keep the runtime behaviour boring.
        return Err(ApiError::AuditUnauthorized);
    };
    let h = headers.get(AUTHORIZATION).ok_or(ApiError::AuditUnauthorized)?;
    let s = h.to_str().map_err(|_| ApiError::AuditUnauthorized)?;
    let token = s
        .strip_prefix("Bearer ")
        .ok_or(ApiError::AuditUnauthorized)?;
    if token.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(())
    } else {
        Err(ApiError::AuditUnauthorized)
    }
}

/// `GET /api/platform/audit?schema_org=&actor=&action=&outcome=&...`
///
/// Returns a paginated slice of `platform.audit_log`. `schema_org` is
/// required — without it the endpoint would be a cross-tenant dump
/// gated only by the shared platform token, which is too coarse.
///
/// Self-audit: every successful call writes a `read` row against the
/// synthetic [`AUDIT_SELF_SCHEMA_ORG`] with the filter set + result
/// count in the payload. Denials self-audit too (they're the row a
/// SOC analyst pivots on after a leaked-token incident).
pub async fn audit_list(
    State(state): State<AppState>,
    Query(params): Query<AuditListParams>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let request_id = request_id_from_headers(&headers).map(str::to_owned);

    // Stage 1: bearer-token gate. On failure, self-audit the denial
    // before returning — operators care more about authn failures on
    // this endpoint than on any other.
    if let Err(e) = verify_platform_token(
        &headers,
        state.platform_audit_token.as_deref().map(String::as_str),
    ) {
        write_self_audit(
            &state,
            action::READ,
            outcome::DENIED,
            json!({ "code": e.code() }),
            request_id.as_deref(),
        )
        .await;
        return Err(e);
    }

    // Stage 2: run the query (which enforces schema_org-required).
    let page = match audit_query::list_audit(
        &state.pool,
        state.cursor_signer.as_ref(),
        &params,
    )
    .await
    {
        Ok(page) => page,
        Err(e) => {
            // Filter-missing / cursor-tampered failures self-audit so
            // a SOC analyst can spot scripted probing.
            write_self_audit(
                &state,
                action::READ,
                outcome::DENIED,
                json!({
                    "code": e.code(),
                    "schema_org": params.schema_org.clone(),
                }),
                request_id.as_deref(),
            )
            .await;
            return Err(e);
        }
    };

    // Self-audit the successful read. Payload carries the filter set
    // + result size so a SOC analyst can reconstruct what was queried
    // without re-parsing the request. Sensitive values are not present
    // — actor/action/outcome/schema_org are all metadata, not PII.
    write_self_audit(
        &state,
        action::READ,
        outcome::SUCCESS,
        json!({
            "schema_org": params.schema_org,
            "actor_filter": params.actor,
            "action_filter": params.action,
            "outcome_filter": params.outcome,
            "from": params.from,
            "to": params.to,
            "limit": params.limit,
            "count": page.count,
        }),
        request_id.as_deref(),
    )
    .await;

    Ok(Json(json!({
        "rows": page.rows,
        "next_cursor": page.next_cursor,
        "count": page.count,
    })))
}

/// Query params for `/audit/verify`. Both timestamps optional —
/// defaults to "the last hour" so the endpoint is a one-click sanity
/// check from a runbook.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct VerifyParams {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// `GET /api/platform/audit/verify?from=&to=`
///
/// Returns `{ from, to, chain_intact: bool, mismatches: [...] }`. A
/// healthy chain has `mismatches = []`; any non-empty list is a
/// tamper signal an operator should escalate.
pub async fn audit_verify(
    State(state): State<AppState>,
    Query(params): Query<VerifyParams>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let request_id = request_id_from_headers(&headers).map(str::to_owned);

    if let Err(e) = verify_platform_token(
        &headers,
        state.platform_audit_token.as_deref().map(String::as_str),
    ) {
        write_self_audit(
            &state,
            "verify",
            outcome::DENIED,
            json!({ "code": e.code() }),
            request_id.as_deref(),
        )
        .await;
        return Err(e);
    }

    // Default window: trailing 1h ending now.
    let to = params.to.unwrap_or_else(Utc::now);
    let from = params
        .from
        .unwrap_or_else(|| to - Duration::hours(DEFAULT_VERIFY_WINDOW_HOURS));

    let mismatches = match audit_query::verify_window(&state.pool, from, to).await {
        Ok(m) => m,
        Err(e) => {
            write_self_audit(
                &state,
                "verify",
                outcome::DENIED,
                json!({
                    "code": e.code(),
                    "from": from,
                    "to": to,
                }),
                request_id.as_deref(),
            )
            .await;
            return Err(e);
        }
    };

    write_self_audit(
        &state,
        "verify",
        outcome::SUCCESS,
        json!({
            "from": from,
            "to": to,
            "mismatch_count": mismatches.len(),
        }),
        request_id.as_deref(),
    )
    .await;

    Ok(Json(audit_query::verify_envelope(from, to, &mismatches)))
}

/// Write a `platform.audit_log` row recording "the audit log itself
/// was read". Failure is logged-not-propagated — we never block a
/// 200 (or a 401/400) on the inability to write the meta row.
///
/// Naming: `action` is the *operation on the audit table* (`read`,
/// `verify`); `outcome` is `success | denied | error`. Together they
/// give the same shape as every other audit row, so existing
/// Grafana queries keep working.
async fn write_self_audit(
    state: &AppState,
    action: &str,
    outcome: &str,
    payload: Value,
    request_id: Option<&str>,
) {
    if let Err(e) = audit::write_audit_meta(
        &state.pool,
        PLATFORM_AUDIT_ACTOR,
        AUDIT_SELF_SCHEMA_ORG,
        action,
        outcome,
        &payload,
        request_id,
    )
    .await
    {
        tracing::error!(
            error = %e,
            action = %action,
            outcome = %outcome,
            "platform audit self-audit write failed"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = auth {
            h.insert(AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn verify_token_admits_exact_bearer_match() {
        let expected = "a-secure-audit-token-1234567890";
        let h = headers_with(Some("Bearer a-secure-audit-token-1234567890"));
        assert!(verify_platform_token(&h, Some(expected)).is_ok());
    }

    #[test]
    fn verify_token_denies_when_unset() {
        // If the env var wasn't set at startup, every caller MUST 401 —
        // we never silently admit. Pinned so a future refactor can't
        // turn `None` into "no auth required".
        let h = headers_with(Some("Bearer anything"));
        let err = verify_platform_token(&h, None).unwrap_err();
        assert!(matches!(err, ApiError::AuditUnauthorized));
    }

    #[test]
    fn verify_token_denies_missing_header() {
        let expected = "a-secure-audit-token-1234567890";
        let h = headers_with(None);
        let err = verify_platform_token(&h, Some(expected)).unwrap_err();
        assert!(matches!(err, ApiError::AuditUnauthorized));
    }

    #[test]
    fn verify_token_denies_wrong_scheme() {
        // A `Basic` header on this endpoint shouldn't be accepted —
        // even if its decoded value happened to match the bearer
        // token. We pin Bearer-only.
        let expected = "a-secure-audit-token-1234567890";
        let h = headers_with(Some("Basic a-secure-audit-token-1234567890"));
        let err = verify_platform_token(&h, Some(expected)).unwrap_err();
        assert!(matches!(err, ApiError::AuditUnauthorized));
    }

    #[test]
    fn verify_token_denies_mismatched_value() {
        let expected = "a-secure-audit-token-1234567890";
        let h = headers_with(Some("Bearer not-the-real-token-xxxxxxxxxx"));
        let err = verify_platform_token(&h, Some(expected)).unwrap_err();
        assert!(matches!(err, ApiError::AuditUnauthorized));
    }

    #[test]
    fn verify_token_denies_non_ascii_header() {
        // `HeaderValue::to_str()` errors on non-ASCII; we MUST surface
        // a uniform AuditUnauthorized instead of leaking the parse
        // failure as a 500.
        let expected = "a-secure-audit-token-1234567890";
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff\xfe").unwrap(),
        );
        let err = verify_platform_token(&h, Some(expected)).unwrap_err();
        assert!(matches!(err, ApiError::AuditUnauthorized));
    }

    #[test]
    fn verify_params_unknown_field_rejected() {
        // serde `deny_unknown_fields` — a typo'd `from_ts` would 400
        // rather than be silently ignored.
        let json = r#"{ "fromm": "2026-05-19T00:00:00Z" }"#;
        let err = serde_json::from_str::<VerifyParams>(json).unwrap_err();
        assert!(err.to_string().contains("fromm") || err.to_string().contains("unknown"));
    }
}
