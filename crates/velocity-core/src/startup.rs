//! Startup gates and pure wire-up extracted from `main.rs`.
//!
//! - `pool_with_checks` enforces the ADR-007 NOBYPASSRLS / NOSUPERUSER gate.
//! - `parse_flow_cookie_key` turns an env value into the OIDC HMAC key
//!   per the same rules main.rs used (≥32 bytes, hard fail on short value,
//!   warn-and-empty on absence).
//! - `build_oidc_http_client` builds the bounded-timeout reqwest client
//!   that the auth callback uses.
//!
//! (The data-plane `build_tiered_reader` lives in `velocity-data-api`.)
//!
//! The API connects as `velocity_api`. That role MUST be NOBYPASSRLS and
//! NOSUPERUSER — otherwise row-level security would be silently disabled and
//! the whole multi-tenant story collapses. We verify at startup and abort on
//! violation.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};

use crate::ApiConfig;

/// Build a Postgres pool and assert ADR-007 invariants.
pub async fn pool_with_checks(cfg: &ApiConfig) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(cfg.pg_pool_max)
        .connect(&cfg.pg_url)
        .await
        .context("connecting to postgres as velocity_api")?;

    let current: String = sqlx::query_scalar("SELECT current_user").fetch_one(&pool).await?;
    tracing::info!(role = %current, "api connected to postgres");

    verify_role_no_bypass(&pool).await?;

    Ok(pool)
}

/// ADR-007 — fail-stop gate. The connection role must be NOBYPASSRLS and
/// NOSUPERUSER. Anything else means RLS is a no-op and we refuse to run.
pub async fn verify_role_no_bypass(pool: &PgPool) -> Result<()> {
    let row =
        sqlx::query("SELECT rolbypassrls, rolsuper FROM pg_roles WHERE rolname = current_user")
            .fetch_optional(pool)
            .await
            .context("querying pg_roles for current_user")?;

    let row = row.ok_or_else(|| anyhow::anyhow!("current_user has no pg_roles entry"))?;
    let bypass: bool = row.try_get("rolbypassrls")?;
    let superuser: bool = row.try_get("rolsuper")?;

    if bypass || superuser {
        bail!(
            "ADR-007 violation: API role has bypassrls={bypass}, superuser={superuser}. \
             Row-level security would be silently disabled. Fix the role before starting."
        );
    }

    tracing::info!("API role verified: NOBYPASSRLS, NOSUPERUSER");
    Ok(())
}

/// Resolve the OIDC flow-cookie HMAC key from an env lookup.
///
/// Three cases (matching what main.rs has always done):
///   - present and ≥32 bytes: returned as `Arc<Vec<u8>>`.
///   - present and <32 bytes: hard error — refuse to start. A short HMAC
///     key would silently weaken the cookie signature.
///   - absent: log a warning and return an empty key. Non-OIDC
///     deployments do not need to set it; the empty key forces
///     `encode_flow_cookie` to error on any /auth/login attempt, which
///     surfaces as 500 — never a silently-admitted unsigned cookie.
pub fn parse_flow_cookie_key(get: impl Fn(&str) -> Option<String>) -> Result<Arc<Vec<u8>>> {
    match get("VELOCITY_API_FLOW_COOKIE_KEY") {
        Some(s) if s.len() >= 32 => Ok(Arc::new(s.into_bytes())),
        Some(_) => {
            bail!("VELOCITY_API_FLOW_COOKIE_KEY must be at least 32 bytes — refusing to start")
        }
        None => {
            tracing::warn!(
                "VELOCITY_API_FLOW_COOKIE_KEY not set — /auth/login will reject every request"
            );
            Ok(Arc::new(Vec::new()))
        }
    }
}

/// Build the bounded-timeout reqwest client used for OIDC token + JWKS
/// calls. Per CLAUDE.md §Inter-Service RPC: timeouts MUST be set on the
/// Client itself, not per-call, so there is no path through the code
/// that ships without an upper bound.
pub fn build_oidc_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(3))
        .build()
        .context("building OIDC http client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| map.get(k).map(|s| s.to_string())
    }

    #[test]
    fn parse_flow_cookie_key_accepts_at_least_32_bytes() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_FLOW_COOKIE_KEY", "x".repeat(32).leak() as &str);
        let key = parse_flow_cookie_key(lookup(&env)).unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn parse_flow_cookie_key_accepts_longer_value() {
        let long = "x".repeat(64);
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_FLOW_COOKIE_KEY", long.as_str());
        let key = parse_flow_cookie_key(lookup(&env)).unwrap();
        assert_eq!(key.len(), 64);
    }

    #[test]
    fn parse_flow_cookie_key_rejects_short_value() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_API_FLOW_COOKIE_KEY", "tooshort");
        let err = parse_flow_cookie_key(lookup(&env)).unwrap_err();
        assert!(format!("{err:#}").contains("at least 32 bytes"));
    }

    #[test]
    fn parse_flow_cookie_key_absent_yields_empty_placeholder() {
        let env: HashMap<&str, &str> = HashMap::new();
        let key = parse_flow_cookie_key(lookup(&env)).unwrap();
        assert!(key.is_empty(), "absent key returns empty placeholder, not error");
    }

    #[tokio::test]
    async fn build_oidc_http_client_produces_a_client() {
        // Smoke test — we can't reasonably make a real HTTP call here,
        // but constructing the client exercises the timeout builder
        // chain and proves it returns Ok.
        let c = build_oidc_http_client().expect("client should build");
        // Reqwest doesn't expose its configured timeout; the fact that
        // `build()` succeeded is the assertion.
        drop(c);
    }
}
