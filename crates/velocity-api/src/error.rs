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

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::SchemaNotFound | ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::VersionConflict | ApiError::IdempotencyConflict => StatusCode::CONFLICT,
            ApiError::UnknownField(_)
            | ApiError::NotFilterable(_)
            | ApiError::NotSortable(_)
            | ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Database(_) | ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ApiError::SchemaNotFound => "SCHEMA_NOT_FOUND",
            ApiError::NotFound => "NOT_FOUND",
            ApiError::VersionConflict => "VERSION_CONFLICT",
            ApiError::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            ApiError::UnknownField(_) => "UNKNOWN_FIELD",
            ApiError::NotFilterable(_) => "FIELD_NOT_FILTERABLE",
            ApiError::NotSortable(_) => "FIELD_NOT_SORTABLE",
            ApiError::BadRequest(_) => "BAD_REQUEST",
            ApiError::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            ApiError::Database(_) => "DATABASE_ERROR",
            ApiError::Internal(_) => "INTERNAL_ERROR",
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
}
