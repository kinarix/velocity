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
    /// Phase 5: HMAC key for POST /query keyset cursors. ≥32 bytes.
    /// When `None`, cursor pagination is disabled — first-page reads
    /// still work, but a request carrying `cursor` returns 400.
    pub cursor_signing_key: Option<Vec<u8>>,
    /// Phase 5c: Typesense base URL (e.g. `http://typesense:8108`).
    /// When `None`, Tier-3 schemas are accepted but the CDC worker
    /// logs a warning and stays idle — outbox rows accumulate.
    pub typesense_url: Option<String>,
    /// Phase 5c: Typesense API key. REQUIRED when `typesense_url` is
    /// set; otherwise startup fails (mirrors the warm-reader pairing).
    pub typesense_api_key: Option<String>,
    /// Phase 6a-2: shared secret the `/api/platform/audit*` endpoints
    /// require in `Authorization: Bearer <token>`. When `None`, those
    /// endpoints return 401 to every caller — explicit failure over
    /// silent admission. Minimum 16 chars (parity with
    /// `VELOCITY_WARM_READER_SERVICE_TOKEN`).
    pub platform_audit_token: Option<String>,
}

impl ApiConfig {
    /// Read config from the process environment. Thin wrapper around
    /// `from_env_with` — the function under test is the latter, which
    /// takes an explicit lookup closure so unit tests don't have to
    /// touch process-wide env state.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let pg_url = match get("VELOCITY_API_PG_URL").or_else(|| get("DATABASE_URL")) {
            Some(url) => url,
            None => Self::compose_pg_url(&get)
                .context("VELOCITY_API_PG_URL/DATABASE_URL not set and PG_HOST/PORT/USER/DB/PASSWORD env vars are incomplete")?,
        };

        let bind_addr =
            get("VELOCITY_API_BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let health_addr =
            get("VELOCITY_API_HEALTH_ADDR").unwrap_or_else(|| "0.0.0.0:8081".to_string());
        let watch_namespace = get("VELOCITY_API_NAMESPACE");
        let pg_pool_max = get("VELOCITY_API_PG_POOL_MAX")
            .and_then(|v| v.parse().ok())
            .unwrap_or(16);
        let pretty_logs = get("VELOCITY_API_PRETTY_LOGS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let redis_url = get("VELOCITY_API_REDIS_URL").filter(|v| !v.trim().is_empty());

        let warm_reader_url =
            get("VELOCITY_API_WARM_READER_URL").filter(|v| !v.trim().is_empty());
        let warm_reader_service_token =
            get("VELOCITY_API_WARM_READER_SERVICE_TOKEN").filter(|v| !v.trim().is_empty());
        // Pair them: if a URL is set, demand a token. Allowing
        // unauthenticated calls to the warm reader would let any pod
        // with network access query historical data — fail-loud here.
        if warm_reader_url.is_some() && warm_reader_service_token.is_none() {
            anyhow::bail!(
                "VELOCITY_API_WARM_READER_URL is set but VELOCITY_API_WARM_READER_SERVICE_TOKEN is missing"
            );
        }
        let warm_reader_timeout_ms = get("VELOCITY_API_WARM_READER_TIMEOUT_MS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(15_000);

        let cursor_signing_key = match get("VELOCITY_API_CURSOR_SIGNING_KEY") {
            Some(s) if !s.trim().is_empty() => {
                let bytes = s.into_bytes();
                if bytes.len() < 32 {
                    anyhow::bail!("VELOCITY_API_CURSOR_SIGNING_KEY must be at least 32 bytes");
                }
                Some(bytes)
            }
            _ => None,
        };

        let typesense_url =
            get("VELOCITY_API_TYPESENSE_URL").filter(|v| !v.trim().is_empty());
        let typesense_api_key =
            get("VELOCITY_API_TYPESENSE_API_KEY").filter(|v| !v.trim().is_empty());
        if typesense_url.is_some() && typesense_api_key.is_none() {
            anyhow::bail!(
                "VELOCITY_API_TYPESENSE_URL is set but VELOCITY_API_TYPESENSE_API_KEY is missing"
            );
        }

        let platform_audit_token =
            get("VELOCITY_API_PLATFORM_AUDIT_TOKEN").filter(|v| !v.trim().is_empty());
        if let Some(t) = &platform_audit_token {
            if t.len() < 16 {
                anyhow::bail!(
                    "VELOCITY_API_PLATFORM_AUDIT_TOKEN must be at least 16 characters (got {})",
                    t.len()
                );
            }
        }

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
            cursor_signing_key,
            typesense_url,
            typesense_api_key,
            platform_audit_token,
        })
    }

    fn compose_pg_url(get: &dyn Fn(&str) -> Option<String>) -> Result<String> {
        let host = get("VELOCITY_API_PG_HOST").context("PG_HOST")?;
        let port = get("VELOCITY_API_PG_PORT").unwrap_or_else(|| "5432".into());
        let user = get("VELOCITY_API_PG_USER").context("PG_USER")?;
        let db = get("VELOCITY_API_PG_DB").context("PG_DB")?;
        let password = get("VELOCITY_API_PG_PASSWORD").context("PG_PASSWORD")?;
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::HashMap;

    fn lookup<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| map.get(k).map(|s| s.to_string())
    }

    #[test]
    fn percent_encode_reserved_chars() {
        assert_eq!(percent_encode("plain"), "plain");
        assert_eq!(percent_encode("a:b/c@d"), "a%3Ab%2Fc%40d");
    }

    #[test]
    fn from_env_uses_velocity_pg_url_when_set() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://alpha/db");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.pg_url, "postgres://alpha/db");
        // Defaults populate the rest.
        assert_eq!(cfg.bind_addr, "0.0.0.0:8080");
        assert_eq!(cfg.health_addr, "0.0.0.0:8081");
        assert_eq!(cfg.pg_pool_max, 16);
        assert!(!cfg.pretty_logs);
        assert!(cfg.watch_namespace.is_none());
        assert!(cfg.redis_url.is_none());
        assert_eq!(cfg.warm_reader_timeout_ms, 15_000);
    }

    #[test]
    fn from_env_falls_back_to_database_url() {
        let mut env = HashMap::new();
        env.insert("DATABASE_URL", "postgres://fallback/db");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.pg_url, "postgres://fallback/db");
    }

    #[test]
    fn from_env_composes_pg_url_from_parts() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_HOST", "pg.svc");
        env.insert("VELOCITY_API_PG_USER", "velocity_api");
        env.insert("VELOCITY_API_PG_DB", "velocity");
        env.insert("VELOCITY_API_PG_PASSWORD", "s3cret/with:specials");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        // Password chars `/` and `:` must be percent-encoded.
        assert_eq!(cfg.pg_url, "postgres://velocity_api:s3cret%2Fwith%3Aspecials@pg.svc:5432/velocity");
    }

    #[test]
    fn from_env_compose_pg_url_uses_custom_port() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_HOST", "pg");
        env.insert("VELOCITY_API_PG_PORT", "6432");
        env.insert("VELOCITY_API_PG_USER", "u");
        env.insert("VELOCITY_API_PG_DB", "d");
        env.insert("VELOCITY_API_PG_PASSWORD", "p");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert!(cfg.pg_url.contains(":6432/"), "custom port should appear: {}", cfg.pg_url);
    }

    #[test]
    fn from_env_errors_when_neither_url_nor_parts_present() {
        let env = HashMap::new();
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("VELOCITY_API_PG_URL/DATABASE_URL"), "{msg}");
    }

    #[test]
    fn from_env_warm_url_without_token_fails_loud() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_WARM_READER_URL", "http://wr:9090");
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("SERVICE_TOKEN is missing"));
    }

    #[test]
    fn from_env_warm_token_blank_treated_as_unset() {
        // Trim-whitespace filter — whitespace-only token is treated as
        // None, which (paired with a set URL) trips the error path.
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_WARM_READER_URL", "http://wr");
        env.insert("VELOCITY_API_WARM_READER_SERVICE_TOKEN", "   ");
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("SERVICE_TOKEN is missing"));
    }

    #[test]
    fn from_env_warm_reader_pair_accepted() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_WARM_READER_URL", "http://wr:9090");
        env.insert("VELOCITY_API_WARM_READER_SERVICE_TOKEN", "a-token");
        env.insert("VELOCITY_API_WARM_READER_TIMEOUT_MS", "5000");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.warm_reader_url.as_deref(), Some("http://wr:9090"));
        assert_eq!(cfg.warm_reader_service_token.as_deref(), Some("a-token"));
        assert_eq!(cfg.warm_reader_timeout_ms, 5000);
    }

    #[test]
    fn from_env_short_cursor_signing_key_rejected() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_CURSOR_SIGNING_KEY", "tooshort");
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("at least 32 bytes"));
    }

    #[test]
    fn from_env_cursor_signing_key_accepted_when_long_enough() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_CURSOR_SIGNING_KEY", "a-very-long-cursor-signing-key-32+");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        let bytes = cfg.cursor_signing_key.unwrap();
        assert!(bytes.len() >= 32);
    }

    #[test]
    fn from_env_typesense_url_without_key_fails_loud() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_TYPESENSE_URL", "http://typesense:8108");
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("TYPESENSE_API_KEY is missing"));
    }

    #[test]
    fn from_env_typesense_pair_accepted() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_TYPESENSE_URL", "http://typesense:8108");
        env.insert("VELOCITY_API_TYPESENSE_API_KEY", "xyz");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.typesense_url.as_deref(), Some("http://typesense:8108"));
        assert_eq!(cfg.typesense_api_key.as_deref(), Some("xyz"));
    }

    #[test]
    fn from_env_platform_audit_token_too_short_rejected() {
        // Mirrors the warm-reader policy: a sub-16-char shared secret is
        // trivially brute-forceable in a credential-stuffing scenario.
        // Fail-loud at startup over silently accepting a weak token.
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_PLATFORM_AUDIT_TOKEN", "short");
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("at least 16 characters"));
    }

    #[test]
    fn from_env_platform_audit_token_blank_treated_as_unset() {
        // Whitespace-only token should be None (audit endpoint will 401)
        // rather than be accepted as a valid short token.
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_PLATFORM_AUDIT_TOKEN", "   ");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert!(cfg.platform_audit_token.is_none());
    }

    #[test]
    fn from_env_platform_audit_token_accepted() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert(
            "VELOCITY_API_PLATFORM_AUDIT_TOKEN",
            "a-secure-audit-token-1234567890",
        );
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(
            cfg.platform_audit_token.as_deref(),
            Some("a-secure-audit-token-1234567890")
        );
    }

    #[test]
    fn from_env_pretty_logs_truthy_values() {
        for v in ["1", "true", "TRUE", "True"] {
            let mut env = HashMap::new();
            env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
            env.insert("VELOCITY_API_PRETTY_LOGS", v);
            let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
            assert!(cfg.pretty_logs, "value {v:?} should be truthy");
        }
    }

    #[test]
    fn from_env_pg_pool_max_invalid_falls_back_to_default() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_API_PG_POOL_MAX", "not-a-number");
        let cfg = ApiConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.pg_pool_max, 16, "invalid value should fall back");
    }

    #[test]
    fn from_env_compose_pg_url_missing_required_part_errors() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_PG_HOST", "pg");
        // user, db, password absent — each `?` produces an Err.
        let err = ApiConfig::from_env_with(lookup(&env)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("VELOCITY_API_PG_URL/DATABASE_URL"));
    }

    #[test]
    fn from_env_wrapper_is_invokable() {
        // Wrapper calls std::env::var — just exercising it for coverage
        // is fine; the wrapper's behavior is identity over the closure.
        let _ = ApiConfig::from_env();
    }
}
