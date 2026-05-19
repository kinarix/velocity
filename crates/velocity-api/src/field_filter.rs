//! Layer 5 — per-field read/write gating from `FieldSpec.access`.
//!
//! Each schema field can declare role lists that limit who may *read* its
//! value off a row and who may *write* it on a create/update. Both lists
//! follow the same open-default semantic the other access layers use:
//!
//! - empty `read` list ⇒ the field is readable by anyone who can read the row
//! - empty `write` list ⇒ the field is writable by anyone who can write the row
//! - non-empty list ⇒ the caller must carry at least one of the named roles
//!
//! ## Read strip vs write reject — why they're asymmetric
//!
//! On read we *silently strip* fields the caller isn't entitled to. Returning
//! 403 there would leak the row's existence and the field's existence — and
//! the caller has already passed Layer 1 RBAC on the route, so stripping is
//! the right shape.
//!
//! On write we *reject* the whole request with 403 `FIELD_WRITE_DENIED`. A
//! silent drop on writes would let a caller submit `{ "price": 1 }`, get a
//! 201 back, and find the price unchanged in the row — confusing at best,
//! a data integrity hazard at worst. Loud-fail is the only sane choice.
//!
//! ## Where it gets applied
//!
//! - CREATE / UPDATE: [`FieldFilterIndex::check_writes`] runs after Layer-2
//!   ABAC, before validation. Failing here doesn't waste a DB round-trip.
//! - LIST / GET: [`FieldFilterIndex::strip_for_read`] runs on every row in
//!   the response after the SQL completes. Both endpoints must call it.
//!
//! Skipping the strip on either endpoint would let a caller read a hidden
//! field by switching to the other verb — the unit tests pin both call
//! sites.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};
use velocity_types::crds::schema::{FieldAccess, FieldSpec, SchemaDefinitionSpec};

/// Pre-computed `field_name -> FieldAccess` map, with one entry per field
/// that has a non-default `access` block. Built at schema-resolve time so
/// the request hot path is a single `HashMap` lookup per touched field.
#[derive(Debug, Default)]
pub struct FieldFilterIndex {
    by_field: HashMap<String, FieldAccess>,
}

impl FieldFilterIndex {
    pub fn from_spec(spec: &SchemaDefinitionSpec) -> Self {
        let mut by_field = HashMap::new();
        for f in &spec.fields {
            if let Some(a) = field_access(f) {
                by_field.insert(f.name.clone(), a);
            }
        }
        Self { by_field }
    }

    pub fn is_empty(&self) -> bool {
        self.by_field.is_empty()
    }

    /// Returns `true` if `roles` may *read* `field`. Fields with no access
    /// block (or an empty `read` list) are readable by anyone — that
    /// matches the open default everywhere else.
    pub fn read_visible(&self, field: &str, roles: &[String]) -> bool {
        match self.by_field.get(field) {
            None => true,
            Some(a) if a.read.is_empty() => true,
            Some(a) => roles.iter().any(|r| a.read.contains(r)),
        }
    }

    /// Returns `true` if `roles` may *write* `field`. Same open-default
    /// rule as [`Self::read_visible`].
    pub fn write_allowed(&self, field: &str, roles: &[String]) -> bool {
        match self.by_field.get(field) {
            None => true,
            Some(a) if a.write.is_empty() => true,
            Some(a) => roles.iter().any(|r| a.write.contains(r)),
        }
    }

