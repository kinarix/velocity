//! Env-driven processor configuration.
//!
//! Reads from environment so the same binary is shippable to multiple
//! environments without a config file rewrite — Helm sets the env vars,
//! the binary reads them. The single file-shaped piece of config is
//! the policy bundle path, which the operator writes via a ConfigMap.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ProcessorConfig {
    /// `host:port` for the inbound HTTP receiver. Collectors POST
    /// `/v1/logs` here.
    pub bind_addr: String,
    /// Path to the YAML bundle written by the operator's reconciler.
    pub policy_path: PathBuf,
    /// Bearer token the collector must present. Required — fail loud
    /// instead of accepting unauthenticated input.
    pub ingest_token: String,
    /// Polling cadence for policy reload. 30s is fine — the operator
    /// reconciles on CRD change and the file appears within a second
    /// or two, but the processor doesn't need sub-minute latency
    /// here (filters apply on subsequent log lines, not retroactively).
    pub policy_reload_secs: u64,
}

impl ProcessorConfig {
    pub fn from_env() -> Result<Self> {
        let bind_addr =
            std::env::var("VELOCITY_LP_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
        let policy_path: PathBuf = std::env::var("VELOCITY_LP_POLICY_PATH")
            .unwrap_or_else(|_| "/etc/velocity/log-policies.yaml".to_string())
            .into();
        let ingest_token = std::env::var("VELOCITY_LP_INGEST_TOKEN")
            .context("VELOCITY_LP_INGEST_TOKEN required")?;
        if ingest_token.len() < 16 {
            return Err(anyhow!(
                "VELOCITY_LP_INGEST_TOKEN must be at least 16 chars (got {})",
                ingest_token.len()
            ));
        }
        let policy_reload_secs = std::env::var("VELOCITY_LP_POLICY_RELOAD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        Ok(Self { bind_addr, policy_path, ingest_token, policy_reload_secs })
    }
}

/// Pure validation helper extracted from `from_env` so we can exercise
/// the gate without `unsafe` env mutation in tests (the workspace
/// forbids `unsafe-code`).
pub fn validate_token(token: &str) -> Result<()> {
    if token.len() < 16 {
        return Err(anyhow!(
            "VELOCITY_LP_INGEST_TOKEN must be at least 16 chars (got {})",
            token.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rejects_short_token() {
        assert!(validate_token("short").is_err());
    }

    #[test]
    fn accepts_long_token() {
        assert!(validate_token("abcdefghijklmnopqrst").is_ok());
    }
}
