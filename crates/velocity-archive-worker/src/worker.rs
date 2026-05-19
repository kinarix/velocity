//! Driver loop for the archive worker.
//!
//! Phase 8 slice 5 — single-replica, age-trigger only, postgres-cold
//! destination only. Cron-precise scheduling, multi-trigger support
//! (`field`/`tableSize`/`cel`), and S3 destinations land in subsequent
//! slices.
//!
//! ## Tick model
//!
//! On every `tick`, the worker:
//!
//! 1. Lists every `ArchivePolicy` (cluster-wide or namespace-scoped per
//!    config).
//! 2. Filters to policies that are `Ready`, have `destination.backend ==
//!    postgres-cold`, have an `age` trigger, and whose `lastRunAt` is
//!    older than `min_run_interval` (debounce so a tight tick cadence
//!    doesn't hammer a policy).
//! 3. For each kept policy, lists `SchemaDefinitions` in the policy's
//!    namespace and calls [`archive_batch`](crate::archive_batch) in a
//!    loop per schema until either:
//!    - `archive_batch` reports `more_pending == false` (caught up), or
//!    - the policy's `max_duration` (if any) is exceeded.
//! 4. Patches the policy's status with `lastRunAt` (now, RFC 3339) and
//!    `recordsArchived` (cumulative across the policy's lifetime).
//!
//! ## What's NOT here
//!
//! - Cron precision: we tick every `tick_interval` regardless of what
//!   `spec.schedule` says; `min_run_interval` is the only debounce.
//!   Slice 6 will add a real cron parser.
//! - Concurrent workers: assumes a single replica. `archive_batch`
//!   doesn't take SKIP LOCKED, and the status read-modify-write here is
//!   last-writer-wins.
//! - S3 destinations: branch is gated on `postgres-cold`; s3 work is
//!   slice 8+.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;
use serde_json::json;
use sqlx::PgPool;
use velocity_types::common::sanitize;
use velocity_types::crds::{ArchivePolicy, ReconcilePhase, SchemaDefinition};

use crate::s3_destination::{archive_batch_to_s3, S3ArchiveArgs};
use crate::{
    archive_batch_with_predicate, ordered_column_names, purge_batch, table_size_bytes,
    ArchiveBatchArgs, ArchiveError, ArchivePredicate, FieldOp, PurgeBatchArgs,
};
use object_store::ObjectStore;

const MANAGER: &str = "velocity-archive-worker";

/// Tunables for [`run`]. All have defensible defaults; the binary fills
/// them from env vars / flags.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// How often the worker wakes up to consider all policies.
    pub tick_interval: Duration,
    /// Minimum wall-clock time between two runs of the same policy.
    /// Debounces tight tick intervals so a 30 s tick doesn't archive
    /// the same policy 120 times an hour.
    pub min_run_interval: Duration,
    /// Default batch size when a policy doesn't override it.
    pub default_batch_size: usize,
    /// Default cap on a single per-policy tick when `spec.max_duration`
    /// is unset. Protects against a runaway policy eating the worker.
    pub default_max_duration: Duration,
    /// When set, restricts the watch to a single namespace; `None`
    /// means cluster-scoped.
    pub watch_namespace: Option<String>,
    /// Optional object-store sink for `destination.backend = "s3"`
    /// policies. When `None`, s3-destined policies are skipped (the
    /// worker logs a warning per tick rather than failing).
    #[allow(clippy::type_complexity)]
    pub s3_store: Option<Arc<dyn ObjectStore>>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(60),
            min_run_interval: Duration::from_secs(300),
            default_batch_size: 500,
            default_max_duration: Duration::from_secs(600),
            watch_namespace: None,
            s3_store: None,
        }
    }
}

/// One-pass summary returned from [`tick`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub policies_considered: usize,
    pub policies_run: usize,
    pub policies_skipped: usize,
    pub rows_archived: usize,
}

/// Run the worker forever. Wakes every `cfg.tick_interval` and processes
/// all eligible policies via [`tick`].
pub async fn run(pool: PgPool, kube: kube::Client, cfg: WorkerConfig) -> ! {
    let cfg = Arc::new(cfg);
    let mut interval = tokio::time::interval(cfg.tick_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        match tick(&pool, &kube, &cfg).await {
            Ok(report) => tracing::info!(
                policies_considered = report.policies_considered,
                policies_run = report.policies_run,
                policies_skipped = report.policies_skipped,
                rows_archived = report.rows_archived,
                "archive worker tick complete"
            ),
            Err(e) => tracing::warn!(error = %e, "archive worker tick failed"),
        }
    }
}

