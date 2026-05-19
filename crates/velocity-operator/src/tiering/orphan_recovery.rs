//! Startup-time orphan detection for warm-tier Parquet objects.
//!
//! The exporter's main loop is already self-healing for in-process
//! crashes: each tick runs the full export-then-drop sequence inside
//! one transaction; the Parquet writes are idempotent over the same
//! object key, so a crash mid-flight just makes the next tick redo
//! the work. We don't *need* a separate recovery path for that case.
//!
//! What we DO need is visibility into orphans created by paths we
//! can't fully control:
//!   - Operator restarted mid-tick on a partition that had multiple
//!     `schema_org` shards — some objects were written, some weren't.
//!     Steady-state is fine, but until the next successful tick
//!     finishes, we have partial coverage on disk.
//!   - A human DETACH'd a partition manually but never re-ran the
//!     exporter, so the hot table is gone but no warm objects exist.
//!   - A human dropped the warm bucket prefix while the hot partition
//!     was already DROPPED — data loss, but we surface the gap so an
//!     operator can replay from a backup.
//!
//! This module logs orphans loudly at startup; it does NOT modify
//! state. The exporter's regular tick converges the steady-state on
//! its own.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use futures::TryStreamExt;
use object_store::ObjectStore;
use sqlx::PgPool;

/// One orphan classification result.
#[derive(Debug)]
pub struct OrphanReport {
    /// Warm objects present whose hot partition still exists. These
    /// are harmless after the next tick (the writer overwrites and
    /// then DETACHes), but we log them so a stuck operator is visible.
    pub warm_with_hot_still_present: Vec<String>,
    /// Hot partitions older than retention whose warm coverage is
    /// partial or missing. The next tick converges these — the value
    /// of reporting is "we don't silently sit on a missed migration".
    pub partitions_missing_warm_coverage: Vec<String>,
}

/// Run the scan and log results. Returns the report so callers can
/// react if they want, but the operator's main loop only needs to
/// `let _ =` it.
pub async fn scan(pool: &PgPool, warm_store: Arc<dyn ObjectStore>) -> Result<OrphanReport> {
    let warm_keys = list_warm_keys(warm_store).await?;
    let hot_partitions = list_hot_partitions(pool).await?;

    let warm_with_hot: Vec<String> = warm_keys
        .iter()
        .filter_map(|k| {
            partition_name_from_key(k).filter(|p| hot_partitions.contains(p)).map(|_| k.clone())
        })
        .collect();

    // For "missing warm coverage" we only flag partitions OLDER than
    // the export horizon — current/next month never need warm objects.
    let cutoff = Utc::now() - chrono::Duration::days(super::exporter::HOT_RETENTION_DAYS);
    let warm_partition_names: HashSet<String> =
        warm_keys.iter().filter_map(|k| partition_name_from_key(k)).collect();
    let missing: Vec<String> = hot_partitions
        .iter()
        .filter(|p| partition_is_older_than(p, cutoff) && !warm_partition_names.contains(*p))
        .cloned()
        .collect();

    if !warm_with_hot.is_empty() {
        tracing::warn!(
            count = warm_with_hot.len(),
            sample = ?warm_with_hot.iter().take(3).collect::<Vec<_>>(),
            "orphan scan: warm objects present whose hot partition still exists — exporter tick will converge",
        );
    }
    if !missing.is_empty() {
        tracing::warn!(
            count = missing.len(),
            partitions = ?missing,
            "orphan scan: hot partitions older than retention with no warm coverage — next exporter tick will export",
        );
    }
    if warm_with_hot.is_empty() && missing.is_empty() {
        tracing::info!("orphan scan: no warm/hot drift detected");
    }

    Ok(OrphanReport {
        warm_with_hot_still_present: warm_with_hot,
        partitions_missing_warm_coverage: missing,
    })
}

async fn list_warm_keys(store: Arc<dyn ObjectStore>) -> Result<Vec<String>> {
    let mut stream = store.list(None);
    let mut out = Vec::new();
    while let Some(item) = stream.try_next().await.context("listing warm objects")? {
        out.push(item.location.to_string());
    }
    Ok(out)
}

async fn list_hot_partitions(pool: &PgPool) -> Result<HashSet<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT c.relname::text \
         FROM pg_inherits i \
         JOIN pg_class c ON c.oid = i.inhrelid \
         JOIN pg_class p ON p.oid = i.inhparent \
         JOIN pg_namespace n ON n.oid = p.relnamespace \
         WHERE n.nspname = 'platform' AND p.relname = 'event_log'",
    )
    .fetch_all(pool)
    .await
    .context("listing event_log partitions")?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// Extract `event_log_YYYY_MM` from a warm object key like
/// `acme/supply/procurement/event_log_2026_03.parquet`. Returns None
/// for keys that don't match this layout (e.g. a stray file someone
/// uploaded).
fn partition_name_from_key(key: &str) -> Option<String> {
    let basename = key.rsplit('/').next()?;
    let stem = basename.strip_suffix(".parquet")?;
    if stem.starts_with("event_log_") && stem.len() == "event_log_YYYY_MM".len() {
        Some(stem.to_string())
    } else {
        None
    }
}

/// True if `event_log_YYYY_MM` names a partition wholly older than
/// `cutoff`. Conservative: anything we can't parse is treated as
/// "not older" so we don't accidentally flag something we don't
/// understand.
fn partition_is_older_than(name: &str, cutoff: chrono::DateTime<Utc>) -> bool {
    let Some(stem) = name.strip_prefix("event_log_") else {
        return false;
    };
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() != 2 {
        return false;
    }
    let (Ok(year), Ok(month)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>()) else {
        return false;
    };
    // Partition upper-bound is the first of the next month.
    let upper = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let Some(upper) = upper else { return false };
    let Some(upper_dt) = upper.and_hms_opt(0, 0, 0) else {
        return false;
    };
    let upper_utc: chrono::DateTime<Utc> = chrono::TimeZone::from_utc_datetime(&Utc, &upper_dt);
    upper_utc <= cutoff
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn extracts_partition_name_from_warm_key() {
        assert_eq!(
            partition_name_from_key("acme/supply/procurement/event_log_2026_03.parquet").as_deref(),
            Some("event_log_2026_03")
        );
    }

    #[test]
    fn rejects_keys_with_unexpected_shape() {
        assert!(partition_name_from_key("foo.parquet").is_none());
        assert!(partition_name_from_key("event_log_2026_03.parquet").is_some());
        assert!(partition_name_from_key("a/b/c/event_log_2026_3.parquet").is_none());
        assert!(partition_name_from_key("a/b/c/event_log_2026_03.txt").is_none());
    }

    #[test]
    fn partition_is_older_than_handles_boundary() {
        // event_log_2026_03 ends at 2026-04-01 00:00:00 UTC.
        let just_before =
            Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap() - chrono::Duration::seconds(1);
        let after = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
        assert!(!partition_is_older_than("event_log_2026_03", just_before));
        assert!(partition_is_older_than("event_log_2026_03", after));
    }

    #[test]
    fn partition_is_older_than_handles_december_wrap() {
        let dec_2025 = "event_log_2025_12";
        // Upper bound is 2026-01-01 00:00:00.
        let cutoff = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert!(partition_is_older_than(dec_2025, cutoff));
    }

    use chrono::TimeZone;
}
