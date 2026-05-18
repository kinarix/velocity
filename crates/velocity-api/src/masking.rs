//! Layer 6 — per-field value masking from `FieldSpec.mask`.
//!
//! A field can declare a masking strategy plus a list of roles that are
//! *exempt* (`unmaskedFor`). Anyone whose roles don't intersect the exempt
//! list sees a transformed value instead of the raw one.
//!
//! ## Relationship to Layer 5
//!
//! Layer 5 (field filter) decides **whether** a field appears on the
//! response. Layer 6 decides **how** it appears. Strip MUST run before
//! mask — if a field is stripped, there's nothing to mask, and we
//! don't want to bring it back via a mask transform. The handler call
//! sites enforce this ordering.
//!
//! ## Strategies
//!
//! - `Redact` — replace with the opaque marker `"***"`. Works on any
//!   field type. The response shape changes (numeric → string), which
//!   is the point: the caller learns "you may not see this" rather
//!   than getting a plausible-looking value.
//! - `Partial { keep_last }` — keep the last *N* characters verbatim,
//!   replace the rest with `*`. String-shaped fields only.
//! - `Hash` — replace with `sha256:<hex>`. Stable across calls, so
//!   downstream systems can join on the masked value without seeing
//!   the secret.
//!
//! A future `Range` strategy will bucket numeric values into coarse
//! bands. It isn't declared on the CRD yet — adding it without runtime
//! support would let configs deserialize that the runtime can't honour,
//! so the variant lands when the implementation does.
//!
//! ## Where it gets applied
//!
//! All five read-shaped response sites:
//!   - LIST (each item)
//!   - GET
//!   - CREATE response body
//!   - UPDATE response body
//!   - Idempotency replay body (the cached body is pre-strip / pre-mask
//!     so a replay seen by a wider-role identity still benefits from
//!     their roles)
//!
//! Skipping a site would let a caller read the raw value by switching
//! to that verb — pinned by HTTP-level tests.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use velocity_types::crds::schema::{
    FieldKind, FieldSpec, MaskingStrategyKind, SchemaDefinitionSpec,
};

/// Resolved form of a [`MaskingStrategyKind`] — parameters baked in,
/// invariants checked at index-build time.
#[derive(Debug, Clone)]
pub enum CompiledMask {
    Redact,
    /// `keep_last` is clamped to a sane ceiling so a CRD that sets
    /// `keepLast: 1_000_000` doesn't blow up our string handling.
    Partial { keep_last: u32 },
    Hash,
    /// A strategy/field-kind combination that doesn't make sense
    /// (Partial on an integer, say). Treated as Redact at runtime to
    /// avoid leaking the value; the operator/webhook is expected to
    /// reject the CRD long before it lands in the registry.
    Broken { reason: &'static str },
}

/// Compiled per-field mask: strategy + exempt-role list + the field's
/// JSON kind (so the runtime can decide whether to coerce to string).
#[derive(Debug, Clone)]
pub struct FieldMask {
    pub mask: CompiledMask,
    pub unmasked_for: Vec<String>,
    pub field_kind: FieldKind,
}

/// Reasonable upper bound on `keep_last`. The CRD shouldn't ever ask for
/// more than this — if it does, we clamp here and trust the operator's
/// validator to surface the warning. Picked to be longer than any
/// plausible string identifier but short enough that pathological CRDs
/// can't allocate gigabytes of `*`.
const PARTIAL_KEEP_LAST_CEILING: u32 = 256;

/// Pre-computed `field_name -> FieldMask` map. Built once at resolve
/// time; the request hot path is a single `HashMap` lookup per field.
#[derive(Debug, Default)]
pub struct MaskingIndex {
    by_field: HashMap<String, FieldMask>,
}

impl MaskingIndex {
    pub fn from_spec(spec: &SchemaDefinitionSpec) -> Self {
        let mut by_field = HashMap::new();
        for f in &spec.fields {
            if let Some(m) = compile_field(f) {
                by_field.insert(f.name.clone(), m);
            }
        }
        Self { by_field }
    }

    pub fn is_empty(&self) -> bool {
        self.by_field.is_empty()
    }

    /// Apply masking in place. No-op when the index is empty, when
    /// `value` isn't a JSON object, or when the caller's roles
    /// intersect the field's `unmasked_for` list. Recurses into arrays
    /// of objects the same way `FieldFilterIndex::strip_for_read` does
    /// so a sensitive value in `LIST` rows can't leak through the array
    /// wrapper.
    pub fn apply_for_read(&self, value: &mut Value, roles: &[String]) {
        if self.is_empty() {
            return;
        }
        match value {
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    let Some(fm) = self.by_field.get(k) else { continue };
                    if is_exempt(&fm.unmasked_for, roles) {
                        continue;
                    }
                    apply_strategy(v, &fm.mask);
                }
                // Recurse only into nested arrays — nested objects don't
                // surface user-declared fields, mirroring Layer-5's
                // policy on deep walks.
                for v in map.values_mut() {
                    if v.is_array() {
                        self.apply_for_read(v, roles);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.apply_for_read(item, roles);
                }
            }
            _ => {}
        }
    }

