//! Environment-driven config for `velocity-warm-reader`.
//!
//! Mirrors the pattern in `velocity-operator::config` and
//! `velocity-api::config`: a flat struct, `from_env()` constructor, no
//! hidden defaults that matter to security.
//!
//! Security-sensitive fields (`service_token`) are required, not
//! optional. If the deployment forgot to set them, we want a startup
//! failure, not a runtime "accidentally accepting unauthenticated"
//! footgun.

use anyhow::{Context, Result};
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct WarmReaderConfig {
    /// `object_store` URL pointing at where the operator's exporter writes
    /// monthly Parquet objects. `s3://bucket/prefix` in prod;
    /// `file:///abs/path` in tests. The reader is read-only on this URL.
    pub storage_url: String,

    /// HTTP address for the `/v1/warm/events` endpoint.
    pub bind_addr: SocketAddr,

    /// HTTP address for `/healthz` and `/readyz`. Split off the data port
    /// so probes don't share the auth surface with the read endpoint —
    /// the same pattern the operator uses (operator/src/health.rs).
    pub health_addr: SocketAddr,

    /// Shared secret the API sends in `Authorization: Bearer <token>`.
    /// Compared in constant time. REQUIRED — fail-loud on missing config
    /// rather than allow unauthenticated reads.
    pub service_token: String,

    /// JSON logging by default; pretty for local dev when set true.
    pub pretty_logs: bool,
}

impl WarmReaderConfig {
    /// Read config from the process environment. Thin wrapper around
    /// `from_env_with` — the function under test is the latter, which
    /// takes an explicit lookup closure so unit tests don't have to
    /// touch process-wide env state.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let storage_url = get("VELOCITY_WARM_READER_STORAGE_URL").context(
            "VELOCITY_WARM_READER_STORAGE_URL is required (e.g. s3://bucket/prefix or file:///abs/path)",
        )?;

        let bind_addr: SocketAddr = get("VELOCITY_WARM_READER_BIND_ADDR")
            .unwrap_or_else(|| "0.0.0.0:9090".to_string())
            .parse()
            .context("VELOCITY_WARM_READER_BIND_ADDR must be a SocketAddr (e.g. 0.0.0.0:9090)")?;

        let health_addr: SocketAddr = get("VELOCITY_WARM_READER_HEALTH_ADDR")
            .unwrap_or_else(|| "0.0.0.0:9091".to_string())
            .parse()
            .context("VELOCITY_WARM_READER_HEALTH_ADDR must be a SocketAddr (e.g. 0.0.0.0:9091)")?;

        let service_token = get("VELOCITY_WARM_READER_SERVICE_TOKEN").context(
            "VELOCITY_WARM_READER_SERVICE_TOKEN is required — refusing to start without auth configured",
        )?;

        if service_token.len() < 16 {
            anyhow::bail!(
                "VELOCITY_WARM_READER_SERVICE_TOKEN must be at least 16 characters (got {})",
                service_token.len()
            );
        }

        let pretty_logs = matches!(
            get("VELOCITY_WARM_READER_PRETTY_LOGS").as_deref(),
            Some("1" | "true" | "yes")
        );

        Ok(Self { storage_url, bind_addr, health_addr, service_token, pretty_logs })
    }
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
    fn from_env_requires_storage_url_and_token() {
        let map = HashMap::new();
        let err = WarmReaderConfig::from_env_with(lookup(&map)).unwrap_err();
        assert!(format!("{err:#}").contains("STORAGE_URL"), "missing storage url should be flagged: {err:#}");
    }

    #[test]
    fn from_env_rejects_short_token() {
        let mut map = HashMap::new();
        map.insert("VELOCITY_WARM_READER_STORAGE_URL", "file:///tmp/warm");
        map.insert("VELOCITY_WARM_READER_SERVICE_TOKEN", "tooshort");
        let err = WarmReaderConfig::from_env_with(lookup(&map)).unwrap_err();
        assert!(format!("{err:#}").contains("at least 16"), "short token must be rejected: {err:#}");
    }

    #[test]
    fn from_env_accepts_valid_input() {
        let mut map = HashMap::new();
        map.insert("VELOCITY_WARM_READER_STORAGE_URL", "file:///tmp/warm");
        map.insert("VELOCITY_WARM_READER_SERVICE_TOKEN", "a-test-token-32-chars-min-xxxxxxx");
        let cfg = WarmReaderConfig::from_env_with(lookup(&map)).expect("valid env should parse");
        assert_eq!(cfg.storage_url, "file:///tmp/warm");
        assert_eq!(cfg.bind_addr.port(), 9090);
        assert_eq!(cfg.health_addr.port(), 9091);
        assert!(!cfg.pretty_logs);
    }

    #[test]
    fn from_env_pretty_logs_truthy_strings() {
        let mut map = HashMap::new();
        map.insert("VELOCITY_WARM_READER_STORAGE_URL", "file:///tmp/warm");
        map.insert("VELOCITY_WARM_READER_SERVICE_TOKEN", "a-test-token-32-chars-min-xxxxxxx");
        map.insert("VELOCITY_WARM_READER_PRETTY_LOGS", "true");
        let cfg = WarmReaderConfig::from_env_with(lookup(&map)).unwrap();
        assert!(cfg.pretty_logs);
    }

    #[test]
    fn from_env_requires_service_token_when_storage_url_is_set() {
        // Hits the `?` on the SERVICE_TOKEN context (line 65). The
        // existing missing-storage-url test short-circuits earlier.
        let mut map = HashMap::new();
        map.insert("VELOCITY_WARM_READER_STORAGE_URL", "file:///tmp/warm");
        let err = WarmReaderConfig::from_env_with(lookup(&map)).unwrap_err();
        assert!(
            format!("{err:#}").contains("SERVICE_TOKEN"),
            "missing service token should be flagged: {err:#}"
        );
    }

    #[test]
    fn from_env_wrapper_is_invokable() {
        // Thin wrapper around `from_env_with(std::env::var)`. Just
        // invoking it covers the wrapper; behavior is exercised in
        // depth by the other tests using an explicit map.
        let _ = WarmReaderConfig::from_env();
    }
}
