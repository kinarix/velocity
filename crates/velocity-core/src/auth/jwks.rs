//! Per-issuer JWKS cache with background refresh.
//!
//! Each registered issuer has its current key set held behind an `ArcSwap`
//! so the verification hot path is a single atomic pointer load — no awaits,
//! no locks. A background task refreshes every [`REFRESH_INTERVAL`].
//!
//! Cold-start posture (advisor decision, ADR-003):
//! - First refresh runs synchronously when the issuer is registered.
//! - If it fails, the issuer is left in [`IssuerStatus::Pending`]; tokens
//!   from it are rejected with `JwksError::IssuerUnavailable`. The
//!   background task keeps retrying.
//! - Once the first fetch succeeds, transient JWKS outages do not flip the
//!   issuer back to Pending — we serve the most recently cached keys until
//!   a successful refresh replaces them.
//!
//! Key rotation:
//! - Cache is logically keyed by `(issuer, kid)`. On a `kid` miss we force
//!   one out-of-band refresh, rate-limited per [`KID_MISS_REFRESH_INTERVAL`],
//!   so a flood of unknown-kid tokens cannot DoS the JWKS endpoint.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use jsonwebtoken::jwk::{Jwk as JsonwtJwk, JwkSet};
use thiserror::Error;
use tokio::sync::Mutex;

/// How long between background refreshes per issuer.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Minimum gap between consecutive forced refreshes triggered by a `kid`
/// miss. Bounds the cost of an attacker spraying tokens with nonsense kids.
pub const KID_MISS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Per-key timeout on the JWKS HTTP fetch.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Re-export of the standard JWK shape so callers don't have to depend on
/// `jsonwebtoken` directly.
pub type Jwk = JsonwtJwk;

#[derive(Debug, Error)]
pub enum JwksError {
    #[error("issuer `{0}` is not registered")]
    UnknownIssuer(String),
    /// First fetch has not yet succeeded — the issuer is configured but
    /// nothing in the cache verifies tokens from it yet.
    #[error("issuer `{0}` has no keys yet (cold-start or persistent failure)")]
    IssuerUnavailable(String),
    /// Issuer is Ready but no JWK matches the requested `kid`, even after a
    /// forced refresh. Either the token is forged or the IdP has revoked
    /// the key since our last successful refresh.
    #[error("issuer `{issuer}`: no JWK for kid `{kid}`")]
    UnknownKid { issuer: String, kid: String },
    #[error("HTTP fetch of `{url}` failed: {source}")]
    Fetch {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("JWKS body from `{url}` did not parse as a JWKS: {source}")]
    Parse {
        url: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssuerStatus {
    /// Has served at least one successful fetch. `keys` is non-empty.
    Ready,
    /// Configured but never fetched successfully. `keys` is empty.
    Pending,
}

/// One issuer's worth of cached JWKS state. The `keys` map is wrapped in
/// `ArcSwap` so [`JwksCache::lookup`] is a single atomic load.
#[derive(Debug)]
struct IssuerState {
    jwks_url: String,
    keys: ArcSwap<HashMap<String, Arc<Jwk>>>,
    status: ArcSwap<IssuerStatus>,
    /// Time of the last `kid`-miss-triggered refresh. Used to rate-limit
    /// the kid-miss path. Plain `Mutex<Instant>` is fine — contention is
    /// rare and the critical section is two instructions.
    last_forced_refresh: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone)]
pub struct IssuerConfig {
    pub issuer: String,
    pub jwks_url: String,
}

/// Multi-issuer JWKS cache. Cheap to clone (just an `Arc`).
#[derive(Debug, Clone)]
pub struct JwksCache {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    issuers: DashMap<String, Arc<IssuerState>>,
    http: reqwest::Client,
}

impl JwksCache {
    /// Build a cache backed by a default reqwest client. Falls back to
    /// `Client::new()` if the builder fails — both paths return a usable
    /// client; the only reason to prefer the builder is the per-request
    /// timeout, so a fallback without the timeout is acceptable here.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { inner: Arc::new(Inner { issuers: DashMap::new(), http }) }
    }

    pub fn with_client(http: reqwest::Client) -> Self {
        Self { inner: Arc::new(Inner { issuers: DashMap::new(), http }) }
    }

    /// Register an issuer and attempt the first fetch. Returns `Ok` even if
    /// the initial fetch fails — the issuer is added in `Pending` state and
    /// the background task will retry. Callers that need to fail-fast can
    /// inspect the returned [`IssuerStatus`].
    pub async fn add_issuer(&self, cfg: IssuerConfig) -> IssuerStatus {
        let state = Arc::new(IssuerState {
            jwks_url: cfg.jwks_url.clone(),
            keys: ArcSwap::from_pointee(HashMap::new()),
            status: ArcSwap::from_pointee(IssuerStatus::Pending),
            last_forced_refresh: Mutex::new(None),
        });
        self.inner.issuers.insert(cfg.issuer.clone(), state.clone());

        // Best-effort first fetch — log on failure but leave the issuer
        // registered in Pending state so the background task can retry.
        match Self::fetch_and_store(&self.inner.http, &state).await {
            Ok(()) => IssuerStatus::Ready,
            Err(e) => {
                tracing::warn!(issuer = %cfg.issuer, error = %e, "JWKS cold-start fetch failed; issuer marked Pending");
                IssuerStatus::Pending
            }
        }
    }

