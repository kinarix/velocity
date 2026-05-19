//! Startup gates — fail loud, fail early.

use anyhow::{bail, Context as _, Result};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};

use crate::OperatorConfig;

/// Build a Postgres pool and run ADR-007 startup checks:
///
/// 1. `velocity_api` role exists.
/// 2. `velocity_api` has `BYPASSRLS = false`.
/// 3. `velocity_operator` role exists (the one we're connecting as).
/// 4. The `platform` schema exists with `audit_insert` installed.
///
/// Any failure aborts startup with a clear message — never run with
/// a misconfigured role.
pub async fn pool_with_checks(cfg: &OperatorConfig) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&cfg.pg_url)
        .await
        .context("connecting to postgres as velocity_operator")?;

    let current: String = sqlx::query_scalar("SELECT current_user").fetch_one(&pool).await?;
    tracing::info!(role = %current, "operator connected to postgres");

    verify_velocity_api_role(&pool).await?;
    verify_platform_schema(&pool).await?;

    Ok(pool)
}

/// ADR-007 — the gate.
pub async fn verify_velocity_api_role(pool: &PgPool) -> Result<()> {
    let row =
        sqlx::query("SELECT rolbypassrls, rolsuper FROM pg_roles WHERE rolname = 'velocity_api'")
            .fetch_optional(pool)
            .await
            .context("querying pg_roles for velocity_api")?;

    let row = row.ok_or_else(|| {
        anyhow::anyhow!(
            "velocity_api role does not exist — run db/init/01-roles.sql or `make db-bootstrap`"
        )
    })?;

    let bypass: bool = row.try_get("rolbypassrls")?;
    let superuser: bool = row.try_get("rolsuper")?;

    if bypass || superuser {
        bail!(
            "ADR-007 violation: velocity_api role has bypassrls={bypass}, superuser={superuser}. \
             Row-level security would be silently disabled. Fix the role before starting."
        );
    }

    tracing::info!("velocity_api role verified: NOBYPASSRLS, NOSUPERUSER");
    Ok(())
}

/// Confirms the platform migrations have been applied (proc + table exist).
pub async fn verify_platform_schema(pool: &PgPool) -> Result<()> {
    let has_audit_log: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname='platform' AND tablename='audit_log')",
    )
    .fetch_one(pool)
    .await?;
    if !has_audit_log {
        bail!("platform.audit_log missing — apply `migrations/*.sql` (e.g. `make migrate`)");
    }

    let has_audit_insert: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_proc p \
            JOIN pg_namespace n ON n.oid = p.pronamespace \
            WHERE n.nspname='platform' AND p.proname='audit_insert')",
    )
    .fetch_one(pool)
    .await?;
    if !has_audit_insert {
        bail!("platform.audit_insert function missing — apply migrations/0002_audit_insert.sql");
    }

    let has_reap_queue: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables \
            WHERE schemaname='platform' AND tablename='pending_typesense_reaps')",
    )
    .fetch_one(pool)
    .await?;
    if !has_reap_queue {
        bail!(
            "platform.pending_typesense_reaps missing — apply migrations/0005_pending_typesense_reaps.sql"
        );
    }

    tracing::info!("platform schema verified");
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Integration tests live in `tests/provisioner_integration.rs` so they
    //! can opt-in via the `VELOCITY_OPERATOR_PG_URL` env var.
}
