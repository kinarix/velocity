//! Hourly drift detection. Same orphan-table logic the CLI's `velocity
//! drift check` uses, but runs continuously inside the operator so
//! Prometheus + alertmanager can fire on regressions.
//!
//! What counts as drift in v1:
//!   - Orphan table: a row in `pg_class` whose `(nspname, relname)`
//!     pair is NOT claimed by any currently-applied SchemaDefinition
//!     CRD. Column drift and missing-index detection follow.
//!
//! What this is NOT:
//!   - A reconciler. Drift is logged + counted, never auto-fixed.
//!     Auto-fix on orphan tables is dangerous — a transient kube
//!     informer hiccup that drops a SchemaDefinition for a few seconds
//!     would otherwise quarantine live data. Humans run `velocity
//!     drift quarantine` when they've confirmed the orphan.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use kube::api::ListParams;
use kube::{Api, Client};
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use velocity_types::common::sanitize;
use velocity_types::crds::schema::SchemaDefinition;

use crate::metrics;

/// How often the sweep runs. The constant is `pub` so tests can read it
/// and so the value shows up next to the prometheus counter for ops
/// dashboards that want to reason about expected emission cadence.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Background sweep task. Runs forever; intended to be spawned from
/// `main` alongside the controllers. Respects `shutdown_rx` so process
/// exit can drain it cleanly.
pub async fn run(
    pool: PgPool,
    client: Client,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    // Run once at startup so the first observation arrives soon after
    // operator boot rather than an hour later. Subsequent ticks happen
    // on the scheduled cadence.
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Err(e) = sweep_once(&pool, &client).await {
                    tracing::warn!(error = %e, "drift sweep failed");
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("drift sweep shutting down");
                    return Ok(());
                }
            }
        }
    }
}

/// Single sweep iteration. Public so an integration test can exercise
/// the full DB+kube path without spinning the timer.
pub async fn sweep_once(pool: &PgPool, client: &Client) -> Result<DriftReport> {
    let expected = expected_tables(client).await?;
    if expected.is_empty() {
        // No SchemaDefinitions yet — nothing to compare. Don't emit
        // counter ticks because "no schemas declared" isn't drift.
        return Ok(DriftReport::default());
    }
    let schemas: Vec<String> =
        expected.iter().map(|(s, _)| s.clone()).collect::<HashSet<_>>().into_iter().collect();
    let actual = actual_tables(pool, &schemas).await?;

    let mut orphans: Vec<(String, String)> = Vec::new();
    for (s, t) in actual {
        if !expected.contains(&(s.clone(), t.clone())) {
            orphans.push((s, t));
        }
    }

    if !orphans.is_empty() {
        metrics::drift_detected_total()
            .with_label_values(&["orphan_table"])
            .inc_by(orphans.len() as u64);
        for (s, t) in &orphans {
            tracing::warn!(
                pg_schema = %s,
                table = %t,
                kind = "orphan_table",
                "drift: table not claimed by any SchemaDefinition"
            );
        }
    }

    Ok(DriftReport { expected_count: expected.len(), actual_count: 0, orphans })
}

/// Sweep result. The integration test asserts on these counts; ops
/// dashboards observe via the prometheus counter, not this struct.
#[derive(Debug, Default)]
pub struct DriftReport {
    pub expected_count: usize,
    pub actual_count: usize,
    pub orphans: Vec<(String, String)>,
}

async fn expected_tables(client: &Client) -> Result<HashSet<(String, String)>> {
    let api: Api<SchemaDefinition> = Api::all(client.clone());
    let list = api
        .list(&ListParams::default())
        .await
        .context("listing SchemaDefinitions for drift sweep")?;

    let mut out: HashSet<(String, String)> = HashSet::new();
    for sd in list.items {
        let labels = sd.metadata.labels.clone().unwrap_or_default();
        let (Some(org), Some(app), Some(domain)) = (
            labels.get("velocity.sh/org"),
            labels.get("velocity.sh/app"),
            labels.get("velocity.sh/domain"),
        ) else {
            // Operator hasn't reconciled labels yet; skip rather than
            // guess. Next sweep will pick it up once labels are set.
            continue;
        };
        let pg_schema = sanitize(&format!("{org}_{app}_{domain}"));
        let Some(object) = sd.metadata.name.as_ref() else {
            continue;
        };
        let table = format!("{}_{}", sanitize(object), sanitize(&sd.spec.version));
        out.insert((pg_schema.clone(), table.clone()));
        out.insert((pg_schema.clone(), format!("{table}_history")));
        out.insert((pg_schema, format!("{table}_outbox")));
    }
    Ok(out)
}

async fn actual_tables(pool: &PgPool, schemas: &[String]) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT n.nspname::text AS pg_schema, c.relname::text AS table_name \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname = ANY($1)",
    )
    .bind(schemas)
    .fetch_all(pool)
    .await
    .context("listing actual tables for drift sweep")?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("pg_schema"), r.get::<String, _>("table_name")))
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn drift_report_default_is_empty() {
        let r = DriftReport::default();
        assert_eq!(r.expected_count, 0);
        assert_eq!(r.actual_count, 0);
        assert!(r.orphans.is_empty());
    }

    #[test]
    fn sweep_interval_is_hourly() {
        // Sanity guard against accidental tuning that breaks alerting.
        assert_eq!(SWEEP_INTERVAL.as_secs(), 3600);
    }
}
