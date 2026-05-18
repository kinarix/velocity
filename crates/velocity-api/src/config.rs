//! API server configuration loaded from environment.

use anyhow::{Context as _, Result};

/// Runtime configuration for the API. All values come from env vars so
/// the same binary works in `cargo run` and in-cluster.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Postgres URL — connects as `velocity_api` (NOBYPASSRLS, ADR-007).
    pub pg_url: String,
    /// Public API bind address (e.g. `0.0.0.0:8080`).
    pub bind_addr: String,
    /// Health server bind address — separate so probes survive a saturated
    /// public listener.
    pub health_addr: String,
    /// Watched namespace, or `None` for cluster-wide.
    pub watch_namespace: Option<String>,
    /// Maximum DB pool size.
    pub pg_pool_max: u32,
    /// Pretty logs (true) vs JSON logs (false; default for production).
    pub pretty_logs: bool,
    /// Optional Redis URL for the actor revocation backend (ADR-003).
    /// When unset, no revocation checker is wired and the middleware
    /// admits every actor — the startup log emits a warning so the gap is
    /// visible. When set, [`crate::auth::RedisRevocationChecker`] is
    /// connected and the strategy's `revocation_fail_open` flag governs
    /// behaviour on backend errors.
    pub redis_url: Option<String>,
    /// URL of `velocity-warm-reader` for time-machine queries whose
    /// `at` falls outside the hot window (Phase 4.4). Example:
    /// `http://velocity-warm-reader.platform.svc:9090`. When `None`,
    /// any warm-tier time-machine request returns 503
    /// `WARM_TIER_NOT_CONFIGURED` — explicit failure, never silent fall
    /// back to "no events" (ADR-003 fail-closed).
    pub warm_reader_url: Option<String>,
    /// Bearer token sent to `velocity-warm-reader`. Must match
    /// `VELOCITY_WARM_READER_SERVICE_TOKEN` on the reader. REQUIRED
    /// when `warm_reader_url` is set; otherwise startup fails.
    pub warm_reader_service_token: Option<String>,
    /// Per-request timeout for warm-reader calls. Default 15s
    /// (CLAUDE.md §Inter-Service RPC). Configurable so a slow warm
    /// path can be given more budget without code changes.
    pub warm_reader_timeout_ms: u64,
}

impl ApiConfig {
    pub fn from_env() -> Result<Self> {
        let pg_url = match std::env::var("VELOCITY_API_PG_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
        {
            Ok(url) => url,
            Err(_) => Self::compose_pg_url()
                .context("VELOCITY_API_PG_URL/DATABASE_URL not set and PG_HOST/PORT/USER/DB/PASSWORD env vars are incomplete")?,
        };

        let bind_addr =
            std::env::var("VELOCITY_API_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let health_addr = std::env::var("VELOCITY_API_HEALTH_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8081".to_string());
        let watch_namespace = std::env::var("VELOCITY_API_NAMESPACE").ok();
        let pg_pool_max = std::env::var("VELOCITY_API_PG_POOL_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16);
        let pretty_logs = std::env::var("VELOCITY_API_PRETTY_LOGS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let redis_url = std::env::var("VELOCITY_API_REDIS_URL")
            .ok()
            .filter(|v| !v.trim().is_empty());

        let warm_reader_url = std::env::var("VELOCITY_API_WARM_READER_URL")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let warm_reader_service_token = std::env::var("VELOCITY_API_WARM_READER_SERVICE_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        // Pair them: if a URL is set, demand a token. Allowing
        // unauthenticated calls to the warm reader would let any pod
        // with network access query historical data — fail-loud here.
        if warm_reader_url.is_some() && warm_reader_service_token.is_none() {
            anyhow::bail!(
                "VELOCITY_API_WARM_READER_URL is set but VELOCITY_API_WARM_READER_SERVICE_TOKEN is missing"
            );
        }
        let warm_reader_timeout_ms = std::env::var("VELOCITY_API_WARM_READER_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15_000);

        Ok(Self {
            pg_url,
            bind_addr,
            health_addr,
            watch_namespace,
            pg_pool_max,
            pretty_logs,
            redis_url,
            warm_reader_url,
            warm_reader_service_token,
            warm_reader_timeout_ms,
        })
    }

    fn compose_pg_url() -> Result<String> {
        let host = std::env::var("VELOCITY_API_PG_HOST").context("PG_HOST")?;
        let port = std::env::var("VELOCITY_API_PG_PORT").unwrap_or_else(|_| "5432".into());
        let user = std::env::var("VELOCITY_API_PG_USER").context("PG_USER")?;
        let db = std::env::var("VELOCITY_API_PG_DB").context("PG_DB")?;
        let password = std::env::var("VELOCITY_API_PG_PASSWORD").context("PG_PASSWORD")?;
        Ok(format!(
            "postgres://{}:{}@{}:{}/{}",
            percent_encode(&user),
            percent_encode(&password),
            host,
            port,
            db
        ))
    }
}

fn percent_encode(s: &str) -> String {
    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
    }
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if is_unreserved(*b) {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn percent_encode_reserved_chars() {
        assert_eq!(percent_encode("plain"), "plain");
        assert_eq!(percent_encode("a:b/c@d"), "a%3Ab%2Fc%40d");
    }
}
