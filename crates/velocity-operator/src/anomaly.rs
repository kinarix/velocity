//! Phase 6c — anomaly detection over `platform.audit_log`.
//!
//! The operator runs a periodic scanner that evaluates a small fixed
//! set of heuristics against rows arriving in the audit chain since
//! the last sweep. Detections land in `platform.anomaly_alerts`
//! (deduped by `(rule, actor, schema_org, hour-bucket)` via partial
//! unique index in migration 0006) and are also published to:
//!
//! - `tracing::warn!` — always, structured JSON for SIEM ingest
//! - Optional HTTP webhook (`VELOCITY_OPERATOR_ALERT_WEBHOOK_URL`) —
//!   POST `application/json` with the alert envelope. Stands in for
//!   the Kafka `velocity.alerts` topic referenced in phases.md L496
//!   until a follow-up ADR adds a real Kafka client.
//!
//! ## Why a composite (occurred_at, id) high-watermark
//!
//! `platform.audit_log` is append-only and the chain serialises on a
//! singleton row, so `occurred_at` is monotonic in practice but the row
//! `id` is a v4 random UUID (not monotonic on its own — `gen_random_uuid()`
//! is unordered). Tracking `(last_scanned_occurred_at, last_scanned_id)`
//! and filtering with the tuple comparison `(occurred_at, id) > ($1, $2)`
//! gives a strict total ordering that survives identical microsecond
//! timestamps. A restart resumes exactly where it left off — no
//! re-alerts, no skipped rows.
//!
//! ## Rules (v1)
//!
//! | rule              | triggers when                                                 |
//! |-------------------|---------------------------------------------------------------|
//! | `bulk_reader`     | one actor produced ≥ N successful read/query/search rows      |
//! |                   | in the sweep window                                           |
//! | `after_hours`     | a write (create/update/delete) landed outside 06:00–22:00 UTC |
//! | `repeated_denials`| one actor produced ≥ N denial rows in the sweep window        |
//!
//! Thresholds are constants — tunable by editing the source; v1 picks
//! conservative defaults so dashboards aren't spammed on a quiet
//! cluster. A future CRD-driven threshold policy is straightforward
//! to add once we know which numbers operators actually want.

use std::time::Duration;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};

/// How often the scanner ticks. Short enough that a sustained attack
/// is caught within minutes; long enough that the audit-log query
/// stays cheap on a busy cluster.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Cap on rows pulled per sweep. Bounds the cost of a single tick if
/// audit traffic spikes; remainder rolls into the next tick.
const SWEEP_BATCH: i64 = 10_000;

/// Threshold for the `bulk_reader` rule: ≥ this many successful read
/// rows from one actor in the sweep window fires an alert.
pub const BULK_READER_THRESHOLD: i64 = 100;

/// Threshold for the `repeated_denials` rule.
pub const REPEATED_DENIALS_THRESHOLD: i64 = 10;

/// Business-hours window (UTC, half-open). Writes outside `[06:00, 22:00)`
/// flag the `after_hours` rule. UTC because the platform is org-agnostic
/// — orgs in non-UTC offsets can wrap with their own LogFilterPolicy if
/// they need narrower windows.
const BUSINESS_HOURS_START_UTC: u32 = 6;
const BUSINESS_HOURS_END_UTC: u32 = 22;

/// Names used in `anomaly_alerts.rule`. Constants so Grafana queries
/// and the dedupe unique index stay in lock-step with the code.
pub mod rule {
    pub const BULK_READER: &str = "bulk_reader";
    pub const AFTER_HOURS: &str = "after_hours";
    pub const REPEATED_DENIALS: &str = "repeated_denials";
}

/// Alert envelope. Identical shape across the DB row and the webhook
/// POST body — one shape, one parser.
#[derive(Debug, Clone, Serialize)]
pub struct AnomalyAlert {
    pub rule: &'static str,
    pub actor: Option<String>,
    pub schema_org: Option<String>,
    pub severity: &'static str,
    pub detail: Value,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
}

/// Optional HTTP webhook config. When set, every freshly-inserted
/// alert is POSTed as JSON. Failures are logged but don't block — the
/// DB row is the source of truth.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub url: String,
    pub client: reqwest::Client,
}