    /// Strip every field the caller can't read from the JSON row in place.
    /// No-op if `value` isn't a JSON object. We also recurse into nested
    /// arrays of objects (returned by `row_to_json` on jsonb columns) so a
    /// sensitive sub-object can't leak through a nested column.
    pub fn strip_for_read(&self, value: &mut Value, roles: &[String]) {
        if self.is_empty() {
            return;
        }
        match value {
            Value::Object(map) => {
                map.retain(|k, _| self.read_visible(k, roles));
                // No deep-strip on nested objects: nested fields aren't
                // first-class schema fields, so they have no `access` block
                // to consult. A schema author who needs to gate a sub-field
                // must promote it to a top-level field.
                for v in map.values_mut() {
                    if v.is_array() {
                        self.strip_for_read(v, roles);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.strip_for_read(item, roles);
                }
            }
            _ => {}
        }
    }

    /// Strip JSON-Patch operations whose `path` references a field the
    /// caller can't read. Used by the time-machine endpoints (`/history`,
    /// `/replay`) which surface diffs computed before the per-reader
    /// strip ran. Without this, a reader without role X could read
    /// stripped-field values through the diff channel even though
    /// [`Self::strip_for_read`] hides them on the payload.
    ///
    /// The path-segment check matches the strip's shape: only the FIRST
    /// path component is consulted (`/region` and `/region/sub` both
    /// gate on `region`). RFC 6902 paths are escaped (`~0` for `~`,
    /// `~1` for `/`); we unescape the first segment before looking it
    /// up. Operations whose path doesn't reference a top-level field
    /// (`/`, empty path) are kept verbatim — the strip never decides
    /// for them.
    ///
    /// No-op when the index is empty or `diff` isn't an array.
    pub fn strip_diff_for_read(&self, diff: &mut Value, roles: &[String]) {
        if self.is_empty() {
            return;
        }
        let ops = match diff.as_array_mut() {
            Some(a) => a,
            None => return,
        };
        ops.retain(|op| {
            let path = match op.get("path").and_then(Value::as_str) {
                Some(p) => p,
                None => return true,
            };
            let first = match first_path_segment(path) {
                Some(s) => s,
                None => return true,
            };
            // If the strip would remove this top-level field, drop the
            // patch op too. `read_visible` returns true for unknown or
            // unrestricted fields, so unrelated metadata ops survive.
            self.read_visible(&first, roles)
        });
    }

    /// Returns the list of field names in `payload` the caller may not
    /// write. Empty list ⇒ the write is admissible. We only check keys
    /// that correspond to a known field (anything else is "out-of-band"
    /// per the validator's stance and will be silently ignored at SQL
    /// build time — flagging it here would be a confusing 403 on a stray
    /// `_metadata` key).
    pub fn check_writes(&self, payload: &Map<String, Value>, roles: &[String]) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut denied = Vec::new();
        for k in payload.keys() {
            // Only fields that have an access block can be denied — others
            // pass straight through. (Fields not declared on the schema
            // are out-of-band and handled by the validator.)
            if !self.by_field.contains_key(k) {
                continue;
            }
            if !self.write_allowed(k, roles) {
                denied.push(k.clone());
            }
        }
        denied.sort();
        denied
    }
}

/// Extract and unescape the first segment of an RFC 6902 JSON Pointer.
/// Returns `None` for the empty pointer (`""`) or the root (`"/"`)
/// where there is no field name to decide against — callers keep those
/// ops verbatim.
fn first_path_segment(path: &str) -> Option<String> {
    let trimmed = path.strip_prefix('/')?;
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed.split('/').next().unwrap_or("");
    if first.is_empty() {
        return None;
    }
    // RFC 6902 escapes: `~1` → `/`, `~0` → `~`. Order matters because
    // `~0` could otherwise eat the `~` in `~1`.
    Some(first.replace("~1", "/").replace("~0", "~"))
}

fn field_access(f: &FieldSpec) -> Option<FieldAccess> {
    match &f.access {
        Some(a) if !a.read.is_empty() || !a.write.is_empty() => Some(a.clone()),
        _ => None,
    }
}