/// One pass over every policy in scope. Returned `TickReport` is the
/// caller-visible summary; individual per-policy failures are logged but
/// do not abort the tick.
pub async fn tick(
    pool: &PgPool,
    kube: &kube::Client,
    cfg: &WorkerConfig,
) -> Result<TickReport, kube::Error> {
    let policies = list_policies(kube, cfg.watch_namespace.as_deref()).await?;
    let mut report = TickReport { policies_considered: policies.len(), ..Default::default() };

    for policy in policies {
        if !is_eligible(&policy, cfg.min_run_interval) {
            report.policies_skipped += 1;
            continue;
        }
        let name = policy.name_any();
        let namespace = match policy.namespace() {
            Some(ns) => ns,
            None => {
                tracing::warn!(%name, "policy has no namespace; skipping");
                report.policies_skipped += 1;
                continue;
            }
        };
        match run_policy(pool, kube, cfg, &policy, &namespace).await {
            Ok(n) => {
                report.policies_run += 1;
                report.rows_archived += n;
            }
            Err(e) => {
                tracing::warn!(%name, %namespace, error = %e, "policy run failed");
            }
        }
    }
    Ok(report)
}

async fn list_policies(
    kube: &kube::Client,
    ns: Option<&str>,
) -> Result<Vec<ArchivePolicy>, kube::Error> {
    let api: Api<ArchivePolicy> = match ns {
        Some(ns) => Api::namespaced(kube.clone(), ns),
        None => Api::all(kube.clone()),
    };
    Ok(api.list(&ListParams::default()).await?.items)
}

/// A policy is eligible for this tick iff:
/// - status.phase == Ready
/// - destination.backend == "postgres-cold"
/// - trigger.kind == "age" (slice 5 only handles age)
/// - lastRunAt is unset OR (now - lastRunAt) >= min_run_interval
pub fn is_eligible(policy: &ArchivePolicy, min_run_interval: Duration) -> bool {
    let Some(status) = policy.status.as_ref() else {
        return false;
    };
    if status.phase != Some(ReconcilePhase::Ready) {
        return false;
    }
    if !matches!(policy.spec.destination.backend.as_str(), "postgres-cold" | "s3") {
        return false;
    }
    if !matches!(policy.spec.trigger.kind.as_str(), "age" | "field" | "tableSize") {
        return false;
    }
    match status.last_run_at.as_deref() {
        None => true,
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(t) => {
                let age = Utc::now().signed_duration_since(t.with_timezone(&Utc));
                age >= chrono::Duration::from_std(min_run_interval).unwrap_or_default()
            }
            Err(_) => true, // malformed timestamp — treat as never run
        },
    }
}