    /// Look up a JWK for `(issuer, kid)`. On `kid` miss against a Ready
    /// issuer, force one refresh (rate-limited) and retry once.
    pub async fn lookup(&self, issuer: &str, kid: &str) -> Result<Arc<Jwk>, JwksError> {
        let state = self
            .inner
            .issuers
            .get(issuer)
            .map(|s| s.clone())
            .ok_or_else(|| JwksError::UnknownIssuer(issuer.to_string()))?;

        if **state.status.load() == IssuerStatus::Pending {
            return Err(JwksError::IssuerUnavailable(issuer.to_string()));
        }

        if let Some(jwk) = state.keys.load().get(kid).cloned() {
            return Ok(jwk);
        }

        // kid miss — force one refresh, rate-limited per
        // KID_MISS_REFRESH_INTERVAL so a bad-kid spray can't DoS the IdP.
        let now = Instant::now();
        let allow_refresh = {
            let mut last = state.last_forced_refresh.lock().await;
            let allow = last.is_none_or(|t| now.duration_since(t) >= KID_MISS_REFRESH_INTERVAL);
            if allow {
                *last = Some(now);
            }
            allow
        };
        if allow_refresh {
            if let Err(e) = Self::fetch_and_store(&self.inner.http, &state).await {
                tracing::warn!(%issuer, error = %e, "kid-miss refresh failed; serving stale keys");
            }
            if let Some(jwk) = state.keys.load().get(kid).cloned() {
                return Ok(jwk);
            }
        }

        Err(JwksError::UnknownKid { issuer: issuer.to_string(), kid: kid.to_string() })
    }

    /// Refresh every registered issuer once. Used by the background loop
    /// and by tests to drive deterministic refreshes.
    pub async fn refresh_all(&self) {
        let states: Vec<_> = self.inner.issuers.iter().map(|e| e.value().clone()).collect();
        for state in states {
            if let Err(e) = Self::fetch_and_store(&self.inner.http, &state).await {
                tracing::warn!(url = %state.jwks_url, error = %e, "JWKS background refresh failed");
            }
        }
    }

    /// Spawn the background refresh loop. Returns a `JoinHandle` so callers
    /// can abort it on shutdown.
    pub fn spawn_refresher(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_INTERVAL);
            // Skip the immediate tick — `add_issuer` already kicked the
            // first fetch.
            tick.tick().await;
            loop {
                tick.tick().await;
                this.refresh_all().await;
            }
        })
    }

    /// Current readiness of `issuer`. Useful for `/readyz` and tests.
    pub fn status_of(&self, issuer: &str) -> Option<IssuerStatus> {
        self.inner.issuers.get(issuer).map(|s| **s.status.load())
    }

    /// Number of JWKs currently cached for `issuer`. Returns 0 for unknown
    /// or Pending issuers.
    pub fn key_count(&self, issuer: &str) -> usize {
        self.inner.issuers.get(issuer).map(|s| s.keys.load().len()).unwrap_or(0)
    }

    /// Fetch the issuer's JWKS URL and swap the result into the state. The
    /// key set is only replaced on a successful parse — a 500 or a malformed
    /// body leaves the previously cached keys in place.
    async fn fetch_and_store(
        http: &reqwest::Client,
        state: &Arc<IssuerState>,
    ) -> Result<(), JwksError> {
        let url = state.jwks_url.clone();
        let resp = http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|source| JwksError::Fetch { url: url.clone(), source })?;
        let bytes =
            resp.bytes().await.map_err(|source| JwksError::Fetch { url: url.clone(), source })?;
        let set: JwkSet =
            serde_json::from_slice(&bytes).map_err(|source| JwksError::Parse { url, source })?;

        let mut map = HashMap::with_capacity(set.keys.len());
        for jwk in set.keys {
            // jsonwebtoken's CommonParameters carries the kid as Option<String>.
            // Skip JWKs without a kid — a kid-less key can't be referenced
            // by a JWT header and would only add noise to lookups.
            if let Some(kid) = jwk.common.key_id.clone() {
                map.insert(kid, Arc::new(jwk));
            }
        }
        state.keys.store(Arc::new(map));
        state.status.store(Arc::new(IssuerStatus::Ready));
        Ok(())
    }
}

impl Default for JwksCache {
    fn default() -> Self {
        Self::new()
    }
}
