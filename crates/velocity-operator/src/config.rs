//! Operator configuration loaded from environment.

use std::time::Duration;

use anyhow::{Context as _, Result};

/// Runtime configuration for the operator. All values come from env vars so
/// the same binary works in `cargo run` and in-cluster.
#[derive(Debug, Clone)]
pub struct OperatorConfig {
    /// Postgres URL the operator uses (role: `velocity_operator`).
    pub pg_url: String,
    /// Address for the health server (`/healthz`, `/readyz`).
    pub health_addr: String,
    /// How often a reconciler re-checks its objects after a successful run.
    pub requeue_after: Duration,
    /// Watched namespace, or `None` for cluster-wide.
    pub watch_namespace: Option<String>,
    /// Whether to enable leader election (no-op in Phase 0; placeholder).
    pub leader_election: bool,
    /// Pretty logs (true) vs JSON logs (false; default for production).
    pub pretty_logs: bool,
    /// Redis URL for actor revocation publishing — `redis://host:port`.
    /// `None` runs the RoleBinding reconciler in DB-only mode (handy for
    /// dev environments without Redis). Production always sets this.
    pub redis_url: Option<String>,
    /// Override for the revocation set key. Defaults to `revoked_actors`,
    /// matching `velocity_api::auth::DEFAULT_REVOKED_SET_KEY`.
    pub redis_revoked_key: String,
    /// `object_store` URL where the tiering exporter writes warm-tier
    /// Parquet objects (Phase 4.2). `s3://bucket/prefix` in prod,
    /// `file:///abs/path` in dev/CI. When `None`, the exporter logs a
    /// warning at startup and stays idle — partitions stay in the hot
    /// tier until the operator is reconfigured. We deliberately don't
    /// fail-loud: warm-tier is optional configuration, unlike
    /// `velocity-warm-reader` which can't run without it.
    pub warm_storage_url: Option<String>,
    /// Typesense base URL, e.g. `http://typesense:8108` (Phase 5d-2).
    /// When set, the SchemaDefinition reconciler creates per-schema
    /// Typesense collections eagerly for `search.tier: Tier3` schemas.
    /// When `None`, the operator skips eager provisioning and the API's
    /// CDC worker falls back to lazy creation. Dev / single-tier
    /// installs can leave this unset.
    pub typesense_url: Option<String>,
    /// API key sent as `X-TYPESENSE-API-KEY` on every operator-side
    /// Typesense call. Required when `typesense_url` is set; missing
    /// here while `typesense_url` is set fails boot.
    pub typesense_api_key: Option<String>,
}

