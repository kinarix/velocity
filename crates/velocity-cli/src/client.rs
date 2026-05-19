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

    /// Fetch a single record by id. Path is the canonical
    /// `org/app/domain/object/version` shape — same as the URL the API
    /// exposes, no rewriting on the CLI side.
    pub(crate) async fn get_record(
        &self,
        path: &SchemaPath,
        id: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}/api/{}/{}", self.base_url, path.as_url(), id);
        let resp = self.inner.get(&url).send().await?;
        decode_json(resp).await
    }

    /// LIST with optional query-string params (limit/cursor passed via
    /// query, filters via the POST `/query` endpoint instead — see
    /// `query_records`). Returns the raw envelope `{ items, next_cursor }`.
    pub(crate) async fn list_records(
        &self,
        path: &SchemaPath,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<ListEnvelope, ApiError> {
        let mut url = format!("{}/api/{}", self.base_url, path.as_url());
        let mut sep = '?';
        if let Some(l) = limit {
            url.push(sep);
            url.push_str(&format!("limit={l}"));
            sep = '&';
        }
        if let Some(c) = cursor {
            url.push(sep);
            url.push_str("cursor=");
            url.push_str(c);
        }
        let resp = self.inner.get(&url).send().await?;
        decode_json(resp).await
    }

    /// POST a query DSL body (`{ limit, cursor, sort, filter }`). The
    /// CLI doesn't validate the shape — the server is the source of
    /// truth, and lockstep validation would mean a CLI rebuild whenever
    /// the DSL grows a field. We forward bytes and surface server errors.
    pub(crate) async fn query_records(
        &self,
        path: &SchemaPath,
        body: &serde_json::Value,
    ) -> Result<ListEnvelope, ApiError> {
        let url = format!("{}/api/{}/query", self.base_url, path.as_url());
        let resp = self.inner.post(&url).json(body).send().await?;
        decode_json(resp).await
    }

    /// `GET /{path}/{id}/history`. Two modes (chosen by query params):
    /// list events (newest-first, paginated) when `at` is absent;
    /// reconstruct state at a point-in-time when `at` is set.
    pub(crate) async fn get_history(
        &self,
        path: &SchemaPath,
        id: &str,
        limit: Option<u32>,
        before: Option<&str>,
        at: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        let mut url = format!("{}/api/{}/{}/history", self.base_url, path.as_url(), id);
        let mut sep = '?';
        if let Some(l) = limit {
            url.push(sep);
            url.push_str(&format!("limit={l}"));
            sep = '&';
        }
        if let Some(b) = before {
            url.push(sep);
            url.push_str("before=");
            url.push_str(b);
            sep = '&';
        }
        if let Some(a) = at {
            url.push(sep);
            url.push_str("at=");
            url.push_str(a);
        }
        let resp = self.inner.get(&url).send().await?;
        decode_json(resp).await
    }

    /// `POST /{path}/{id}/restore` with `{ at, reason }`. Restoring is a
    /// write — the server creates a new event in `platform.event_log`
    /// representing the rollback; older history is preserved.
    pub(crate) async fn post_restore(
        &self,
        path: &SchemaPath,
        id: &str,
        at: &str,
        reason: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}/api/{}/{}/restore", self.base_url, path.as_url(), id);
        let body = match reason {
            Some(r) => serde_json::json!({ "at": at, "reason": r }),
            None => serde_json::json!({ "at": at }),
        };
        let resp = self.inner.post(&url).json(&body).send().await?;
        decode_json(resp).await
    }

    /// `GET /{path}/{id}/archive` — fetch a single archived record.
    pub(crate) async fn get_archive(
        &self,
        path: &SchemaPath,
        id: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}/api/{}/{}/archive", self.base_url, path.as_url(), id);
        let resp = self.inner.get(&url).send().await?;
        decode_json(resp).await
    }

    /// `POST /{path}/archive/query` — DSL against the archive store.
    /// Accepts `{ limit, cursor, archivedAfter }` (camelCase per
    /// archive_handlers.rs). CLI forwards bytes, server validates.
    pub(crate) async fn query_archive(
        &self,
        path: &SchemaPath,
        body: &serde_json::Value,
    ) -> Result<ListEnvelope, ApiError> {
        let url = format!("{}/api/{}/archive/query", self.base_url, path.as_url());
        let resp = self.inner.post(&url).json(body).send().await?;
        decode_json(resp).await
    }

    /// `POST /{path}/{id}/unarchive` — restore the archived row to the
    /// hot table. 410 ARCHIVE_HOT_ROW_PURGED when the row is already
    /// gone past `purgeAfter`.
    pub(crate) async fn post_unarchive(
        &self,
        path: &SchemaPath,
        id: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}/api/{}/{}/unarchive", self.base_url, path.as_url(), id);
        let resp = self.inner.post(&url).send().await?;
        decode_json(resp).await
    }

    /// `GET /metrics` — Prometheus exposition format text. Most
    /// production deployments expose this on a side-listener (the
    /// health server in `velocity-api/src/health.rs`); same-listener
    /// is also valid and is what `cargo run` does by default. The
    /// CLI honours `--metrics-url` to point elsewhere if needed.
    pub(crate) async fn get_metrics_raw(&self, override_url: Option<&str>) -> Result<String, ApiError> {
        let url = match override_url {
            Some(u) => u.to_string(),
            None => format!("{}/metrics", self.base_url),
        };
        let resp = self.inner.get(&url).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp.text().await?);
        }
        Err(parse_error(status, resp).await)
    }
}

/// 5-segment data-plane path. Validates shape eagerly so a typo at the
/// command line doesn't reach the API and come back as a confusing 404.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaPath {
    pub org: String,
    pub app: String,
    pub domain: String,
    pub object: String,
    pub version: String,
}

impl SchemaPath {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 5 {
            anyhow::bail!(
                "schema path `{s}` must have 5 segments: org/app/domain/object/version"
            );
        }
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() {
                anyhow::bail!("schema path segment #{} is empty in `{s}`", i + 1);
            }
        }
        Ok(Self {
            org: parts[0].into(),
            app: parts[1].into(),
            domain: parts[2].into(),
            object: parts[3].into(),
            version: parts[4].into(),
        })
    }

    pub(crate) fn as_url(&self) -> String {
        format!("{}/{}/{}/{}/{}", self.org, self.app, self.domain, self.object, self.version)
    }
}

/// List/query response shape (matches `velocity-api`'s `ListEnvelope`).
/// We deserialise to `serde_json::Value` for items so the CLI doesn't
/// know about per-schema fields — server is the truth.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListEnvelope {
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
    #[serde(default, rename = "nextCursor", alias = "next_cursor")]
    pub next_cursor: Option<String>,
}

async fn decode_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, ApiError> {
    let status = resp.status();
    if status.is_success() {
        let v: T = resp.json().await?;
        return Ok(v);
    }
    Err(parse_error(status, resp).await)
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