    /// For tests / startup logging — number of fields that have a mask
    /// (broken or otherwise) declared.
    pub fn len(&self) -> usize {
        self.by_field.len()
    }
}

fn is_exempt(unmasked_for: &[String], roles: &[String]) -> bool {
    if unmasked_for.is_empty() {
        return false;
    }
    roles.iter().any(|r| unmasked_for.contains(r))
}

fn compile_field(f: &FieldSpec) -> Option<FieldMask> {
    let spec = f.mask.as_ref()?;
    let mask = match spec.strategy {
        MaskingStrategyKind::Redact => CompiledMask::Redact,
        MaskingStrategyKind::Partial => {
            // Partial only makes sense on string-shaped fields. Forcing
            // it onto integer/json would produce a confusing typed mix.
            match f.kind {
                FieldKind::String | FieldKind::Enum | FieldKind::Ref | FieldKind::Uuid => {
                    let keep = spec.keep_last.unwrap_or(0).min(PARTIAL_KEEP_LAST_CEILING);
                    CompiledMask::Partial { keep_last: keep }
                }
                _ => CompiledMask::Broken { reason: "partial requires a string-shaped field" },
            }
        }
        MaskingStrategyKind::Hash => CompiledMask::Hash,
    };
    Some(FieldMask {
        mask,
        unmasked_for: spec.unmasked_for.clone(),
        field_kind: f.kind,
    })
}

fn apply_strategy(value: &mut Value, mask: &CompiledMask) {
    // Null/absent values stay as-is — there's nothing to hide.
    if value.is_null() {
        return;
    }
    let next = match mask {
        CompiledMask::Redact | CompiledMask::Broken { .. } => Value::String("***".into()),
        CompiledMask::Partial { keep_last } => {
            // We only compile Partial on string-shaped fields, but the
            // runtime may still see a non-string `Value` (e.g. on a
            // field whose stored value violated the schema). Coerce to
            // string before masking — leaking the structure would
            // defeat the point.
            let s = match &*value {
                Value::String(s) => s.clone(),
                Value::Null => return,
                other => other.to_string(),
            };
            Value::String(mask_partial(&s, *keep_last))
        }
        CompiledMask::Hash => {
            // Hash stringifies whatever's there so e.g. a numeric field
            // still produces a stable, comparable digest. Numbers and
            // strings hash differently (`42` vs `"42"`) — accept that
            // surprising-but-deterministic behaviour rather than papering
            // over it.
            let s = match &*value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Value::String(hash_value(&s))
        }
    };
    *value = next;
}

fn mask_partial(s: &str, keep_last: u32) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let keep = (keep_last as usize).min(n);
    let mask_count = n - keep;
    let mut out = String::with_capacity(n);
    for _ in 0..mask_count {
        out.push('*');
    }
    for c in &chars[mask_count..] {
        out.push(*c);
    }
    out
}

