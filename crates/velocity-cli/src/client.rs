//! Shared HTTP client for the data plane.
//!
//! One `reqwest::Client` per CLI invocation, configured once with a
//! base URL, bearer token, and a request-wide timeout (CLAUDE.md ›
//! Inter-Service RPC: timeouts live on the client builder, not per call).
//!
//! Slice 1 only uses `get_version()`. The shape (`ApiClient` +
//! `ApiError` + envelope deserialize) is built now so slice 3's
//! data-plane reads don't reinvent it.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::{header, StatusCode};
use serde::Deserialize;

use crate::config::Context;

/// Default per-request timeout. Matches the API↔warm-reader hop in
/// CLAUDE.md. Configurable per CLI call site only if a slice has a
/// real need; today, nothing does.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Stable error envelope returned by `velocity-api` for every 4xx/5xx.
/// Shape is `{ code, message, request_id }` per CLAUDE.md.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ApiErrorEnvelope {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) request_id: String,
}

/// Error produced by `ApiClient`. We keep two variants — a parsed
/// envelope when the server played by the contract, and a raw-status
/// fallback for the rare path where it didn't (network blip, 502 from
/// the LB, etc.).
#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
    #[error("HTTP {status} {code}: {message}", code = envelope.code, message = envelope.message)]
    Envelope { status: StatusCode, envelope: ApiErrorEnvelope },
    #[error("HTTP {status}: {body}")]
    RawStatus { status: StatusCode, body: String },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
}

#[derive(Debug)]
pub(crate) struct ApiClient {
    base_url: String,
    inner: reqwest::Client,
}

impl ApiClient {
    /// Construct from a resolved `Context`. The token is attached as a
    /// default header so individual call sites can't forget it. Empty
    /// token = unauthenticated — useful for `/version` and the OIDC
    /// callback path; everything else will 401 at the server.
    pub(crate) fn from_context(ctx: &Context) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(DEFAULT_TIMEOUT);
        if !ctx.token.is_empty() {
            let mut headers = header::HeaderMap::new();
            let v = header::HeaderValue::from_str(&format!("Bearer {}", ctx.token))
                .map_err(|e| anyhow!("invalid bearer token: {e}"))?;
            headers.insert(header::AUTHORIZATION, v);
            builder = builder.default_headers(headers);
        }
        let inner = builder.build().map_err(|e| anyhow!("build client: {e}"))?;
        Ok(Self { base_url: ctx.api_url.trim_end_matches('/').to_string(), inner })
    }

    /// Hit `/version`. Unauthenticated. Used by `velocity version` to
    /// prove the URL points at a real Velocity API.
    pub(crate) async fn get_version(&self) -> Result<VersionResponse, ApiError> {
        let url = format!("{}/version", self.base_url);
        let resp = self.inner.get(&url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let v: VersionResponse = resp.json().await?;
            return Ok(v);
        }
        Err(parse_error(status, resp).await)
    }
}

/// Mirrors the API's `/version` JSON. Kept narrow on purpose — fields
/// the CLI doesn't use today aren't deserialised, so adding fields on
/// the server side won't break old binaries.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct VersionResponse {
    pub(crate) service: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) git_sha: String,
    #[serde(default)]
    pub(crate) ready: bool,
}

/// Best-effort envelope parse with a raw-status fallback. Pulled out
/// so future call sites share the same shape.
pub(crate) async fn parse_error(status: StatusCode, resp: reqwest::Response) -> ApiError {
    let body = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<ApiErrorEnvelope>(&body) {
        Ok(envelope) => ApiError::Envelope { status, envelope },
        Err(_) => ApiError::RawStatus { status, body },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let raw = r#"{"code":"WARM_READER_TOKEN_INVALID","message":"oops","request_id":"abc"}"#;
        let env: ApiErrorEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.code, "WARM_READER_TOKEN_INVALID");
        assert_eq!(env.request_id, "abc");
    }

    #[test]
    fn envelope_optional_request_id() {
        let raw = r#"{"code":"X","message":"y"}"#;
        let env: ApiErrorEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.request_id, "");
    }

    #[test]
    fn version_response_tolerates_missing_optional_fields() {
        let raw = r#"{"service":"velocity-api","version":"0.1.0"}"#;
        let v: VersionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(v.service, "velocity-api");
        assert_eq!(v.git_sha, "");
        assert!(!v.ready);
    }

    #[test]
    fn from_context_strips_trailing_slash() {
        let ctx =
            Context { name: "t".into(), api_url: "https://api.example/".into(), token: "x".into() };
        let c = ApiClient::from_context(&ctx).unwrap();
        assert_eq!(c.base_url, "https://api.example");
    }

    #[test]
    fn from_context_rejects_bad_token() {
        // Header value can't contain a CRLF — confirms we surface the
        // problem at config load rather than first 401.
        let ctx = Context {
            name: "t".into(),
            api_url: "https://api".into(),
            token: "bad\r\ntoken".into(),
        };
        let err = ApiClient::from_context(&ctx).unwrap_err();
        assert!(err.to_string().contains("invalid bearer token"));
    }
}
