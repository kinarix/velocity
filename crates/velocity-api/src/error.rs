//! API error type with a uniform JSON shape.
//!
//! Every handler returns `Result<_, ApiError>`. The `IntoResponse` impl maps
//! to `{ "error": "<UPPERCASE_CODE>", "message": "<human readable>" }` plus
//! the right HTTP status. Codes are stable strings — clients match on them.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("schema not found")]
    SchemaNotFound,

    #[error("record not found")]
    NotFound,

    #[error("version conflict")]
    VersionConflict,

    /// `Idempotency-Key` re-used with a different request body. The caller
    /// is almost certainly buggy — we refuse rather than re-doing work
    /// against possibly different state.
    #[error("idempotency-key reused with different body")]
    IdempotencyConflict,

    /// Restore target state matches current state — there is nothing to
    /// apply. Distinct from `VersionConflict` so dashboards can split
    /// the "operator clicked restore on the wrong row" noise from real
    /// concurrent-write contention.
    #[error("restore would be a no-op: target state matches current")]
    RestoreNoOp,

    #[error("unknown field `{0}`")]
    UnknownField(String),

    #[error("field `{0}` is not filterable")]
    NotFilterable(String),

    #[error("field `{0}` is not sortable")]
    NotSortable(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("payload too large")]
    PayloadTooLarge,

    /// No bearer token, or the token is structurally malformed (not three
    /// dot-separated segments / not base64). Returned before any JWKS work.
    #[error("authentication required: {0}")]
    Unauthenticated(String),

    /// Token parsed but failed verification — bad signature, expired,
    /// audience/issuer mismatch, unknown kid even after a forced JWKS
    /// refresh. We deliberately collapse these into one client-visible
    /// error so probing for the *reason* doesn't help an attacker.
    #[error("invalid token: {0}")]
    InvalidToken(String),

    /// The token's `iss` matches a configured issuer, but the JWKS for that
    /// issuer has never fetched successfully (cold-start, or sustained IdP
    /// outage). Distinct from `InvalidToken` because retry behaviour differs
    /// — clients should back off and retry.
    #[error("issuer unavailable: {0}")]
    IssuerUnavailable(String),

    /// The schema's `auth.strategyRef` points at a strategy that isn't in
    /// the `AuthRegistry`. Almost always a config drift between the
    /// operator and the API.
    #[error("auth strategy `{0}` not registered")]
    AuthStrategyMissing(String),

    /// The token verified but the actor id is in the Redis revoked-set.
    /// 403 (not 401) — the credential is genuine, it has just been
    /// rescinded. Returning 401 would invite clients to retry with a
    /// refreshed token, which would also be rejected.
    #[error("actor revoked")]
    Revoked,

    /// Revocation backend (Redis) is unreachable AND the strategy is
    /// fail-closed (ADR-003 default). 503 — clients should back off and
    /// retry. Distinct from `IssuerUnavailable` so we can alert on the
    /// two pathways separately.
    #[error("revocation backend unavailable")]
    RevocationUnavailable,

    /// OIDC browser-session backend (Postgres `platform.sessions`) is
    /// unreachable. 503 — distinct from `RevocationUnavailable` so a
    /// Postgres outage and a Redis outage alert on separate signals.
    #[error("session backend unavailable")]
    SessionUnavailable,

    /// Route-level RBAC denied: the schema declares `access.roles` but the
    /// caller's identity doesn't carry a role granting the requested op.
    /// 403 — credential is valid, just insufficient. Distinct from
    /// `Unauthenticated` (401) so clients don't refresh tokens uselessly.
    #[error("access denied")]
    AccessDenied,

    /// Layer-2 ABAC: a CEL policy denied the request. Distinct from
    /// `AccessDenied` (Layer 1 / RBAC) so dashboards and audit can tell
    /// "wrong role" apart from "right role, wrong row". The message body
    /// is the policy's configured `message` (or a generic fallback) —
    /// safe to surface, the CRD author chose the wording.
    #[error("access denied: {0}")]
    PolicyDenied(String),

    /// Layer-5 field filter: caller submitted one or more fields they're
    /// not allowed to write. Loud-fail rather than silent-strip so a
    /// caller doesn't see a 201 and assume their value stuck. The list is
    /// surfaced to the client so an integrator can identify which fields
    /// their token lacks the write role for.
    #[error("write denied for field(s): {}", .0.join(", "))]
    FieldWriteDenied(Vec<String>),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Phase 4: time-machine read targeted the warm tier but no warm
    /// reader is configured for this deployment. 503 — distinct from
    /// `WarmTierUnavailable` (configured, but currently unreachable)
    /// so a missing-config alerts separately from a runtime outage.
    #[error("warm tier not configured")]
    WarmTierNotConfigured,

    /// Phase 4: warm-reader call failed (network, 5xx, timeout). 503,
    /// ADR-003 fail-closed: no silent degradation to "no events".
    #[error("warm tier unavailable: {0}")]
    WarmTierUnavailable(String),

    /// Phase 4: read or write targeted a cold-tier timestamp. Reads
    /// return 202 + job_id via the dedicated cold path; this variant
    /// fires when a code path that should have routed to the cold
    /// stub falls through. Restore explicitly rejects this with 422.
    #[error("tier not supported for this operation")]
    RestoreTierUnsupported,

    /// Phase 5: caller attempted to `include` a related schema they
    /// don't have read access on. 403 — distinct from `AccessDenied`
    /// (which is "denied on THIS schema") so dashboards can spot
    /// "join probing" attempts.
    #[error("cross-schema access denied on include `{0}`")]
    CrossSchemaAccessDenied(String),

    /// Phase 5c: caller hit /search but the API process wasn't
    /// configured with a Typesense URL/key. 503 — fail-loud so a
    /// missing config doesn't masquerade as "no results".
    #[error("search not configured on this server")]
    SearchUnconfigured,

    /// Phase 5c: Typesense is configured but the call failed
    /// (network, 5xx, timeout). 503 — ADR-003 fail-closed; never
    /// silently return an empty result set.
    #[error("search unavailable: {0}")]
    SearchUnavailable(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::SchemaNotFound | ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::VersionConflict
            | ApiError::IdempotencyConflict
            | ApiError::RestoreNoOp => StatusCode::CONFLICT,
            ApiError::UnknownField(_)
            | ApiError::NotFilterable(_)
            | ApiError::NotSortable(_)
            | ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Unauthenticated(_) | ApiError::InvalidToken(_) => StatusCode::UNAUTHORIZED,
            ApiError::Revoked
            | ApiError::AccessDenied
            | ApiError::PolicyDenied(_)
            | ApiError::FieldWriteDenied(_)
            | ApiError::CrossSchemaAccessDenied(_) => StatusCode::FORBIDDEN,
            ApiError::IssuerUnavailable(_)
            | ApiError::RevocationUnavailable
            | ApiError::SessionUnavailable
            | ApiError::WarmTierNotConfigured
            | ApiError::WarmTierUnavailable(_)
            | ApiError::SearchUnconfigured
            | ApiError::SearchUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::RestoreTierUnsupported => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::AuthStrategyMissing(_)
            | ApiError::Database(_)
            | ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ApiError::SchemaNotFound => "SCHEMA_NOT_FOUND",
            ApiError::NotFound => "NOT_FOUND",
            ApiError::VersionConflict => "VERSION_CONFLICT",
            ApiError::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            ApiError::RestoreNoOp => "RESTORE_NO_OP",
            ApiError::UnknownField(_) => "UNKNOWN_FIELD",
            ApiError::NotFilterable(_) => "FIELD_NOT_FILTERABLE",
            ApiError::NotSortable(_) => "FIELD_NOT_SORTABLE",
            ApiError::BadRequest(_) => "BAD_REQUEST",
            ApiError::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            ApiError::Unauthenticated(_) => "UNAUTHENTICATED",
            ApiError::InvalidToken(_) => "INVALID_TOKEN",
            ApiError::IssuerUnavailable(_) => "ISSUER_UNAVAILABLE",
            ApiError::AuthStrategyMissing(_) => "AUTH_STRATEGY_MISSING",
            ApiError::Revoked => "ACTOR_REVOKED",
            ApiError::RevocationUnavailable => "REVOCATION_UNAVAILABLE",
            ApiError::SessionUnavailable => "SESSION_UNAVAILABLE",
            ApiError::AccessDenied => "ACCESS_DENIED",
            ApiError::PolicyDenied(_) => "POLICY_DENIED",
            ApiError::FieldWriteDenied(_) => "FIELD_WRITE_DENIED",
            ApiError::Database(_) => "DATABASE_ERROR",
            ApiError::Internal(_) => "INTERNAL_ERROR",
            ApiError::WarmTierNotConfigured => "WARM_TIER_NOT_CONFIGURED",
            ApiError::WarmTierUnavailable(_) => "WARM_TIER_UNAVAILABLE",
            ApiError::RestoreTierUnsupported => "RESTORE_TIER_UNSUPPORTED",
            ApiError::CrossSchemaAccessDenied(_) => "CROSS_SCHEMA_ACCESS_DENIED",
            ApiError::SearchUnconfigured => "SEARCH_NOT_CONFIGURED",
            ApiError::SearchUnavailable(_) => "SEARCH_UNAVAILABLE",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Database/internal errors carry the raw text into logs but expose a
        // generic message to the client — never leak Postgres details.
        let (log_detail, client_message) = match &self {
            ApiError::Database(e) => {
                (Some(e.to_string()), "an internal error occurred".to_string())
            }
            ApiError::Internal(e) => (Some(e.clone()), "an internal error occurred".to_string()),
            other => (None, other.to_string()),
        };
        if let Some(detail) = log_detail {
            tracing::error!(error = %detail, code = %self.code(), "api error");
        }
        let body = json!({ "error": self.code(), "message": client_message });
        (self.status(), Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_code_mapping() {
        assert_eq!(ApiError::SchemaNotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(ApiError::SchemaNotFound.code(), "SCHEMA_NOT_FOUND");
        assert_eq!(ApiError::VersionConflict.status(), StatusCode::CONFLICT);
        assert_eq!(ApiError::UnknownField("x".into()).status(), StatusCode::BAD_REQUEST);
        assert_eq!(ApiError::UnknownField("x".into()).code(), "UNKNOWN_FIELD");
    }

    /// Pin every remaining variant's `status()` and `code()` so the
    /// match arms can't silently drift. Each line below covers one
    /// arm; together they close the gap llvm-cov reports on this file.
    #[test]
    fn every_variant_status_and_code() {
        let cases: Vec<(ApiError, StatusCode, &'static str)> = vec![
            (ApiError::NotFound, StatusCode::NOT_FOUND, "NOT_FOUND"),
            (ApiError::VersionConflict, StatusCode::CONFLICT, "VERSION_CONFLICT"),
            (ApiError::IdempotencyConflict, StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"),
            (ApiError::RestoreNoOp, StatusCode::CONFLICT, "RESTORE_NO_OP"),
            (ApiError::NotFilterable("f".into()), StatusCode::BAD_REQUEST, "FIELD_NOT_FILTERABLE"),
            (ApiError::NotSortable("f".into()), StatusCode::BAD_REQUEST, "FIELD_NOT_SORTABLE"),
            (ApiError::BadRequest("b".into()), StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            (ApiError::PayloadTooLarge, StatusCode::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE"),
            (ApiError::Unauthenticated("u".into()), StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            (ApiError::InvalidToken("t".into()), StatusCode::UNAUTHORIZED, "INVALID_TOKEN"),
            (
                ApiError::IssuerUnavailable("i".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "ISSUER_UNAVAILABLE",
            ),
            (
                ApiError::AuthStrategyMissing("s".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "AUTH_STRATEGY_MISSING",
            ),
            (ApiError::Revoked, StatusCode::FORBIDDEN, "ACTOR_REVOKED"),
            (
                ApiError::RevocationUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "REVOCATION_UNAVAILABLE",
            ),
            (
                ApiError::SessionUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "SESSION_UNAVAILABLE",
            ),
            (ApiError::AccessDenied, StatusCode::FORBIDDEN, "ACCESS_DENIED"),
            (ApiError::PolicyDenied("p".into()), StatusCode::FORBIDDEN, "POLICY_DENIED"),
            (
                ApiError::FieldWriteDenied(vec!["x".into()]),
                StatusCode::FORBIDDEN,
                "FIELD_WRITE_DENIED",
            ),
            (
                ApiError::Internal("oops".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
            ),
            (
                ApiError::WarmTierNotConfigured,
                StatusCode::SERVICE_UNAVAILABLE,
                "WARM_TIER_NOT_CONFIGURED",
            ),
            (
                ApiError::WarmTierUnavailable("w".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "WARM_TIER_UNAVAILABLE",
            ),
            (
                ApiError::RestoreTierUnsupported,
                StatusCode::UNPROCESSABLE_ENTITY,
                "RESTORE_TIER_UNSUPPORTED",
            ),
            (
                ApiError::CrossSchemaAccessDenied("inc".into()),
                StatusCode::FORBIDDEN,
                "CROSS_SCHEMA_ACCESS_DENIED",
            ),
            (
                ApiError::SearchUnconfigured,
                StatusCode::SERVICE_UNAVAILABLE,
                "SEARCH_NOT_CONFIGURED",
            ),
            (
                ApiError::SearchUnavailable("s".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "SEARCH_UNAVAILABLE",
            ),
        ];
        for (err, status, code) in cases {
            assert_eq!(err.status(), status, "status for {code}");
            assert_eq!(err.code(), code, "code for {code}");
        }
    }

    #[test]
    fn database_error_status_and_code() {
        // Forge a sqlx::Error to exercise the Database arm — it carries
        // the inner detail to logs but renders as INTERNAL_SERVER_ERROR
        // to the client.
        let err = ApiError::Database(sqlx::Error::RowNotFound);
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.code(), "DATABASE_ERROR");
    }
}