/// Parse a `tableSize` trigger value. Accepts a bare integer (bytes) or
/// `NB|KiB|MiB|GiB|TiB`. Matches the operator-side validator's accepted
/// shapes.
pub fn parse_byte_size_value(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_u64() {
        return i64::try_from(n).ok();
    }
    let s = v.as_str()?.trim();
    let cut = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(cut);
    let n: i64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    let mult: i64 = match unit {
        "B" => 1,
        "KiB" => 1024,
        "MiB" => 1024i64.pow(2),
        "GiB" => 1024i64.pow(3),
        "TiB" => 1024i64.pow(4),
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Parse `Nu` durations (matches the operator-side validator).
pub fn parse_age_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let cut = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(cut);
    let n: u64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3_600)?,
        "d" => n.checked_mul(86_400)?,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// Bound parameter wrapper so a `Field` predicate can hold an owned
/// string that outlives the predicate construction.
struct FieldPredicate {
    field: String,
    op: FieldOp,
    value: String,
}

async fn run_policy(
    pool: &PgPool,
    kube: &kube::Client,
    cfg: &WorkerConfig,
    policy: &ArchivePolicy,
    namespace: &str,
) -> anyhow::Result<usize> {
    let (org, app, domain) = resolve_path(policy)?;

    let trigger_kind = policy.spec.trigger.kind.as_str();
    let (min_age, field_pred, table_size_threshold) = match trigger_kind {
        "age" => {
            let age_str = policy
                .spec
                .trigger
                .value
                .as_ref()
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("age trigger.value missing or non-string"))?;
            let d = parse_age_duration(age_str)
                .ok_or_else(|| anyhow::anyhow!("invalid age {age_str:?}"))?;
            (Some(d), None, None)
        }
        "field" => {
            let field = policy
                .spec
                .trigger
                .field
                .clone()
                .ok_or_else(|| anyhow::anyhow!("field trigger requires field"))?;
            let op_str = policy
                .spec
                .trigger
                .op
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("field trigger requires op"))?;
            let op = FieldOp::parse(op_str)
                .ok_or_else(|| anyhow::anyhow!("unknown field op {op_str:?}"))?;
            let value = policy
                .spec
                .trigger
                .value
                .as_ref()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .ok_or_else(|| anyhow::anyhow!("field trigger requires value"))?;
            (None, Some(FieldPredicate { field, op, value }), None)
        }
        "tableSize" => {
            let v = policy
                .spec
                .trigger
                .value
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("tableSize trigger requires value"))?;
            let bytes = parse_byte_size_value(v).ok_or_else(|| {
                anyhow::anyhow!("tableSize trigger value must be bytes integer or \"NGiB\" form")
            })?;
            (None, None, Some(bytes))
        }
        other => return Err(anyhow::anyhow!("unsupported trigger.kind {other:?}")),
    };

    let batch_size = policy
        .spec
        .batch_size
        .map(|n| n as usize)
        .unwrap_or(cfg.default_batch_size)
        .clamp(1, 10_000);

    let max_duration = policy
        .spec
        .max_duration
        .as_deref()
        .and_then(parse_age_duration)
        .unwrap_or(cfg.default_max_duration);

    let hot_schema = format!("{}_{}_{}", sanitize(&org), sanitize(&app), sanitize(&domain));
    let archive_schema = format!("{hot_schema}_archive");

    let sds: Vec<SchemaDefinition> =
        Api::namespaced(kube.clone(), namespace).list(&ListParams::default()).await?.items;

    let purge_after = policy.spec.purge_after.as_deref().and_then(parse_age_duration);

    let started = Instant::now();
    let mut total = 0usize;
    let mut total_purged = 0usize;

    'schemas: for sd in &sds {
        let table = pg_table_name(&sd.name_any(), &sd.spec.version);
        let columns = ordered_column_names(&sd.spec);

        if let Some(threshold) = table_size_threshold {
            match table_size_bytes(pool, &hot_schema, &table).await {
                Ok(size) if size < threshold => {
                    tracing::debug!(
                        policy = %policy.name_any(),
                        table = %table, size_bytes = size, threshold,
                        "tableSize under threshold; skipping table"
                    );
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        policy = %policy.name_any(),
                        table = %table, error = %e,
                        "table size lookup failed; skipping table"
                    );
                    continue;
                }
            }
        }

        let use_s3 = policy.spec.destination.backend == "s3";
        let s3_prefix = policy.spec.destination.bucket.clone().unwrap_or_default();

        loop {
            if started.elapsed() >= max_duration {
                tracing::info!(
                    policy = %policy.name_any(),
                    elapsed_secs = started.elapsed().as_secs(),
                    rows_so_far = total,
                    "policy hit max_duration; stopping"
                );
                break 'schemas;
            }

            if use_s3 {
                let Some(store) = cfg.s3_store.as_ref() else {
                    tracing::warn!(
                        policy = %policy.name_any(),
                        "destination=s3 but worker has no s3_store configured; skipping"
                    );
                    break;
                };
                let s3_args = S3ArchiveArgs {
                    hot_schema: &hot_schema,
                    hot_table: &table,
                    columns: &columns,
                    min_age: min_age.unwrap_or(std::time::Duration::from_secs(1)),
                    batch_size,
                    prefix: &s3_prefix,
                };
                match archive_batch_to_s3(pool, store.as_ref(), &s3_args).await {
                    Ok(r) => {
                        total += r.rows_archived;
                        if !r.more_pending {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            policy = %policy.name_any(),
                            table = %table, error = %e,
                            "archive_batch_to_s3 error; skipping table"
                        );
                        break;
                    }
                }
                continue;
            }

            let args = ArchiveBatchArgs {
                hot_schema: &hot_schema,
                hot_table: &table,
                archive_schema: &archive_schema,
                archive_table: &table,
                columns: &columns,
                min_age: min_age.unwrap_or(std::time::Duration::from_secs(1)),
                batch_size,
            };
            let predicate = if let Some(d) = min_age {
                ArchivePredicate::Age { min_age: d }
            } else if let Some(p) = &field_pred {
                ArchivePredicate::Field { field: &p.field, op: p.op, value: &p.value }
            } else {
                ArchivePredicate::Oldest
            };

            match archive_batch_with_predicate(pool, &args, &predicate).await {
                Ok(r) => {
                    total += r.rows_archived;
                    if !r.more_pending {
                        break;
                    }
                }
                Err(ArchiveError::Sql(e)) => {
                    // A single bad table shouldn't abort the policy run;
                    // log + move on to the next schema.
                    tracing::warn!(
                        policy = %policy.name_any(),
                        table = %table, error = %e,
                        "archive_batch sql error; skipping table"
                    );
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        policy = %policy.name_any(),
                        table = %table, error = %e,
                        "archive_batch error; skipping table"
                    );
                    break;
                }
            }
        }

        if let Some(min_age_since_archive) = purge_after {
            loop {
                if started.elapsed() >= max_duration {
                    break 'schemas;
                }
                let args = PurgeBatchArgs {
                    hot_schema: &hot_schema,
                    hot_table: &table,
                    min_age_since_archive,
                    batch_size,
                };
                match purge_batch(pool, &args).await {
                    Ok(r) => {
                        total_purged += r.rows_archived;
                        if !r.more_pending {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            policy = %policy.name_any(),
                            table = %table, error = %e,
                            "purge_batch error; skipping table"
                        );
                        break;
                    }
                }
            }
        }
    }

    patch_status(kube, policy, namespace, total, total_purged).await?;
    Ok(total + total_purged)
}