/// Run the scanner loop forever.
pub async fn run(pool: PgPool, webhook: Option<WebhookConfig>) {
    tracing::info!(
        interval_secs = SWEEP_INTERVAL.as_secs(),
        batch = SWEEP_BATCH,
        webhook_configured = webhook.is_some(),
        "anomaly scanner started"
    );
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match sweep_once(&pool, webhook.as_ref()).await {
            Ok(n) if n > 0 => {
                tracing::info!(alerts_emitted = n, "anomaly sweep tick produced alerts")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "anomaly sweep tick failed; retrying next interval")
            }
        }
    }
}

/// One sweep tick. Returns the number of new alerts inserted (after
/// dedupe). Public so tests can drive it deterministically.
pub async fn sweep_once(
    pool: &PgPool,
    webhook: Option<&WebhookConfig>,
) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Snapshot the high-watermark + lock it FOR UPDATE so two
    // concurrent operators don't double-scan the same window.
    let cursor: (Option<DateTime<Utc>>, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT last_scanned_occurred_at, last_scanned_id \
         FROM platform.anomaly_scan_state WHERE id = 1 FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;

    // The window is "every audit row strictly after the last cursor".
    // Capped at SWEEP_BATCH so a backlog doesn't blow the tick budget.
    let rows = fetch_window(&mut tx, cursor.0, cursor.1).await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }

    let window_start = rows.iter().map(|r| r.occurred_at).min().unwrap_or_else(Utc::now);
    let window_end = rows.iter().map(|r| r.occurred_at).max().unwrap_or_else(Utc::now);
    // The last row in occurred_at-ordered output is also the largest tuple
    // — it has both the latest occurred_at and the largest id at that
    // timestamp (we ORDER BY both, ASC).
    let highest = rows.last().map(|r| (r.occurred_at, r.id));

    let alerts = evaluate_rules(&rows, window_start, window_end);

    let mut inserted: Vec<AnomalyAlert> = Vec::with_capacity(alerts.len());
    for alert in alerts {
        // The dedupe unique index drops same-rule/actor/hour collisions
        // at INSERT time — we use ON CONFLICT DO NOTHING and count
        // RETURNING-bearing rows as "actually inserted".
        let new: Option<uuid::Uuid> = sqlx::query_scalar(
            "INSERT INTO platform.anomaly_alerts \
                (rule, actor, schema_org, severity, detail, window_start, window_end) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT DO NOTHING \
             RETURNING id",
        )
        .bind(alert.rule)
        .bind(&alert.actor)
        .bind(&alert.schema_org)
        .bind(alert.severity)
        .bind(&alert.detail)
        .bind(alert.window_start)
        .bind(alert.window_end)
        .fetch_optional(&mut *tx)
        .await?;
        if new.is_some() {
            inserted.push(alert);
        }
    }

    // Advance the watermark to the highest (occurred_at, id) tuple we
    // saw so the next tick skips this window entirely.
    if let Some((ts, id)) = highest {
        sqlx::query(
            "UPDATE platform.anomaly_scan_state \
             SET last_scanned_occurred_at = $1, last_scanned_id = $2, \
                 last_scanned_at = now() WHERE id = 1",
        )
        .bind(ts)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Fire alerts AFTER commit — webhook delivery is at-least-once
    // (the row is committed; a delivery failure rolls into the
    // `delivered` flag scan a future enhancement can chase). Logging
    // is unconditional so SIEM ingest doesn't miss any.
    for alert in &inserted {
        tracing::warn!(
            rule = %alert.rule,
            actor = ?alert.actor,
            schema_org = ?alert.schema_org,
            severity = %alert.severity,
            window_start = %alert.window_start,
            window_end = %alert.window_end,
            detail = %alert.detail,
            "anomaly detected"
        );
        if let Some(w) = webhook {
            deliver_webhook(pool, w, alert).await;
        }
    }

    Ok(inserted.len())
}

/// Lightweight projection of `audit_log` rows for rule evaluation.
#[derive(Debug, sqlx::FromRow)]
struct AuditWindowRow {
    id: uuid::Uuid,
    occurred_at: DateTime<Utc>,
    actor: String,
    action: String,
    outcome: String,
    schema_org: Option<String>,
}

