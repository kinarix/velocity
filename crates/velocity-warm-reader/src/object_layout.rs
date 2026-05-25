//! Mapping from `(path, time-range)` to Parquet object keys.
//!
//! The operator's exporter (Phase 4.2) writes one Parquet object per
//! (`schema_org`, calendar-month) at:
//!
//! ```text
//!   <prefix>/<schema_org>/event_log_YYYY_MM.parquet
//! ```
//!
//! where `<schema_org>` is the canonical
//! `org/app/domain/object/version` path the API writes into
//! `platform.event_log.schema_org`. The leading `<prefix>` is the
//! `storage_url`'s path component and is baked into the `object_store`
//! instance — we only deal in keys *relative* to that prefix here.
//!
//! `path` is validated by the caller (the HTTP handler) before it
//! reaches this module, but defensive sanitization is cheap and guards
//! against caller bugs that could otherwise let a malformed `path`
//! escape the storage prefix.

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Utc};
use object_store::path::Path as ObjPath;

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("invalid schema path `{0}` (expected segments matching [a-z0-9_-]+)")]
    InvalidPath(String),
    #[error("invalid `until` timestamp")]
    InvalidUntil,
}

/// Minimum + maximum segments accepted in a `schema_org` path. The
/// API writes 5-segment values (`org/app/domain/object/version`) via
/// `velocity_core::registry::registry_key`. We accept 3 too because
/// historical fixtures + a few operator tests use the 3-segment form
/// (`org/app/domain`) and there's no harm in allowing it — the on-disk
/// key uses the raw path either way.
const MIN_SEGMENTS: usize = 3;
const MAX_SEGMENTS: usize = 5;

/// Validate a `schema_org` of the form `org/app/domain[/object/version]`.
/// Each segment must match `[a-z0-9_-]+`. This is the same surface the
/// operator uses for k8s namespace derivation and Postgres schema
/// sanitization, and matches what `velocity_core::registry::registry_key`
/// emits.
pub fn validate_path(path: &str) -> Result<(), LayoutError> {
    let parts: Vec<&str> = path.split('/').collect();
    if !(MIN_SEGMENTS..=MAX_SEGMENTS).contains(&parts.len()) {
        return Err(LayoutError::InvalidPath(path.to_string()));
    }
    for seg in &parts {
        if seg.is_empty()
            || !seg
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(LayoutError::InvalidPath(path.to_string()));
        }
    }
    Ok(())
}

/// Object key for one (`schema_org`, month). Layout MUST match the
/// operator's exporter (`velocity_operator::tiering::object_store_url::month_key`).
pub fn object_key_for_month(path: &str, year: i32, month: u32) -> Result<ObjPath, LayoutError> {
    validate_path(path)?;
    // ObjPath::from is safe because the path has been validated.
    let key = format!("{path}/event_log_{year:04}_{month:02}.parquet");
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
        // 3-segment legacy form.
        assert!(validate_path("acme/supply/procurement").is_ok());
        assert!(validate_path("a/b/c").is_ok());
        // 5-segment canonical form used by velocity-api's registry_key.
        assert!(validate_path("acme/supply-chain/procurement/purchase-order/v1").is_ok());
        assert!(validate_path("with-dashes/and_underscores/v1/obj/v2").is_ok());
    }

    #[test]
    fn rejects_bad_paths() {
        assert!(validate_path("acme/supply").is_err()); // too few
        assert!(validate_path("a/b/c/d/e/f").is_err()); // too many
        assert!(validate_path("acme//procurement").is_err()); // empty middle
        assert!(validate_path("Acme/supply/procurement").is_err()); // uppercase
        assert!(validate_path("../etc/passwd").is_err()); // escape attempt
        assert!(validate_path("acme/supply/procurement ").is_err()); // trailing space
    }

    #[test]
    fn object_key_layout_matches_exporter_three_segment() {
        // This layout MUST stay in lockstep with the operator's
        // exporter. If you're changing it, change both sides and grep
        // the integration test.
        let k = object_key_for_month("acme/supply/procurement", 2026, 3).unwrap();
        assert_eq!(k.to_string(), "acme/supply/procurement/event_log_2026_03.parquet");
    }

    #[test]
    fn object_key_layout_matches_exporter_five_segment() {
        // Canonical 5-segment form: registry_key value the API actually
        // writes into event_log.schema_org.
        let k = object_key_for_month("acme/supply-chain/procurement/purchase-order/v1", 2026, 3)
            .unwrap();
        assert_eq!(
            k.to_string(),
            "acme/supply-chain/procurement/purchase-order/v1/event_log_2026_03.parquet"
        );
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

    #[test]
    fn month_lower_bound_returns_first_instant_of_month() {
        let lb = month_lower_bound(2026, 3).unwrap();
        assert_eq!(lb, Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn month_lower_bound_rejects_invalid_month() {
        // month=13 is out of range — NaiveDate::from_ymd_opt returns None.
        assert!(month_lower_bound(2026, 13).is_none());
        assert!(month_lower_bound(2026, 0).is_none());
    }

    #[test]
    fn candidate_months_breaks_on_year_underflow() {
        // chrono's NaiveDate min year is i32::MIN. `checked_sub_months`
        // returns None when subtracting would overflow. We exercise this
        // by starting near the minimum representable date and asking for
        // more months than exist before underflow.
        let early = Utc.with_ymd_and_hms(-262143, 2, 15, 0, 0, 0).unwrap();
        let months = candidate_months(early, 12).unwrap();
        // We get at least one (the start month) but fewer than max
        // because the loop breaks when subtraction underflows.
        assert!(!months.is_empty());
        assert!(months.len() < 12, "expected break before reaching cap, got {months:?}");
    }
}
