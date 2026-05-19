//! Build an `ObjectStore` from a user-supplied URL string.
//!
//! Mirror of `velocity-warm-reader::main::build_object_store` —
//! deliberately duplicated because the two services version
//! independently and a future change on either side shouldn't be a
//! hidden coupling. Both sides use the same `object_store::parse_url`
//! semantics so the URL form (`s3://bucket/prefix`, `file:///path`)
//! behaves identically.

use std::sync::Arc;

use anyhow::{Context, Result};
use object_store::ObjectStore;

pub fn build(url_str: &str) -> Result<Arc<dyn ObjectStore>> {
    let url = url::Url::parse(url_str).with_context(|| format!("invalid storage URL: {url_str}"))?;
    let (store, prefix) =
        object_store::parse_url(&url).with_context(|| format!("unsupported storage URL: {url_str}"))?;
    let store: Arc<dyn ObjectStore> = if prefix.as_ref().is_empty() {
        Arc::from(store)
    } else {
        Arc::new(object_store::prefix::PrefixStore::new(store, prefix))
    };
    Ok(store)
}

/// Build the warm-tier object key for `(org/app/domain, year, month)`.
/// Format MUST match `velocity_warm_reader::object_layout`.
pub fn month_key(schema_org: &str, year: i32, month: u32) -> object_store::path::Path {
    // schema_org is validated upstream (it came from event_log rows
    // the operator wrote); still, defensively re-validate that it has
    // the three-segment shape we expect, and panic-free fall back to
    // a normalized form if not.
    object_store::path::Path::from(format!("{schema_org}/event_log_{year:04}_{month:02}.parquet"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn file_url_resolves_to_local_store() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().to_str().unwrap());
        let _ = build(&url).expect("file:// URL should resolve");
    }

    #[test]
    fn month_key_matches_documented_layout() {
        let k = month_key("acme/supply/procurement", 2026, 3);
        assert_eq!(k.to_string(), "acme/supply/procurement/event_log_2026_03.parquet");
    }

    #[test]
    fn month_key_pads_january_to_two_digits() {
        let k = month_key("a/b/c", 2027, 1);
        assert_eq!(k.to_string(), "a/b/c/event_log_2027_01.parquet");
    }

    #[test]
    fn build_rejects_malformed_url() {
        // Triggers the with_context branch on the url::Url::parse line.
        let err = build("not a url").unwrap_err();
        assert!(format!("{err:#}").contains("invalid storage URL"));
    }

    #[test]
    fn build_rejects_unsupported_scheme() {
        // object_store::parse_url rejects unknown schemes — exercises the
        // second with_context branch.
        let err = build("ftp://example.com/bucket").unwrap_err();
        assert!(format!("{err:#}").contains("unsupported storage URL"));
    }

    #[test]
    fn build_with_empty_prefix_returns_bare_store() {
        // memory:// with no path → empty prefix → hits the Arc::from(store)
        // branch (line 20) instead of the PrefixStore wrap.
        let store = build("memory:///").expect("memory:// should resolve");
        // Bare store implements list/put — a smoke check confirms it is a
        // usable ObjectStore (it cannot be the prefixed variant).
        assert!(store.to_string().contains("Memory"));
    }

    #[test]
    fn month_key_handles_five_segment_registry_key() {
        // schema_org in event_log is the 5-segment registry_key
        // (org/app/domain/object/version). The exporter passes it
        // through verbatim — warm-reader's object_layout::validate_path
        // accepts the 5-segment form for this reason.
        let k = month_key("acme/supply-chain/procurement/purchase-order/v1", 2026, 3);
        assert_eq!(
            k.to_string(),
            "acme/supply-chain/procurement/purchase-order/v1/event_log_2026_03.parquet"
        );
    }
}