fn hash_value(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Convenience alias mirroring `SharedFieldFilter`.
pub type SharedMasking = Arc<MaskingIndex>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::common::NamespacedRef;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, MaskingSpec, ObservabilitySpec,
        SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    fn base_field(name: &str, kind: FieldKind) -> FieldSpec {
        let mut f: FieldSpec =
            serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
        f.kind = kind;
        f
    }

    fn masked_field(
        name: &str,
        kind: FieldKind,
        strategy: MaskingStrategyKind,
        keep_last: Option<u32>,
        unmasked_for: &[&str],
    ) -> FieldSpec {
        let mut f = base_field(name, kind);
        f.mask = Some(MaskingSpec {
            strategy,
            keep_last,
            unmasked_for: unmasked_for.iter().map(|s| (*s).to_string()).collect(),
        });
        f
    }

    fn spec(fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
        SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: AuthSpec {
                strategy_ref: NamespacedRef {
                    name: "default".into(),
                    namespace: "acme-platform".into(),
                },
                overrides: Vec::new(),
            },
            access: AccessSpec::default(),
            fields,
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        }
    }

    #[test]
    fn empty_index_is_no_op() {
        let s = spec(vec![base_field("po_number", FieldKind::String)]);
        let idx = MaskingIndex::from_spec(&s);
        assert!(idx.is_empty());
        let mut v = json!({ "po_number": "PO-1" });
        idx.apply_for_read(&mut v, &[]);
        assert_eq!(v["po_number"], "PO-1");
    }

    #[test]
    fn redact_replaces_with_marker() {
        let s = spec(vec![masked_field(
            "ssn",
            FieldKind::String,
            MaskingStrategyKind::Redact,
            None,
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "ssn": "123-45-6789" });
        idx.apply_for_read(&mut v, &[]);
        assert_eq!(v["ssn"], "***");
    }

    #[test]
    fn unmasked_for_role_bypasses_mask() {
        let s = spec(vec![masked_field(
            "ssn",
            FieldKind::String,
            MaskingStrategyKind::Redact,
            None,
            &["pii-admin"],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "ssn": "123-45-6789" });
        idx.apply_for_read(&mut v, &["pii-admin".into()]);
        assert_eq!(v["ssn"], "123-45-6789");
    }

    #[test]
    fn unmasked_for_does_not_help_other_roles() {
        let s = spec(vec![masked_field(
            "ssn",
            FieldKind::String,
            MaskingStrategyKind::Redact,
            None,
            &["pii-admin"],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "ssn": "x" });
        idx.apply_for_read(&mut v, &["other-role".into()]);
        assert_eq!(v["ssn"], "***");
    }

    #[test]
    fn partial_keeps_last_n_chars() {
        let s = spec(vec![masked_field(
            "card_number",
            FieldKind::String,
            MaskingStrategyKind::Partial,
            Some(4),
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "card_number": "4111111111111111" });
        idx.apply_for_read(&mut v, &[]);
        assert_eq!(v["card_number"], "************1111");
    }

    #[test]
    fn partial_keeps_at_most_string_length() {
        let s = spec(vec![masked_field(
            "name",
            FieldKind::String,
            MaskingStrategyKind::Partial,
            Some(99),
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "name": "hi" });
        idx.apply_for_read(&mut v, &[]);
        // Keep > length ⇒ nothing masked.
        assert_eq!(v["name"], "hi");
    }

    #[test]
    fn partial_on_non_string_field_falls_back_to_redact() {
        // Compile-time decision: Partial only valid on string-shaped
        // fields. We expose it as a Broken mask that degrades to redact
        // so a bad CRD never silently passes the raw value through.
        let s = spec(vec![masked_field(
            "amount",
            FieldKind::Integer,
            MaskingStrategyKind::Partial,
            Some(2),
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "amount": 12345 });
        idx.apply_for_read(&mut v, &[]);
        assert_eq!(v["amount"], "***");
    }

    #[test]
    fn hash_produces_stable_prefix() {
        let s = spec(vec![masked_field(
            "email",
            FieldKind::String,
            MaskingStrategyKind::Hash,
            None,
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut a = json!({ "email": "x@y.z" });
        let mut b = json!({ "email": "x@y.z" });
        idx.apply_for_read(&mut a, &[]);
        idx.apply_for_read(&mut b, &[]);
        assert_eq!(a["email"], b["email"]);
        assert!(a["email"].as_str().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn null_values_are_left_alone() {
        // A null on a masked field means "no value" — masking a null
        // would change the response shape for no benefit.
        let s = spec(vec![masked_field(
            "ssn",
            FieldKind::String,
            MaskingStrategyKind::Redact,
            None,
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "ssn": null });
        idx.apply_for_read(&mut v, &[]);
        assert!(v["ssn"].is_null());
    }

    #[test]
    fn apply_recurses_into_arrays_of_objects() {
        let s = spec(vec![masked_field(
            "price",
            FieldKind::Number,
            MaskingStrategyKind::Redact,
            None,
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut rows = json!([
            { "po_number": "PO-1", "price": 42 },
            { "po_number": "PO-2", "price": 99 }
        ]);
        idx.apply_for_read(&mut rows, &[]);
        for r in rows.as_array().unwrap() {
            assert_eq!(r["price"], "***");
            assert_eq!(r["po_number"].as_str().unwrap()[..3].to_string(), "PO-".to_string());
        }
    }

    #[test]
    fn mask_after_strip_does_not_resurrect_removed_field() {
        // The handler runs Layer-5 strip BEFORE Layer-6 mask. If a
        // field has been stripped (caller can't read it), Layer 6 must
        // not add anything back — pin that contract in a unit by
        // composing strip+mask exactly the way handlers.rs does.
        use crate::field_filter::FieldFilterIndex;
        use velocity_types::crds::schema::FieldAccess;

        let mut gated = base_field("ssn", FieldKind::String);
        gated.access = Some(FieldAccess {
            read: vec!["pii-reader".into()],
            write: Vec::new(),
        });
        gated.mask = Some(MaskingSpec {
            strategy: MaskingStrategyKind::Redact,
            keep_last: None,
            unmasked_for: Vec::new(),
        });

        let s = spec(vec![gated]);
        let ff = FieldFilterIndex::from_spec(&s);
        let mask = MaskingIndex::from_spec(&s);

        let mut row = json!({ "ssn": "123-45-6789", "id": "abc" });
        // Caller carries no roles — strip removes `ssn`.
        ff.strip_for_read(&mut row, &[]);
        // Mask must not invent a `***` for the now-absent key.
        mask.apply_for_read(&mut row, &[]);
        assert!(row.get("ssn").is_none(), "stripped field must not be resurrected by mask");
        assert_eq!(row["id"], "abc");
    }

    #[test]
    fn unknown_fields_in_payload_are_ignored() {
        let s = spec(vec![masked_field(
            "ssn",
            FieldKind::String,
            MaskingStrategyKind::Redact,
            None,
            &[],
        )]);
        let idx = MaskingIndex::from_spec(&s);
        let mut v = json!({ "ssn": "x", "extra": "y" });
        idx.apply_for_read(&mut v, &[]);
        assert_eq!(v["ssn"], "***");
        assert_eq!(v["extra"], "y");
    }
}
