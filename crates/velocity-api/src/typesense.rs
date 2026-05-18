//! Minimal Typesense HTTP client — Phase 5c.
//!
//! We do not depend on the official `typesense` Rust crate because (a)
//! it is an alpha-grade wrapper, (b) it pulls a v3 dependency on
//! `serde_with` that conflicts with workspace versions, and (c) we
//! exercise five endpoints total. A 200-line client is cheaper than the
//! transitive risk.
//!
//! Operations we use:
//!   - `create_collection`        — lazy provisioning on first write
//!   - `collection_exists`        — existence check
//!   - `upsert`                   — single-doc upsert (CDC main path)
//!   - `delete`                   — single-doc delete (CDC on `delete` op)
//!   - `search`                   — query-time read endpoint
//!
//! Auth: `X-TYPESENSE-API-KEY` header on every request. Timeouts come
//! from the shared `reqwest::Client` configured at construction time
//! (CLAUDE.md › Inter-Service RPC). Errors are surfaced loudly — never
//! "search returned empty" on a 5xx (ADR-003 fail-closed).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Thin wrapper around `reqwest::Client` with a pre-bound base URL and
/// API key. Clone-cheap (Arc internally via reqwest).
#[derive(Debug, Clone)]
pub struct TypesenseClient {
    base: String,
    api_key: String,
    http: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum TypesenseError {
    #[error("typesense: http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("typesense: status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("typesense: decode: {0}")]
    Decode(String),
}

impl TypesenseClient {
    /// Construct a client. `base_url` must include scheme + host (and
    /// optional port), e.g. `http://typesense:8108`. The connect /
    /// total timeouts apply to every call — picked conservatively to
    /// match Phase 4's warm-reader client (15s default).
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, TypesenseError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { base: base_url.into(), api_key: api_key.into(), http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// Best-effort health check. Used at startup to log warnings if
    /// the configured URL is unreachable — we don't fail boot on it
    /// because Tier-3 outbox CDC will retry once the host returns.
    pub async fn health(&self) -> Result<bool, TypesenseError> {
        let r = self
            .http
            .get(self.url("/health"))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        Ok(r.status().is_success())
    }

    /// Return `true` if the collection exists. Used by the CDC worker
    /// before its first upsert to a new collection (no schema-wide
    /// provisioning step in v1 — lazy creation is good enough).
    pub async fn collection_exists(&self, name: &str) -> Result<bool, TypesenseError> {
        let r = self
            .http
            .get(self.url(&format!("/collections/{name}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(false);
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(true)
    }

    /// Create a collection. `fields` defines the index schema. We
    /// always include `id` (string) and `__schema` (string facet) so
    /// the cross-schema collection can carry rows from many schemas.
    pub async fn create_collection(
        &self,
        spec: &CollectionSpec,
    ) -> Result<(), TypesenseError> {
        let r = self
            .http
            .post(self.url("/collections"))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .json(spec)
            .send()
            .await?;
        if r.status().as_u16() == 409 {
            // Already exists — race between two API replicas. Idempotent.
            return Ok(());
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Upsert a single document. Caller is responsible for ensuring
    /// the `id` field is a string (Typesense's document key type).
    pub async fn upsert(&self, collection: &str, doc: &Value) -> Result<(), TypesenseError> {
        let r = self
            .http
            .post(self.url(&format!("/collections/{collection}/documents?action=upsert")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .json(doc)
            .send()
            .await?;
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Delete a single document by id. Returns `Ok(())` on 404 — the
    /// outbox can replay deletes and they must be idempotent.
    pub async fn delete(&self, collection: &str, id: &str) -> Result<(), TypesenseError> {
        let r = self
            .http
            .delete(self.url(&format!("/collections/{collection}/documents/{id}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(());
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Search a collection. Returns Typesense's raw response — caller
    /// extracts `hits[]` and projects per its needs.
    pub async fn search(
        &self,
        collection: &str,
        params: &SearchParams,
    ) -> Result<Value, TypesenseError> {
        let qs = params.to_query_string();
        let r = self
            .http
            .get(self.url(&format!("/collections/{collection}/documents/search?{qs}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        r.json::<Value>().await.map_err(|e| TypesenseError::Decode(e.to_string()))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionSpec {
    pub name: String,
    pub fields: Vec<TsField>,
    /// Field used to sort by default. Optional; if set, must exist
    /// in `fields` and be `int32`/`int64`/`float`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_sorting_field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsField {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub q: String,
    /// Comma-separated field names to query across, e.g. `"title,body"`.
    pub query_by: String,
    /// `field:value` filter, AND'd if multiple are present.
    pub filter_by: Option<String>,
    /// e.g. `"created_at:desc"`.
    pub sort_by: Option<String>,
    pub per_page: Option<u32>,
    pub page: Option<u32>,
}

impl SearchParams {
    fn to_query_string(&self) -> String {
        let mut parts = vec![
            format!("q={}", urlencode(&self.q)),
            format!("query_by={}", urlencode(&self.query_by)),
        ];
        if let Some(v) = &self.filter_by {
            parts.push(format!("filter_by={}", urlencode(v)));
        }
        if let Some(v) = &self.sort_by {
            parts.push(format!("sort_by={}", urlencode(v)));
        }
        if let Some(v) = self.per_page {
            parts.push(format!("per_page={v}"));
        }
        if let Some(v) = self.page {
            parts.push(format!("page={v}"));
        }
        parts.join("&")
    }
}

/// Minimal URL-encoder for query string values. We deliberately don't
/// pull `urlencoding` for one call site; this handles the bytes we
/// care about (alphanumeric + `-._~` pass; everything else gets
/// `%HH`-encoded).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_string_encodes_special_chars() {
        let p = SearchParams {
            q: "steel widget".into(),
            query_by: "title,body".into(),
            filter_by: Some("status:approved".into()),
            sort_by: Some("created_at:desc".into()),
            per_page: Some(10),
            page: Some(2),
        };
        let qs = p.to_query_string();
        assert!(qs.contains("q=steel%20widget"));
        assert!(qs.contains("query_by=title%2Cbody"));
        assert!(qs.contains("filter_by=status%3Aapproved"));
        assert!(qs.contains("sort_by=created_at%3Adesc"));
        assert!(qs.contains("per_page=10"));
        assert!(qs.contains("page=2"));
    }

    #[test]
    fn url_join_handles_trailing_slash() {
        let c = TypesenseClient::new("http://localhost:8108/", "k").unwrap();
        assert_eq!(c.url("/collections"), "http://localhost:8108/collections");
    }
}