/// Resolve `(org, app, domain)` from the policy's `velocity.sh/*` labels.
pub fn resolve_path(policy: &ArchivePolicy) -> anyhow::Result<(String, String, String)> {
    let labels = policy.labels();
    let org = labels
        .get("velocity.sh/org")
        .ok_or_else(|| anyhow::anyhow!("missing label velocity.sh/org"))?
        .clone();
    let app = labels
        .get("velocity.sh/app")
        .ok_or_else(|| anyhow::anyhow!("missing label velocity.sh/app"))?
        .clone();
    let domain = labels
        .get("velocity.sh/domain")
        .ok_or_else(|| anyhow::anyhow!("missing label velocity.sh/domain"))?
        .clone();
    Ok((org, app, domain))
}

/// `{object}_{version}` with the same sanitisation as the operator.
pub fn pg_table_name(object: &str, version: &str) -> String {
    format!("{}_{}", sanitize(object), sanitize(version))
}

async fn patch_status(
    kube: &kube::Client,
    policy: &ArchivePolicy,
    namespace: &str,
    archived_now: usize,
    purged_now: usize,
) -> anyhow::Result<()> {
    let prior_archived: u64 = policy.status.as_ref().and_then(|s| s.records_archived).unwrap_or(0);
    let prior_purged: u64 = policy.status.as_ref().and_then(|s| s.records_purged).unwrap_or(0);
    let new_archived = prior_archived.saturating_add(archived_now as u64);
    let new_purged = prior_purged.saturating_add(purged_now as u64);

    let api: Api<ArchivePolicy> = Api::namespaced(kube.clone(), namespace);
    let patch = json!({
        "status": {
            "lastRunAt": Utc::now().to_rfc3339(),
            "recordsArchived": new_archived,
            "recordsPurged": new_purged,
        }
    });
    api.patch_status(&policy.name_any(), &PatchParams::apply(MANAGER), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::crds::policies::{
        ArchiveDestination, ArchivePolicySpec, ArchivePolicyStatus, ArchiveTrigger,
    };

    fn now_rfc3339_minus(secs: i64) -> String {
        (Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
    }

    fn ready_policy() -> ArchivePolicy {
        let mut p = ArchivePolicy::new(
            "po-90d",
            ArchivePolicySpec {
                schedule: "0 2 * * *".into(),
                trigger: ArchiveTrigger {
                    kind: "age".into(),
                    field: None,
                    op: None,
                    value: Some(json!("30d")),
                    rule: None,
                    max_execution_ms: None,
                },
                batch_size: Some(100),
                max_duration: None,
                destination: ArchiveDestination {
                    backend: "postgres-cold".into(),
                    bucket: None,
                    format: None,
                },
                purge_after: None,
            },
        );
        p.status = Some(ArchivePolicyStatus {
            phase: Some(ReconcilePhase::Ready),
            last_run_at: None,
            records_archived: None,
            records_purged: None,
            archive_schema: Some("acme_sc_proc_archive".into()),
            archive_roles: vec![],
            mirrored_tables: vec![],
            conditions: vec![],
        });
        p
    }

    #[test]
    fn eligible_when_ready_and_never_run() {
        let p = ready_policy();
        assert!(is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn not_eligible_when_not_ready() {
        let mut p = ready_policy();
        p.status.as_mut().unwrap().phase = Some(ReconcilePhase::Pending);
        assert!(!is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn not_eligible_when_no_status() {
        let mut p = ready_policy();
        p.status = None;
        assert!(!is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn eligible_for_s3_backend() {
        let mut p = ready_policy();
        p.spec.destination.backend = "s3".into();
        assert!(is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn not_eligible_for_unknown_backend() {
        let mut p = ready_policy();
        p.spec.destination.backend = "azure-blob".into();
        assert!(!is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn eligible_for_field_trigger() {
        let mut p = ready_policy();
        p.spec.trigger.kind = "field".into();
        assert!(is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn eligible_for_tablesize_trigger() {
        let mut p = ready_policy();
        p.spec.trigger.kind = "tableSize".into();
        assert!(is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn not_eligible_for_cel_trigger() {
        let mut p = ready_policy();
        p.spec.trigger.kind = "cel".into();
        assert!(!is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn parse_byte_size_accepts_bytes_and_units() {
        use serde_json::json;
        assert_eq!(parse_byte_size_value(&json!(1024)), Some(1024));
        assert_eq!(parse_byte_size_value(&json!("1024B")), Some(1024));
        assert_eq!(parse_byte_size_value(&json!("1KiB")), Some(1024));
        assert_eq!(parse_byte_size_value(&json!("10MiB")), Some(10 * 1024 * 1024));
        assert_eq!(parse_byte_size_value(&json!("2GiB")), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_byte_size_value(&json!("0KiB")), None);
        assert_eq!(parse_byte_size_value(&json!("10GB")), None);
        assert_eq!(parse_byte_size_value(&json!(true)), None);
    }

    #[test]
    fn debounces_recent_runs() {
        let mut p = ready_policy();
        p.status.as_mut().unwrap().last_run_at = Some(now_rfc3339_minus(60));
        assert!(!is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn eligible_after_debounce_window() {
        let mut p = ready_policy();
        p.status.as_mut().unwrap().last_run_at = Some(now_rfc3339_minus(600));
        assert!(is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn eligible_when_lastrun_unparseable() {
        let mut p = ready_policy();
        p.status.as_mut().unwrap().last_run_at = Some("not-a-date".into());
        assert!(is_eligible(&p, Duration::from_secs(300)));
    }

    #[test]
    fn parse_age_handles_suffixes() {
        assert_eq!(parse_age_duration("30d"), Some(Duration::from_secs(30 * 86_400)));
        assert_eq!(parse_age_duration("12h"), Some(Duration::from_secs(12 * 3_600)));
        assert_eq!(parse_age_duration("45m"), Some(Duration::from_secs(45 * 60)));
        assert_eq!(parse_age_duration("15s"), Some(Duration::from_secs(15)));
    }

    #[test]
    fn parse_age_rejects_bad_input() {
        assert!(parse_age_duration("").is_none());
        assert!(parse_age_duration("30").is_none());
        assert!(parse_age_duration("0d").is_none());
        assert!(parse_age_duration("12hours").is_none());
        assert!(parse_age_duration("abcd").is_none());
    }

    #[test]
    fn pg_table_name_sanitises() {
        assert_eq!(pg_table_name("purchase-order", "v1"), "purchase_order_v1");
        assert_eq!(pg_table_name("My.Object", "v2"), "my_object_v2");
    }

    #[test]
    fn resolve_path_reads_labels() {
        let mut p = ready_policy();
        let labels = p.metadata.labels.get_or_insert_with(Default::default);
        labels.insert("velocity.sh/org".into(), "acme".into());
        labels.insert("velocity.sh/app".into(), "supply-chain".into());
        labels.insert("velocity.sh/domain".into(), "procurement".into());
        let (o, a, d) = resolve_path(&p).unwrap();
        assert_eq!((o.as_str(), a.as_str(), d.as_str()), ("acme", "supply-chain", "procurement"));
    }

    #[test]
    fn resolve_path_errors_on_missing_label() {
        let p = ready_policy();
        assert!(resolve_path(&p).is_err());
    }
}
