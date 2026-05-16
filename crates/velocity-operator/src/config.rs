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
}

impl OperatorConfig {
    pub fn from_env() -> Result<Self> {
        let pg_url = std::env::var("VELOCITY_OPERATOR_PG_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .context("VELOCITY_OPERATOR_PG_URL or DATABASE_URL must be set")?;

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

        Ok(Self {
            pg_url,
            health_addr,
            requeue_after,
            watch_namespace,
            leader_election,
            pretty_logs,
        })
    }
}