async fn fetch_window(
    tx: &mut Transaction<'_, Postgres>,
    last_scanned_occurred_at: Option<DateTime<Utc>>,
    last_scanned_id: Option<uuid::Uuid>,
) -> Result<Vec<AuditWindowRow>, sqlx::Error> {
    // Composite tuple comparison gives a strict total ordering even when
    // two rows share an occurred_at down to the microsecond.
    match (last_scanned_occurred_at, last_scanned_id) {
        (Some(ts), Some(id)) => {
            sqlx::query_as::<_, AuditWindowRow>(
                "SELECT id, occurred_at, actor, action, outcome, schema_org \
             FROM platform.audit_log \
             WHERE (occurred_at, id) > ($1, $2) \
             ORDER BY occurred_at, id \
             LIMIT $3",
            )
            .bind(ts)
            .bind(id)
            .bind(SWEEP_BATCH)
            .fetch_all(&mut **tx)
            .await
        }
        _ => {
            // First-ever sweep: process the most recent 5 minutes only,
            // so a freshly-deployed scanner doesn't backfill years of
            // history and spam every prior actor.
            sqlx::query_as::<_, AuditWindowRow>(
                "SELECT id, occurred_at, actor, action, outcome, schema_org \
                 FROM platform.audit_log \
                 WHERE occurred_at >= now() - interval '5 minutes' \
                 ORDER BY occurred_at, id \
                 LIMIT $1",
            )
            .bind(SWEEP_BATCH)
            .fetch_all(&mut **tx)
            .await
        }
    }
}

/// Evaluate every rule against the rows. Pure — returns alerts to
/// insert. Pulled out so unit tests can drive each rule on a hand-built
/// row set without any DB.
pub fn evaluate_rules(
    rows: &[impl AsAuditRow],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<AnomalyAlert> {
    let mut out = Vec::new();
    out.extend(detect_bulk_readers(rows, window_start, window_end));
    out.extend(detect_after_hours(rows, window_start, window_end));
    out.extend(detect_repeated_denials(rows, window_start, window_end));
    out
}

/// Public trait so tests can pass plain structs without depending on
/// the internal `AuditWindowRow` type. `evaluate_rules` operates on
/// anything that exposes the fields each rule needs.
pub trait AsAuditRow {
    fn actor(&self) -> &str;
    fn action(&self) -> &str;
    fn outcome(&self) -> &str;
    fn occurred_at(&self) -> DateTime<Utc>;
    fn schema_org(&self) -> Option<&str>;
}

impl AsAuditRow for AuditWindowRow {
    fn actor(&self) -> &str {
        &self.actor
    }
    fn action(&self) -> &str {
        &self.action
    }
    fn outcome(&self) -> &str {
        &self.outcome
    }
    fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }
    fn schema_org(&self) -> Option<&str> {
        self.schema_org.as_deref()
    }
}

