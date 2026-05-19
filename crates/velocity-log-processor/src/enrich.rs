//! Pod-metadata enrichment.
//!
//! The collector ships records with a `kubernetes` envelope copied
//! from the pod's metadata file:
//!
//! ```json
//! { "kubernetes": { "namespace": "acme-sc-procurement",
//!                   "pod": "...", "labels": { "velocity.sh/org": "acme", ... } },
//!   "log": "..." }
//! ```
//!
//! We hoist the four Velocity-relevant labels into top-level fields so
//! filters and destinations can match on them without re-walking the
//! envelope. Records that aren't from a Velocity-managed pod (no
//! `velocity.sh/org` label) pass through unmodified.

use serde_json::Value;

const LABEL_PATH: &[&str] = &["kubernetes", "labels"];

/// In-place enrichment. Idempotent: re-running on an already-enriched
/// record is a no-op (top-level fields already set; we don't overwrite).
pub fn enrich(record: &mut Value) {
    let Some(labels) = walk(record, LABEL_PATH) else { return };
    let Value::Object(labels) = labels else { return };

    let pairs = [
        ("velocity.sh/org", "velocity_org"),
        ("velocity.sh/app", "velocity_app"),
        ("velocity.sh/domain", "velocity_domain"),
        ("velocity.sh/version", "velocity_version"),
    ];

    let mut to_set: Vec<(&str, Value)> = Vec::with_capacity(pairs.len());
    for (label, top_key) in pairs {
        if let Some(v) = labels.get(label) {
            to_set.push((top_key, v.clone()));
        }
    }
    if to_set.is_empty() {
        return;
    }

    let Value::Object(root) = record else { return };
    for (k, v) in to_set {
        // Don't clobber an existing top-level field — caller may have
        // set it intentionally.
        root.entry(k.to_string()).or_insert(v);
    }
}

fn walk<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path {
        cur = cur.as_object()?.get(*seg)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn hoists_velocity_labels_to_top_level() {
        let mut r = json!({
            "kubernetes": {
                "namespace": "acme-sc-procurement",
                "labels": {
                    "velocity.sh/org": "acme",
                    "velocity.sh/app": "supply-chain",
                    "velocity.sh/domain": "procurement",
                    "velocity.sh/version": "v1",
                    "other": "ignored"
                }
            },
            "log": "hi"
        });
        enrich(&mut r);
        assert_eq!(r["velocity_org"], json!("acme"));
        assert_eq!(r["velocity_app"], json!("supply-chain"));
        assert_eq!(r["velocity_domain"], json!("procurement"));
        assert_eq!(r["velocity_version"], json!("v1"));
    }

    #[test]
    fn no_labels_means_no_change() {
        let original = json!({"log": "hi"});
        let mut r = original.clone();
        enrich(&mut r);
        assert_eq!(r, original);
    }

    #[test]
    fn missing_velocity_labels_leaves_record_unenriched() {
        let original = json!({
            "kubernetes": {"labels": {"app": "other"}},
            "log": "hi"
        });
        let mut r = original.clone();
        enrich(&mut r);
        assert_eq!(r, original);
    }

    #[test]
    fn preserves_existing_top_level_velocity_fields() {
        let mut r = json!({
            "velocity_org": "explicitly-set",
            "kubernetes": {"labels": {"velocity.sh/org": "from-label"}}
        });
        enrich(&mut r);
        assert_eq!(r["velocity_org"], json!("explicitly-set"));
    }
}
