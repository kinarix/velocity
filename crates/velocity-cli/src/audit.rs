//! `velocity audit ...` subcommands.
//!
//! Currently only `verify` is implemented — additional audit operations
//! (export, sign, etc.) will land here as Phase 6+ delivers them.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Subcommand;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;

use crate::output::{self, OutputFormat};

#[derive(Debug, Subcommand)]
pub(crate) enum AuditCmd {
    /// Walk the audit chain over a time window, recompute every row's
    /// hash, and report rows whose stored hash differs from the
    /// recomputed one. Exits non-zero on any mismatch.
    Verify {
        /// Window start (inclusive). ISO-8601 / RFC-3339. Mutually
        /// exclusive with `--window`.
        #[arg(long)]
        from: Option<DateTime<Utc>>,
        /// Window end (exclusive). ISO-8601 / RFC-3339. Mutually
        /// exclusive with `--window`. Defaults to `now()`.
        #[arg(long)]
        to: Option<DateTime<Utc>>,
        /// Convenience: verify the last duration (e.g. `24h`, `7d`).
        /// Equivalent to `--from=now-window --to=now`.
        #[arg(long, value_parser = parse_duration)]
        window: Option<Duration>,
    },
}

pub(crate) async fn run(cmd: AuditCmd, db_url: Option<&str>, output: OutputFormat) -> Result<()> {
    match cmd {
        AuditCmd::Verify { from, to, window } => verify(db_url, from, to, window, output).await,
    }
}

async fn verify(
    db_url: Option<&str>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    window: Option<Duration>,
    output: OutputFormat,
) -> Result<()> {
    let db_url = db_url
        .ok_or_else(|| anyhow!("--db-url or VELOCITY_PG_URL is required for audit verify"))?;

    let (from, to) = resolve_window(from, to, window)?;
    if to <= from {
        return Err(anyhow!("--to must be strictly after --from"));
    }

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(db_url)
        .await
        .context("connecting to Postgres")?;

    // Stored procedure does all the work; we just project rows where
    // stored != computed. Anything returned by this query is a tamper
    // or a serializer drift — both warrant a human look.
    let rows = sqlx::query(
        "SELECT id, occurred_at, stored_hash, computed_hash \
         FROM platform.audit_verify_window($1, $2) \
         WHERE stored_hash IS DISTINCT FROM computed_hash \
         ORDER BY occurred_at",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&pool)
    .await
    .context("calling platform.audit_verify_window")?;

    let total_in_window: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.audit_log WHERE occurred_at >= $1 AND occurred_at < $2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(&pool)
    .await
    .context("counting rows in window")?;

    let tampered = rows.len();
    let formatted_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let id: uuid::Uuid = r.get("id");
            let occurred_at: DateTime<Utc> = r.get("occurred_at");
            let stored: Option<String> = r.get("stored_hash");
            let computed: Option<String> = r.get("computed_hash");
            vec![
                id.to_string(),
                occurred_at.to_rfc3339(),
                stored.unwrap_or_default(),
                computed.unwrap_or_default(),
            ]
        })
        .collect();

    eprintln!(
        "audit chain verify [{from} .. {to}] — {total_in_window} rows in window, {tampered} mismatched"
    );

    if tampered == 0 {
        // Still emit an empty table/JSON document so callers piping
        // output get a predictable shape.
        output::print(
            &["id", "occurred_at", "stored_hash", "computed_hash"],
            &[],
            output,
        );
        Ok(())
    } else {
        output::print(
            &["id", "occurred_at", "stored_hash", "computed_hash"],
            &formatted_rows,
            output,
        );
        // Distinct exit so CI can flag it. anyhow::Error → exit 1.
        Err(anyhow!("{tampered} audit-chain row(s) failed verification"))
    }
}

/// `(from, to)` from whichever of the three flags the caller supplied.
/// Defaults to `now() - 24h .. now()` when none are given so a bare
/// `velocity audit verify` does something useful.
fn resolve_window(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    window: Option<Duration>,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let now = Utc::now();
    match (from, to, window) {
        (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => Err(anyhow!(
            "--window is mutually exclusive with --from / --to"
        )),
        (Some(f), Some(t), None) => Ok((f, t)),
        (Some(f), None, None) => Ok((f, now)),
        (None, Some(t), None) => Ok((t - Duration::hours(24), t)),
        (None, None, Some(w)) => Ok((now - w, now)),
        (None, None, None) => Ok((now - Duration::hours(24), now)),
    }
}

/// Parse durations like `24h`, `7d`, `15m`, `30s`. Deliberately narrow —
/// `humantime` would handle a wider grammar but pulls in another dep
/// and these four units cover every realistic audit-verify window.
fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("duration `{s}` missing unit (expected one of s/m/h/d)"))?,
    );
    let n: i64 = num
        .parse()
        .map_err(|_| format!("duration `{s}` has invalid number"))?;
    if n <= 0 {
        return Err(format!("duration `{s}` must be positive"));
    }
    match unit {
        "s" => Ok(Duration::seconds(n)),
        "m" => Ok(Duration::minutes(n)),
        "h" => Ok(Duration::hours(n)),
        "d" => Ok(Duration::days(n)),
        other => Err(format!("duration `{s}` has unknown unit `{other}` (expected s/m/h/d)")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_duration("15m").unwrap(), Duration::minutes(15));
        assert_eq!(parse_duration("24h").unwrap(), Duration::hours(24));
        assert_eq!(parse_duration("7d").unwrap(), Duration::days(7));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("-5h").is_err());
        assert!(parse_duration("0d").is_err());
        assert!(parse_duration("10y").is_err());
        assert!(parse_duration("10").is_err()); // missing unit
    }

    #[test]
    fn resolve_window_defaults_to_24h() {
        let (f, t) = resolve_window(None, None, None).unwrap();
        assert!(t > f);
        // Window should be ~24h; allow slop for the wall-clock reads.
        let diff = (t - f).num_seconds();
        assert!((86_390..=86_410).contains(&diff), "got {diff}");
    }

    #[test]
    fn resolve_window_with_explicit_bounds() {
        let f = Utc::now() - Duration::hours(2);
        let t = Utc::now();
        let (rf, rt) = resolve_window(Some(f), Some(t), None).unwrap();
        assert_eq!(rf, f);
        assert_eq!(rt, t);
    }

    #[test]
    fn resolve_window_rejects_window_plus_from() {
        let err = resolve_window(Some(Utc::now()), None, Some(Duration::hours(1)));
        assert!(err.is_err());
    }
}
