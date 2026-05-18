//! Error envelope returned by the warm-reader HTTP API.
//!
//! Shape mirrors the API's existing error envelope so log/trace
//! correlation across the inter-service hop is uniform (CLAUDE.md
//! §Inter-Service RPC: "Error envelope").

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WarmReaderError {
    #[error("missing Authorization header")]
    AuthMissing,
    #[error("malformed Authorization header")]
    AuthMalformed,
    #[error("invalid service token")]
    AuthInvalid,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("warm storage I/O: {0}")]
    Storage(String),

    #[error("parquet decode: {0}")]
    Parquet(String),

    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub code: &'static str,
    pub message: String,
    pub request_id: Option<String>,
}

impl WarmReaderError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::AuthMissing => "AUTH_MISSING",
            Self::AuthMalformed => "AUTH_MALFORMED",
            Self::AuthInvalid => "INVALID_SERVICE_TOKEN",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Storage(_) => "WARM_STORAGE_UNAVAILABLE",
            Self::Parquet(_) => "WARM_PARQUET_DECODE_FAILED",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::AuthMissing | Self::AuthMalformed | Self::AuthInvalid => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Parquet(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for WarmReaderError {
    fn into_response(self) -> Response {
        // Log every 5xx with the error chain; auth failures are noisy and
        // would otherwise spam logs at warn — keep them at debug.
        match self.status() {
            StatusCode::UNAUTHORIZED => {
                tracing::debug!(code = self.code(), error = %self, "warm-reader auth rejection");
            }
            s if s.is_server_error() => {
                tracing::error!(code = self.code(), error = %self, "warm-reader server error");
            }
            _ => {
                tracing::warn!(code = self.code(), error = %self, "warm-reader client error");
            }
        }
        let body = ErrorEnvelope {
            code: self.code(),
            message: self.to_string(),
            request_id: None,
        };
        (self.status(), Json(body)).into_response()
    }
}
