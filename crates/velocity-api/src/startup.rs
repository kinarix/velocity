//! Startup gates — fail loud, fail early (ADR-007).
//!
//! The API connects as `velocity_api`. That role MUST be NOBYPASSRLS and
//! NOSUPERUSER — otherwise row-level security would be silently disabled and
//! the whole multi-tenant story collapses. We verify at startup and abort on
//! violation.

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
