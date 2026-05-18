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
        let pg_url = match std::env::var("VELOCITY_OPERATOR_PG_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
        {
            Ok(url) => url,
            Err(_) => Self::compose_pg_url()
                .context("VELOCITY_OPERATOR_PG_URL/DATABASE_URL not set and PG_HOST/PORT/USER/DB/PASSWORD env vars are incomplete")?,
        };

        let health_addr = std::env::var("VELOCITY_OPERATOR_HEALTH_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8081".to_string());

        let requeue_after = std::env::var("VELOCITY_OPERATOR_REQUEUE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(300));

        let watch_namespace = std::env::var("VELOCITY_OPERATOR_NAMESPACE").ok();
        let leader_election = std::env::var("VELOCITY_OPERATOR_LEADER_ELECTION")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let pretty_logs = std::env::var("VELOCITY_OPERATOR_PRETTY_LOGS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let redis_url = std::env::var("VELOCITY_OPERATOR_REDIS_URL").ok();
        let redis_revoked_key = std::env::var("VELOCITY_OPERATOR_REDIS_REVOKED_KEY")
            .unwrap_or_else(|_| "revoked_actors".to_string());

        let warm_storage_url = std::env::var("VELOCITY_OPERATOR_WARM_STORAGE_URL").ok();

        let typesense_url = std::env::var("VELOCITY_OPERATOR_TYPESENSE_URL").ok();
        let typesense_api_key = std::env::var("VELOCITY_OPERATOR_TYPESENSE_API_KEY").ok();
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
    fn compose_pg_url() -> Result<String> {
        let host = std::env::var("VELOCITY_OPERATOR_PG_HOST").context("PG_HOST")?;
        let port = std::env::var("VELOCITY_OPERATOR_PG_PORT").unwrap_or_else(|_| "5432".into());
        let user = std::env::var("VELOCITY_OPERATOR_PG_USER").context("PG_USER")?;
        let db = std::env::var("VELOCITY_OPERATOR_PG_DB").context("PG_DB")?;
        let password = std::env::var("VELOCITY_OPERATOR_PG_PASSWORD").context("PG_PASSWORD")?;
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
    use super::percent_encode;

    #[test]
    fn percent_encode_reserved_chars() {
        assert_eq!(percent_encode("plain"), "plain");
        assert_eq!(percent_encode("a:b/c@d"), "a%3Ab%2Fc%40d");
        assert_eq!(percent_encode("pass#word?"), "pass%23word%3F");
    }
}