/// Convenience handle for the wrap that goes on `ResolvedSchema`.
pub type SharedFieldFilter = Arc<FieldFilterIndex>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::common::NamespacedRef;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
        SearchSpec, SearchTier,
    };

    fn field(name: &str, read: &[&str], write: &[&str]) -> FieldSpec {
        let mut f: FieldSpec =
            serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
        f.kind = FieldKind::String;
        f.access = Some(FieldAccess {
            read: read.iter().map(|s| (*s).to_string()).collect(),
            write: write.iter().map(|s| (*s).to_string()).collect(),
        });
        f
    }

    fn open_field(name: &str) -> FieldSpec {
        let mut f: FieldSpec =
            serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
        f.kind = FieldKind::String;
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
    fn no_access_blocks_is_empty_index() {
        let s = spec(vec![open_field("po_number"), open_field("notes")]);
        let idx = FieldFilterIndex::from_spec(&s);
        assert!(idx.is_empty());
        assert!(idx.read_visible("po_number", &[]));
        assert!(idx.write_allowed("po_number", &[]));
    }

    #[test]
    fn open_default_admits_everyone() {
        let s = spec(vec![field("po_number", &[], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        // Empty read/write lists on a populated FieldAccess still admit —
        // we treat both empty-vec and absent-access as open. The intent is
        // "non-empty restricts; everything else opens".
        assert!(idx.is_empty(), "fully empty access block must collapse to no index entry");
    }

    #[test]
    fn read_visible_admits_role_on_list_and_denies_others() {
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        assert!(idx.read_visible("price", &["pricing-reader".into()]));
        assert!(!idx.read_visible("price", &["nobody".into()]));
        assert!(!idx.read_visible("price", &[]));
    }

    #[test]
    fn write_allowed_separate_from_read() {
        // A field that's readable by anyone but writable only by pricing-admin.
        let s = spec(vec![field("price", &[], &["pricing-admin"])]);
        let idx = FieldFilterIndex::from_spec(&s);
        assert!(idx.read_visible("price", &["anyone".into()]));
        assert!(idx.write_allowed("price", &["pricing-admin".into()]));
        assert!(!idx.write_allowed("price", &["pricing-reader".into()]));
    }

    #[test]
    fn strip_for_read_removes_forbidden_field() {
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut row = json!({ "po_number": "PO-1", "price": 42, "id": "abc" });
        idx.strip_for_read(&mut row, &[]);
        assert!(row.get("price").is_none());
        assert!(row.get("po_number").is_some());
        assert!(row.get("id").is_some());
    }

    #[test]
    fn strip_for_read_recurses_into_arrays() {
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut rows = json!([
            { "po_number": "PO-1", "price": 42 },
            { "po_number": "PO-2", "price": 99 }
        ]);
        idx.strip_for_read(&mut rows, &[]);
        for r in rows.as_array().unwrap() {
            assert!(r.get("price").is_none());
            assert!(r.get("po_number").is_some());
        }
    }

    #[test]
    fn strip_is_no_op_when_index_empty() {
        let s = spec(vec![open_field("po_number")]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut row = json!({ "po_number": "PO-1", "ghost": "x" });
        idx.strip_for_read(&mut row, &[]);
        // Even an unknown field is left alone — the strip only acts on
        // declared, gated fields.
        assert!(row.get("ghost").is_some());
    }

    #[test]
    fn check_writes_flags_forbidden_fields() {
        let s = spec(vec![field("price", &[], &["pricing-admin"]), open_field("po_number")]);
        let idx = FieldFilterIndex::from_spec(&s);
        let payload: Map<String, Value> =
            serde_json::from_value(json!({ "po_number": "PO-1", "price": 42 })).unwrap();
        let denied = idx.check_writes(&payload, &["pricing-reader".into()]);
        assert_eq!(denied, vec!["price".to_string()]);
    }

    #[test]
    fn check_writes_ignores_unknown_keys() {
        // Out-of-band keys (not declared on the schema) bypass the field
        // filter — the validator will decide on them. Otherwise a stray
        // `_metadata` would 403 the request, which is confusing.
        let s = spec(vec![field("price", &[], &["pricing-admin"])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let payload: Map<String, Value> =
            serde_json::from_value(json!({ "_metadata": "x" })).unwrap();
        let denied = idx.check_writes(&payload, &["pricing-reader".into()]);
        assert!(denied.is_empty());
    }

    #[test]
    fn strip_diff_removes_ops_on_forbidden_fields() {
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([
            { "op": "replace", "path": "/price", "value": 99 },
            { "op": "replace", "path": "/po_number", "value": "PO-2" },
        ]);
        idx.strip_diff_for_read(&mut diff, &[]);
        let arr = diff.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "/po_number");
    }

    #[test]
    fn strip_diff_keeps_ops_when_role_matches() {
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([
            { "op": "replace", "path": "/price", "value": 99 },
        ]);
        idx.strip_diff_for_read(&mut diff, &["pricing-reader".into()]);
        // Authorised reader sees the full diff unchanged.
        assert_eq!(diff.as_array().unwrap().len(), 1);
    }

    #[test]
    fn strip_diff_first_segment_gates_nested_path() {
        // `/price/cents` should be dropped because `price` is forbidden,
        // even though the op acts on a sub-field.
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([
            { "op": "replace", "path": "/price/cents", "value": 100 },
        ]);
        idx.strip_diff_for_read(&mut diff, &[]);
        assert!(diff.as_array().unwrap().is_empty());
    }

    #[test]
    fn strip_diff_unescapes_rfc6902_path() {
        // A field named `weird/key` is escaped as `weird~1key` in the
        // pointer — the strip must un-escape before lookup.
        let s = spec(vec![field("weird/key", &["secret"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([
            { "op": "replace", "path": "/weird~1key", "value": "x" },
        ]);
        idx.strip_diff_for_read(&mut diff, &[]);
        assert!(diff.as_array().unwrap().is_empty());
    }

    #[test]
    fn strip_diff_no_op_when_empty_index() {
        // No access blocks declared → no strip needed; diff is left alone.
        let s = spec(vec![open_field("po_number")]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([
            { "op": "replace", "path": "/po_number", "value": "PO-2" },
        ]);
        idx.strip_diff_for_read(&mut diff, &[]);
        assert_eq!(diff.as_array().unwrap().len(), 1);
    }

    #[test]
    fn strip_diff_no_op_when_not_array() {
        // A `null` or scalar diff shouldn't panic; just leave it alone.
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = Value::Null;
        idx.strip_diff_for_read(&mut diff, &[]);
        assert_eq!(diff, Value::Null);
    }

    #[test]
    fn check_writes_passes_when_role_matches() {
        let s = spec(vec![field("price", &[], &["pricing-admin"])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let payload: Map<String, Value> = serde_json::from_value(json!({ "price": 42 })).unwrap();
        let denied = idx.check_writes(&payload, &["pricing-admin".into()]);
        assert!(denied.is_empty());
    }

    #[test]
    fn strip_recurses_into_array_value_under_object_key() {
        // Exercise the recursive path for an array nested under a key
        // (line 101). The outer object has a key whose value is an
        // array of objects with a restricted field — the strip must
        // descend.
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut v = json!({
            "items": [
                { "price": 10, "name": "a" },
                { "price": 20, "name": "b" },
            ]
        });
        idx.strip_for_read(&mut v, &[]);
        let arr = v["items"].as_array().unwrap();
        assert!(arr[0].get("price").is_none(), "price stripped inside nested array");
        assert!(arr[1].get("price").is_none());
        assert_eq!(arr[0]["name"], "a");
    }

    #[test]
    fn strip_for_read_no_op_on_scalar_value() {
        // Hits the `_ => {}` arm (line 110) — strip on a raw scalar
        // is a no-op, not a panic or transformation.
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut v = json!("plain string");
        idx.strip_for_read(&mut v, &[]);
        assert_eq!(v, json!("plain string"));

        let mut n = json!(42);
        idx.strip_for_read(&mut n, &[]);
        assert_eq!(n, json!(42));
    }

    #[test]
    fn strip_diff_keeps_op_with_no_path() {
        // Line 141: a patch op missing `path` is left intact.
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([{ "op": "test", "value": 1 }]);
        idx.strip_diff_for_read(&mut diff, &[]);
        assert_eq!(diff.as_array().unwrap().len(), 1);
    }

    #[test]
    fn strip_diff_keeps_op_with_root_path() {
        // Line 145 + lines 188/192: `path = "/"` or `""` resolves to
        // no first segment — op must be kept verbatim, no field
        // to gate on.
        let s = spec(vec![field("price", &["pricing-reader"], &[])]);
        let idx = FieldFilterIndex::from_spec(&s);
        let mut diff = json!([
            { "op": "replace", "path": "", "value": {} },
            { "op": "replace", "path": "/", "value": {} },
            { "op": "replace", "path": "//", "value": {} },
        ]);
        idx.strip_diff_for_read(&mut diff, &[]);
        // All three remain because none resolves to a first segment.
        assert_eq!(diff.as_array().unwrap().len(), 3);
    }
}
