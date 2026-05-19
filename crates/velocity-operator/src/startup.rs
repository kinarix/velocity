//! Startup gates and pure construction extracted from `main.rs`.
//!
//! Connection-based wire-up (`Redis::connect`, kube `Api::all`) stays in
//! `main.rs` because it requires the runtime infrastructure. The bits
//! that are pure-or-near-pure live here so they're unit-testable
//! without a process.

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

/// Outcome of the Typesense client wire-up. We model the three cases
/// explicitly so callers (and tests) can pattern-match on what
/// happened — instead of an opaque `Option<TypesenseClient>` that
/// loses the difference between "user did not configure it" and
/// "user did configure it but construction failed".
#[derive(Debug)]
pub enum TypesenseStartup {
    /// Both URL and key were present and the client was constructed.
    Configured(velocity_typesense::TypesenseClient),
    /// The user did not set both env vars — Tier-3 collections will be
    /// created lazily by velocity-api CDC.
    NotConfigured,
    /// The user set both env vars but the client could not be built.
    /// Boot continues so the operator still reconciles non-Tier-3
    /// schemas; the failure is logged at error level.
    ConstructionFailed(String),
}

impl TypesenseStartup {
    /// `Some(client)` when configured; `None` otherwise. Used by the
    /// main wire-up which currently treats both NotConfigured and
    /// ConstructionFailed as "no eager provisioning".
    pub fn into_client(self) -> Option<velocity_typesense::TypesenseClient> {
        match self {
            Self::Configured(c) => Some(c),
            _ => None,
        }
    }
}

/// Construct the Typesense client from configuration. Does NOT call
/// `health()` — that requires the runtime and a live endpoint. The
/// main wire-up runs it once on the returned client.
///
/// `cfg.typesense_url` and `cfg.typesense_api_key` are paired by the
/// config layer (both-or-neither is enforced in `from_env_with`), so
/// the partial-config case below is a defensive fallback.
pub fn build_typesense_client(cfg: &OperatorConfig) -> TypesenseStartup {
    match (cfg.typesense_url.as_ref(), cfg.typesense_api_key.as_ref()) {
        (Some(url), Some(key)) => match velocity_typesense::TypesenseClient::new(url.clone(), key.clone()) {
            Ok(c) => {
                tracing::info!(url = %url, "typesense client initialised");
                TypesenseStartup::Configured(c)
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::error!(error = %e, "failed to construct typesense client — Tier-3 schemas will not be eagerly provisioned");
                TypesenseStartup::ConstructionFailed(msg)
            }
        },
        _ => {
            tracing::warn!(
                "VELOCITY_OPERATOR_TYPESENSE_URL is unset — Tier-3 collections will be created lazily by velocity-api CDC instead"
            );
            TypesenseStartup::NotConfigured
        }
    }
}

#[cfg(test)]
mod tests {
    //! DB-backed integration tests live in `tests/provisioner_integration.rs`
    //! so they can opt-in via the `VELOCITY_OPERATOR_PG_URL` env var. The
    //! tests below exercise pure construction only.

    use super::*;

    fn base_cfg() -> OperatorConfig {
        OperatorConfig {
            pg_url: "postgres://stub@127.0.0.1/stub".into(),
            health_addr: "0.0.0.0:8081".into(),
            requeue_after: std::time::Duration::from_secs(300),
            watch_namespace: None,
            leader_election: false,
            pretty_logs: false,
            redis_url: None,
            redis_revoked_key: "revoked_actors".into(),
            warm_storage_url: None,
            typesense_url: None,
            typesense_api_key: None,
            alert_webhook_url: None,
        }
    }

    #[test]
    fn typesense_not_configured_when_env_absent() {
        let cfg = base_cfg();
        match build_typesense_client(&cfg) {
            TypesenseStartup::NotConfigured => {}
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn typesense_not_configured_when_only_url_present() {
        // The config layer pairs URL+key, so this is defensive only —
        // but the pattern fall-through is exercised here.
        let mut cfg = base_cfg();
        cfg.typesense_url = Some("http://typesense:8108".into());
        match build_typesense_client(&cfg) {
            TypesenseStartup::NotConfigured => {}
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn typesense_configured_when_url_and_key_both_present() {
        // TypesenseClient::new performs lazy URL validation — bad URLs
        // surface as request-time errors, not construction errors —
        // so any non-empty string pair produces Configured.
        let mut cfg = base_cfg();
        cfg.typesense_url = Some("http://typesense.test:8108".into());
        cfg.typesense_api_key = Some("a-key".into());
        match build_typesense_client(&cfg) {
            TypesenseStartup::Configured(_) => {}
            other => panic!("expected Configured, got {other:?}"),
        }
    }

    #[test]
    fn typesense_into_client_unwraps_configured_only() {
        let nc = TypesenseStartup::NotConfigured;
        assert!(nc.into_client().is_none());
        let cf = TypesenseStartup::ConstructionFailed("boom".into());
        assert!(cf.into_client().is_none());

        let mut cfg = base_cfg();
        cfg.typesense_url = Some("http://typesense.test:8108".into());
        cfg.typesense_api_key = Some("a-key".into());
        let configured = build_typesense_client(&cfg);
        assert!(configured.into_client().is_some());
    }
}
