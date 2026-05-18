//! Mapping from `(path, time-range)` to Parquet object keys.
//!
//! The operator's exporter (Phase 4.2) writes one Parquet object per
//! (org/app/domain, calendar-month) at:
//!
//! ```text
//!   <prefix>/<org>/<app>/<domain>/event_log_YYYY_MM.parquet
//! ```
//!
//! The leading `<prefix>` is the `storage_url`'s path component and is
//! baked into the `object_store` instance — we only deal in keys
//! *relative* to that prefix here.
//!
//! `path` is the canonical `schema_org` form (`org/app/domain`). It is
//! validated by the caller (the HTTP handler) before it reaches this
//! module, but defensive sanitization is cheap and guards against
//! caller bugs that could otherwise let a malformed `path` escape the
//! storage prefix.

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Utc};
use object_store::path::Path as ObjPath;

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("invalid schema path: expected `org/app/domain`, got `{0}`")]
    InvalidPath(String),
    #[error("invalid `until` timestamp")]
    InvalidUntil,
}

/// Validate a `schema_org` of the form `org/app/domain`. Each segment
/// must be `[a-z0-9][a-z0-9-]*` (the same surface the operator uses for
/// k8s namespace derivation and Postgres schema sanitization).
pub fn validate_path(path: &str) -> Result<(&str, &str, &str), LayoutError> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() != 3 {
        return Err(LayoutError::InvalidPath(path.to_string()));
    }
    for seg in &parts {
        if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err(LayoutError::InvalidPath(path.to_string()));
        }
    }
    Ok((parts[0], parts[1], parts[2]))
}

/// Object key for one (org/app/domain, month).
pub fn object_key_for_month(path: &str, year: i32, month: u32) -> Result<ObjPath, LayoutError> {
    let (org, app, domain) = validate_path(path)?;
    // ObjPath::from is safe because all components have been validated.
    let key = format!("{org}/{app}/{domain}/event_log_{year:04}_{month:02}.parquet");
    Ok(ObjPath::from(key))
}

/// All month-objects whose ranges could contain rows up to `until`,
/// going back `max_months` months. The reader stops as soon as it has
/// the entity's full history; we just need to enumerate candidates in
/// recency-first order.
///
/// `max_months` is a per-request fan-out cap so a runaway `until` (e.g.
/// 50 years in the past) can't ask us to consult thousands of objects.
/// The operator only writes monthly objects up to the warm-tier
/// retention horizon anyway — anything older is in cold tier and not
/// the warm reader's concern.
pub fn candidate_months(
    until: DateTime<Utc>,
    max_months: u32,
) -> Result<Vec<(i32, u32)>, LayoutError> {
    if max_months == 0 {
        return Ok(Vec::new());
    }
    let mut months = Vec::with_capacity(max_months as usize);
    let start = first_of_month(until.date_naive()).ok_or(LayoutError::InvalidUntil)?;
    let mut cursor = start;
    for _ in 0..max_months {
        months.push((cursor.year(), cursor.month()));
        let Some(prev) = cursor.checked_sub_months(Months::new(1)) else {
            break;
        };
        cursor = first_of_month(prev).ok_or(LayoutError::InvalidUntil)?;
    }
    Ok(months)
}

/// Lower bound for a given month — the first instant of `YYYY-MM-01 UTC`.
/// Used as a Parquet row-group predicate against `occurred_at`.
pub fn month_lower_bound(year: i32, month: u32) -> Option<DateTime<Utc>> {
    let d = NaiveDate::from_ymd_opt(year, month, 1)?;
    let dt = d.and_hms_opt(0, 0, 0)?;
    Utc.from_local_datetime(&dt).single()
}

fn first_of_month(d: NaiveDate) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn validates_well_formed_path() {
        assert!(validate_path("acme/supply/procurement").is_ok());
        assert!(validate_path("a/b/c").is_ok());
        assert!(validate_path("with-dashes/and_underscores/v1").is_ok());
    }

    #[test]
    fn rejects_bad_paths() {
        assert!(validate_path("acme/supply").is_err()); // too few
        assert!(validate_path("acme/supply/proc/extra").is_err()); // too many
        assert!(validate_path("acme//procurement").is_err()); // empty middle
        assert!(validate_path("Acme/supply/procurement").is_err()); // uppercase
        assert!(validate_path("../etc/passwd").is_err()); // escape attempt
        assert!(validate_path("acme/supply/procurement ").is_err()); // trailing space
    }

    #[test]
    fn object_key_layout_matches_exporter() {
        // This layout MUST stay in lockstep with the operator's
        // exporter. If you're changing it, change both sides and grep
        // the integration test.
        let k = object_key_for_month("acme/supply/procurement", 2026, 3).unwrap();
        assert_eq!(k.to_string(), "acme/supply/procurement/event_log_2026_03.parquet");
    }

    #[test]
    fn january_pads_to_two_digits() {
        let k = object_key_for_month("a/b/c", 2027, 1).unwrap();
        assert_eq!(k.to_string(), "a/b/c/event_log_2027_01.parquet");
    }

    #[test]
    fn candidate_months_walks_back_inclusive() {
        let until = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap();
        let months = candidate_months(until, 4).unwrap();
        assert_eq!(months, vec![(2026, 3), (2026, 2), (2026, 1), (2025, 12)]);
    }

    #[test]
    fn candidate_months_handles_zero_cap() {
        let until = Utc::now();
        assert!(candidate_months(until, 0).unwrap().is_empty());
    }
}