impl OperatorConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let pg_url = match get("VELOCITY_OPERATOR_PG_URL").or_else(|| get("DATABASE_URL")) {
            Some(url) => url,
            None => Self::compose_pg_url(&get)
                .context("VELOCITY_OPERATOR_PG_URL/DATABASE_URL not set and PG_HOST/PORT/USER/DB/PASSWORD env vars are incomplete")?,
        };

        let health_addr =
            get("VELOCITY_OPERATOR_HEALTH_ADDR").unwrap_or_else(|| "0.0.0.0:8081".to_string());

        let requeue_after = get("VELOCITY_OPERATOR_REQUEUE_SECS")
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(300));

        let watch_namespace = get("VELOCITY_OPERATOR_NAMESPACE");
        let leader_election = get("VELOCITY_OPERATOR_LEADER_ELECTION")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let pretty_logs = get("VELOCITY_OPERATOR_PRETTY_LOGS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let redis_url = get("VELOCITY_OPERATOR_REDIS_URL");
        let redis_revoked_key =
            get("VELOCITY_OPERATOR_REDIS_REVOKED_KEY").unwrap_or_else(|| "revoked_actors".into());

        let warm_storage_url = get("VELOCITY_OPERATOR_WARM_STORAGE_URL");

        let typesense_url = get("VELOCITY_OPERATOR_TYPESENSE_URL");
        let typesense_api_key = get("VELOCITY_OPERATOR_TYPESENSE_API_KEY");
        if typesense_url.is_some() && typesense_api_key.is_none() {
            anyhow::bail!(
                "VELOCITY_OPERATOR_TYPESENSE_URL is set but VELOCITY_OPERATOR_TYPESENSE_API_KEY is missing"
            );
        }

        Ok(Self {
            pg_url,
            health_addr,
            requeue_after,
            watch_namespace,
            leader_election,
            pretty_logs,
            redis_url,
            redis_revoked_key,
            warm_storage_url,
            typesense_url,
            typesense_api_key,
        })
    }

    /// Build `postgres://user:password@host:port/db` from the piecewise env vars
    /// used by the Helm chart. The password is percent-encoded so reserved
    /// URL chars (`:/@?#[]`) round-trip cleanly.
    fn compose_pg_url(get: &dyn Fn(&str) -> Option<String>) -> Result<String> {
        let host = get("VELOCITY_OPERATOR_PG_HOST").context("PG_HOST")?;
        let port = get("VELOCITY_OPERATOR_PG_PORT").unwrap_or_else(|| "5432".into());
        let user = get("VELOCITY_OPERATOR_PG_USER").context("PG_USER")?;
        let db = get("VELOCITY_OPERATOR_PG_DB").context("PG_DB")?;
        let password = get("VELOCITY_OPERATOR_PG_PASSWORD").context("PG_PASSWORD")?;
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

/// Minimal percent-encoder for the URL "userinfo" component. Encodes every
/// byte that's not in the unreserved set or sub-delims allowed inside userinfo.
/// (See RFC 3986 §3.2.1.)
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
        assert_eq!(percent_encode("pass#word?"), "pass%23word%3F");
    }

    #[test]
    fn from_env_uses_pg_url_when_set() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://primary/db");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.pg_url, "postgres://primary/db");
        assert_eq!(cfg.health_addr, "0.0.0.0:8081");
        assert_eq!(cfg.requeue_after, Duration::from_secs(300));
        assert!(!cfg.leader_election);
        assert!(!cfg.pretty_logs);
        assert!(cfg.redis_url.is_none());
        assert_eq!(cfg.redis_revoked_key, "revoked_actors");
    }

    #[test]
    fn from_env_falls_back_to_database_url() {
        let mut env = HashMap::new();
        env.insert("DATABASE_URL", "postgres://fallback/db");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.pg_url, "postgres://fallback/db");
    }

    #[test]
    fn from_env_composes_pg_url_and_percent_encodes_password() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_HOST", "pg");
        env.insert("VELOCITY_OPERATOR_PG_USER", "vel_op");
        env.insert("VELOCITY_OPERATOR_PG_DB", "velocity");
        env.insert("VELOCITY_OPERATOR_PG_PASSWORD", "pa:ss/word");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.pg_url, "postgres://vel_op:pa%3Ass%2Fword@pg:5432/velocity");
    }

    #[test]
    fn from_env_composes_pg_url_with_custom_port() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_HOST", "pg");
        env.insert("VELOCITY_OPERATOR_PG_PORT", "6432");
        env.insert("VELOCITY_OPERATOR_PG_USER", "u");
        env.insert("VELOCITY_OPERATOR_PG_DB", "d");
        env.insert("VELOCITY_OPERATOR_PG_PASSWORD", "p");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert!(cfg.pg_url.contains(":6432/"));
    }

    #[test]
    fn from_env_compose_missing_required_part_errors() {
        let env = HashMap::new();
        let err = OperatorConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("env vars are incomplete"));
    }

    #[test]
    fn from_env_typesense_url_without_key_fails_loud() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_OPERATOR_TYPESENSE_URL", "http://typesense:8108");
        let err = OperatorConfig::from_env_with(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("TYPESENSE_API_KEY is missing"));
    }

    #[test]
    fn from_env_typesense_pair_accepted() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_OPERATOR_TYPESENSE_URL", "http://typesense:8108");
        env.insert("VELOCITY_OPERATOR_TYPESENSE_API_KEY", "key");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.typesense_url.as_deref(), Some("http://typesense:8108"));
        assert_eq!(cfg.typesense_api_key.as_deref(), Some("key"));
    }

    #[test]
    fn from_env_leader_and_pretty_logs_truthy_values() {
        for v in ["1", "true", "TRUE"] {
            let mut env = HashMap::new();
            env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://x/y");
            env.insert("VELOCITY_OPERATOR_LEADER_ELECTION", v);
            env.insert("VELOCITY_OPERATOR_PRETTY_LOGS", v);
            let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
            assert!(cfg.leader_election, "value {v:?} should be truthy");
            assert!(cfg.pretty_logs, "value {v:?} should be truthy");
        }
    }

    #[test]
    fn from_env_requeue_secs_invalid_falls_back_to_default() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_OPERATOR_REQUEUE_SECS", "not-a-number");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.requeue_after, Duration::from_secs(300));
    }

    #[test]
    fn from_env_requeue_secs_valid_parsed() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_OPERATOR_REQUEUE_SECS", "60");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.requeue_after, Duration::from_secs(60));
    }

    #[test]
    fn from_env_custom_revoked_key() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_OPERATOR_PG_URL", "postgres://x/y");
        env.insert("VELOCITY_OPERATOR_REDIS_REVOKED_KEY", "custom_revoked");
        let cfg = OperatorConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.redis_revoked_key, "custom_revoked");
    }

    #[test]
    fn from_env_wrapper_is_invokable() {
        let _ = OperatorConfig::from_env();
    }
}