fn detect_bulk_readers<R: AsAuditRow>(
    rows: &[R],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<AnomalyAlert> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, i64> = HashMap::new();
    for r in rows {
        // Successful reads only — denials are the repeated_denials
        // rule's concern; failures aren't "bulk read" by an actor.
        if matches!(r.action(), "read" | "query" | "search") && r.outcome() == "success" {
            *counts.entry(r.actor().to_string()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n >= BULK_READER_THRESHOLD)
        .map(|(actor, n)| AnomalyAlert {
            rule: rule::BULK_READER,
            actor: Some(actor),
            schema_org: None,
            severity: "warning",
            detail: json!({
                "reads": n,
                "threshold": BULK_READER_THRESHOLD,
            }),
            window_start,
            window_end,
        })
        .collect()
}

fn detect_after_hours<R: AsAuditRow>(
    rows: &[R],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<AnomalyAlert> {
    use std::collections::HashSet;
    // One alert per (actor, schema_org) — dedupe handled by the unique
    // index, so duplicate-emitting here just rolls into the same row.
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        if !is_write(r.action()) || r.outcome() != "success" {
            continue;
        }
        // Skip weekend writes too — same business-hours intent. We
        // collapse "weekend" into the after-hours signal rather than
        // adding a 4th rule that would mostly duplicate this one.
        let h = r.occurred_at().hour();
        let weekday = r.occurred_at().weekday();
        let is_business_day = !matches!(weekday, chrono::Weekday::Sat | chrono::Weekday::Sun);
        let in_hours = (BUSINESS_HOURS_START_UTC..BUSINESS_HOURS_END_UTC).contains(&h);
        if is_business_day && in_hours {
            continue;
        }
        let key = (r.actor().to_string(), r.schema_org().map(str::to_string));
        if !seen.insert(key.clone()) {
            continue;
        }
        out.push(AnomalyAlert {
            rule: rule::AFTER_HOURS,
            actor: Some(key.0),
            schema_org: key.1,
            severity: "info",
            detail: json!({
                "action": r.action(),
                "hour_utc": h,
                "weekday": format!("{:?}", weekday),
                "business_hours_utc": [BUSINESS_HOURS_START_UTC, BUSINESS_HOURS_END_UTC],
            }),
            window_start,
            window_end,
        });
    }
    out
}

fn is_write(action: &str) -> bool {
    matches!(action, "create" | "update" | "delete" | "restore")
}

fn detect_repeated_denials<R: AsAuditRow>(
    rows: &[R],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<AnomalyAlert> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, i64> = HashMap::new();
    for r in rows {
        if r.outcome() == "denied" {
            *counts.entry(r.actor().to_string()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n >= REPEATED_DENIALS_THRESHOLD)
        .map(|(actor, n)| AnomalyAlert {
            rule: rule::REPEATED_DENIALS,
            actor: Some(actor),
            schema_org: None,
            severity: "critical",
            detail: json!({
                "denials": n,
                "threshold": REPEATED_DENIALS_THRESHOLD,
            }),
            window_start,
            window_end,
        })
        .collect()
}

/// POST the alert JSON to the configured webhook. Failures flip the
/// row's `delivered` flag to false (already the default), so a future
/// retry loop can chase undelivered alerts; we don't block on retry
/// inline to keep the sweep tick fast.
async fn deliver_webhook(pool: &PgPool, w: &WebhookConfig, alert: &AnomalyAlert) {
    let res = w.client.post(&w.url).json(alert).timeout(Duration::from_secs(5)).send().await;
    match res {
        Ok(r) if r.status().is_success() => {
            let _ = sqlx::query(
                "UPDATE platform.anomaly_alerts \
                 SET delivered = true, delivered_at = now() \
                 WHERE rule = $1 AND COALESCE(actor, '') = COALESCE($2, '') \
                       AND COALESCE(schema_org, '') = COALESCE($3, '') \
                       AND delivered = false \
                       AND date_trunc('hour', (detected_at AT TIME ZONE 'UTC')) \
                           = date_trunc('hour', (now() AT TIME ZONE 'UTC'))",
            )
            .bind(alert.rule)
            .bind(&alert.actor)
            .bind(&alert.schema_org)
            .execute(pool)
            .await;
        }
        Ok(r) => tracing::warn!(
            url = %w.url,
            status = %r.status(),
            rule = %alert.rule,
            "anomaly webhook returned non-2xx; row left undelivered for retry"
        ),
        Err(e) => tracing::warn!(
            url = %w.url,
            error = %e,
            rule = %alert.rule,
            "anomaly webhook POST failed; row left undelivered for retry"
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Pure test struct — implements `AsAuditRow` so we can drive the
    /// rule logic without sqlx.
    struct Row {
        actor: &'static str,
        action: &'static str,
        outcome: &'static str,
        when: DateTime<Utc>,
        schema_org: Option<&'static str>,
    }

    impl AsAuditRow for Row {
        fn actor(&self) -> &str {
            self.actor
        }
        fn action(&self) -> &str {
            self.action
        }
        fn outcome(&self) -> &str {
            self.outcome
        }
        fn occurred_at(&self) -> DateTime<Utc> {
            self.when
        }
        fn schema_org(&self) -> Option<&str> {
            self.schema_org
        }
    }

    fn ts_at(hour: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 19, hour, 0, 0).single().unwrap()
    }

    #[test]
    fn bulk_reader_fires_only_above_threshold() {
        let when = ts_at(10);
        // alice: 100 reads (>= threshold) → alert.
        // bob:   99 reads (just below) → no alert.
        let mut rows: Vec<Row> = Vec::new();
        for _ in 0..BULK_READER_THRESHOLD {
            rows.push(Row {
                actor: "alice",
                action: "read",
                outcome: "success",
                when,
                schema_org: None,
            });
        }
        for _ in 0..(BULK_READER_THRESHOLD - 1) {
            rows.push(Row {
                actor: "bob",
                action: "read",
                outcome: "success",
                when,
                schema_org: None,
            });
        }
        let alerts = detect_bulk_readers(&rows, when, when);
        assert_eq!(alerts.len(), 1, "exactly one bulk-reader alert (alice)");
        assert_eq!(alerts[0].actor.as_deref(), Some("alice"));
        assert_eq!(alerts[0].rule, rule::BULK_READER);
        assert_eq!(alerts[0].detail["reads"], BULK_READER_THRESHOLD);
    }

    #[test]
    fn bulk_reader_ignores_denials_and_writes() {
        let when = ts_at(10);
        // 200 denied reads + 200 successful writes — neither should
        // trip the bulk-reader rule (those are other rules' jobs).
        let mut rows: Vec<Row> = Vec::new();
        for _ in 0..200 {
            rows.push(Row {
                actor: "alice",
                action: "read",
                outcome: "denied",
                when,
                schema_org: None,
            });
        }
        for _ in 0..200 {
            rows.push(Row {
                actor: "alice",
                action: "create",
                outcome: "success",
                when,
                schema_org: None,
            });
        }
        let alerts = detect_bulk_readers(&rows, when, when);
        assert!(alerts.is_empty(), "denials + writes must not trip bulk_reader: {alerts:?}");
    }

    #[test]
    fn after_hours_fires_outside_business_window() {
        let early = ts_at(3); // 03:00 UTC — before business hours
        let mid_day = ts_at(14); // 14:00 UTC — squarely inside
        let late = ts_at(23); // 23:00 UTC — after business hours
        let rows = vec![
            Row {
                actor: "ops",
                action: "create",
                outcome: "success",
                when: early,
                schema_org: Some("o/a/d/x/v1"),
            },
            Row {
                actor: "ops",
                action: "update",
                outcome: "success",
                when: mid_day,
                schema_org: Some("o/a/d/x/v1"),
            },
            Row {
                actor: "ops",
                action: "delete",
                outcome: "success",
                when: late,
                schema_org: Some("o/a/d/x/v1"),
            },
        ];
        let alerts = detect_after_hours(&rows, early, late);
        // Two after-hours writes (early + late). Dedupe-within-tick
        // collapses to one alert per (actor, schema_org). Pinned so a
        // refactor that drops the in-fn dedupe doesn't spam.
        assert_eq!(
            alerts.len(),
            1,
            "in-tick dedupe collapses early+late into one alert: {alerts:?}"
        );
        assert_eq!(alerts[0].rule, rule::AFTER_HOURS);
    }

    #[test]
    fn after_hours_skips_reads() {
        // After-hours should fire only on writes — a SOC analyst
        // reading at 02:00 is normal during an incident response and
        // shouldn't be flagged twice (bulk_reader already covers it).
        let late = ts_at(2);
        let rows = vec![Row {
            actor: "soc",
            action: "read",
            outcome: "success",
            when: late,
            schema_org: None,
        }];
        let alerts = detect_after_hours(&rows, late, late);
        assert!(alerts.is_empty(), "reads outside hours must not trip after_hours: {alerts:?}");
    }

    #[test]
    fn repeated_denials_fires_only_above_threshold() {
        let when = ts_at(10);
        let mut rows: Vec<Row> = Vec::new();
        for _ in 0..REPEATED_DENIALS_THRESHOLD {
            rows.push(Row {
                actor: "evil",
                action: "create",
                outcome: "denied",
                when,
                schema_org: None,
            });
        }
        let alerts = detect_repeated_denials(&rows, when, when);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].rule, rule::REPEATED_DENIALS);
        assert_eq!(alerts[0].severity, "critical");
    }

    #[test]
    fn empty_window_produces_no_alerts() {
        let now = Utc::now();
        let empty: Vec<Row> = Vec::new();
        let alerts = evaluate_rules(&empty, now, now);
        assert!(alerts.is_empty());
    }
}
