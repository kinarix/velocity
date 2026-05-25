//! OIDC discovery (`.well-known/openid-configuration`) client.
//!
//! When an `AuthStrategy` of kind `oidc` carries a `configUrl`, the auth
//! informer fetches that URL once at strategy load time and uses the
//! returned document to fill in any endpoint fields the operator did not
//! pin explicitly.
//!
//! Discovery is deliberately a load-time concern, not a request-time one.
//! Once an `AuthStrategy` is registered the resolved endpoints live in
//! [`crate::auth::ResolvedAuthStrategy`] and the hot path never touches
//! the IdP's discovery doc — so a discovery doc that flips after the fact
//! has no effect until the operator re-applies the CRD. That preserves
//! the "pinned redirect target" property the docs promise; the only
//! difference vs the original posture is that the operator pins them
//! once on apply instead of the human pasting them in by hand.
//!
//! The cache is tiny (one entry per distinct `configUrl`) and exists so
//! that several `AuthStrategy` CRDs sharing the same IdP only pay the
//! round-trip once across a tight burst of informer events.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Mutex;

/// How long a successful discovery doc is reused before refetching. Short
/// enough that an operator who wants the new value can `kubectl annotate`
/// to trigger an informer event within minutes; long enough that an
/// AuthStrategy storm doesn't hammer the IdP.
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Per-request timeout on the discovery HTTP fetch. Matches JWKS so a
/// flaky IdP behaves consistently across both code paths.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("HTTP fetch of `{url}` failed: {source}")]
    Fetch {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("discovery endpoint `{url}` returned HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("discovery body from `{url}` did not parse as JSON: {source}")]
    Parse {
        url: String,
        #[source]
        source: reqwest::Error,
    },
}

/// The subset of OIDC discovery fields Velocity reads. We deliberately
/// ignore the rest — anything the platform doesn't act on stays out of
/// the type so a malicious or malformed extension can't surprise us.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcDiscovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    pub jwks_uri: String,
}

/// Cache of discovery docs, keyed by `configUrl`. Cloneable handle —
/// internally an `Arc<Mutex<HashMap<...>>>` so multiple informer events
/// landing in parallel share one fetch per URL.
#[derive(Debug, Clone)]
pub struct DiscoveryCache {
    http: reqwest::Client,
    inner: Arc<Mutex<HashMap<String, CacheEntry>>>,
    ttl: Duration,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    doc: OidcDiscovery,
    fetched_at: Instant,
}

impl DiscoveryCache {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self::with_client(http)
    }

    pub fn with_client(http: reqwest::Client) -> Self {
        Self { http, inner: Arc::new(Mutex::new(HashMap::new())), ttl: CACHE_TTL }
    }

    /// Override the cache TTL — used by tests to force refresh.
    #[cfg(test)]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Fetch the discovery doc, returning a cached copy when one is still
    /// fresh. The mutex is held across the HTTP fetch so concurrent
    /// callers for the same URL share a single round-trip.
    pub async fn fetch(&self, url: &str) -> Result<OidcDiscovery, DiscoveryError> {
        let mut guard = self.inner.lock().await;
        if let Some(entry) = guard.get(url) {
            if entry.fetched_at.elapsed() < self.ttl {
                return Ok(entry.doc.clone());
            }
        }
        let doc = fetch_once(&self.http, url).await?;
        guard.insert(url.to_string(), CacheEntry { doc: doc.clone(), fetched_at: Instant::now() });
        Ok(doc)
    }
}

impl Default for DiscoveryCache {
    fn default() -> Self {
        Self::new()
    }
}

async fn fetch_once(http: &reqwest::Client, url: &str) -> Result<OidcDiscovery, DiscoveryError> {
    let resp = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|source| DiscoveryError::Fetch { url: url.to_string(), source })?;

    if !resp.status().is_success() {
        return Err(DiscoveryError::Status { url: url.to_string(), status: resp.status().as_u16() });
    }

    resp.json::<OidcDiscovery>()
        .await
        .map_err(|source| DiscoveryError::Parse { url: url.to_string(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_discovery_doc() {
        let body = r#"{
            "issuer": "https://idp.example.com",
            "authorization_endpoint": "https://idp.example.com/oauth2/authorize",
            "token_endpoint": "https://idp.example.com/oauth2/token",
            "jwks_uri": "https://idp.example.com/.well-known/jwks.json"
        }"#;
        let doc: OidcDiscovery = serde_json::from_str(body).unwrap();
        assert_eq!(doc.issuer, "https://idp.example.com");
        assert_eq!(doc.authorization_endpoint, "https://idp.example.com/oauth2/authorize");
        assert_eq!(doc.token_endpoint, "https://idp.example.com/oauth2/token");
        assert!(doc.userinfo_endpoint.is_none());
        assert_eq!(doc.jwks_uri, "https://idp.example.com/.well-known/jwks.json");
    }

    #[test]
    fn parses_discovery_doc_with_userinfo_and_ignores_unknown_fields() {
        let body = r#"{
            "issuer": "https://idp.example.com",
            "authorization_endpoint": "https://idp.example.com/oauth2/authorize",
            "token_endpoint": "https://idp.example.com/oauth2/token",
            "userinfo_endpoint": "https://idp.example.com/oauth2/userinfo",
            "jwks_uri": "https://idp.example.com/.well-known/jwks.json",
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code"],
            "id_token_signing_alg_values_supported": ["RS256"]
        }"#;
        let doc: OidcDiscovery = serde_json::from_str(body).unwrap();
        assert_eq!(doc.userinfo_endpoint.as_deref(), Some("https://idp.example.com/oauth2/userinfo"));
    }

    #[tokio::test]
    async fn fetch_returns_cached_doc_within_ttl() {
        // Spin up a tiny http server that counts hits.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let body = r#"{
            "issuer": "https://idp.example.com",
            "authorization_endpoint": "https://idp.example.com/a",
            "token_endpoint": "https://idp.example.com/t",
            "jwks_uri": "https://idp.example.com/jwks"
        }"#;
        let hits_for_server = hits.clone();
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match server.accept().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                hits_for_server.fetch_add(1, Ordering::SeqCst);
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let cache = DiscoveryCache::new();
        let url = format!("http://{addr}/.well-known/openid-configuration");
        let a = cache.fetch(&url).await.unwrap();
        let b = cache.fetch(&url).await.unwrap();
        assert_eq!(a.issuer, b.issuer);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "second call must hit cache");
    }
}
