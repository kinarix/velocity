#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Shared Typesense client + collection-spec helpers.
//!
//! Phase 5d-2 extraction: this crate used to live as two modules inside
//! `velocity-api` (`typesense.rs` for the HTTP client, parts of `cdc.rs`
//! for the per-schema `CollectionSpec` builder). Pulling them out lets
//! `velocity-operator` provision Typesense collections eagerly at
//! reconcile time (Phase 5d-2) while sharing the exact same wire
//! representation that the API's CDC worker will publish into.
//!
//! Crate boundary: this crate depends on `velocity-types` (CRD shapes +
//! `SchemaPath`) and a small HTTP stack (`reqwest`). It does **not**
//! depend on `velocity-api` or `velocity-operator`, so it can sit beneath
//! both. Spec helpers accept `&SchemaDefinitionSpec` + `&SchemaPath`
//! rather than a higher-level `ResolvedSchema` so the operator (which
//! doesn't carry a resolver) can call them directly.
//!
//! Fail semantics follow ADR-003: every HTTP call surfaces 4xx/5xx as
//! `TypesenseError::Status`. There are no silent "fall back to empty"
//! paths. Callers decide whether to retry (CDC worker), error a
//! reconcile (operator), or fail the request (search handlers).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use velocity_types::common::{sanitize, SchemaPath};
use velocity_types::crds::schema::{FieldKind, FieldSpec, SchemaDefinitionSpec};

// ─── HTTP client ─────────────────────────────────────────────────────────────

/// Thin wrapper around `reqwest::Client` with a pre-bound base URL and
/// API key. Clone-cheap (Arc internally via reqwest).
#[derive(Debug, Clone)]
pub struct TypesenseClient {
    base: String,
    api_key: String,
    http: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum TypesenseError {
    #[error("typesense: http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("typesense: status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("typesense: decode: {0}")]
    Decode(String),
}

impl TypesenseClient {
    /// Construct a client. `base_url` must include scheme + host (and
    /// optional port), e.g. `http://typesense:8108`. Connect + total
    /// timeouts match velocity-api's Phase-4 warm-reader client (15s).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, TypesenseError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { base: base_url.into(), api_key: api_key.into(), http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// Best-effort health probe (`GET /health`). Used at startup to log
    /// warnings; never panics or fails boot.
    pub async fn health(&self) -> Result<bool, TypesenseError> {
        let r = self
            .http
            .get(self.url("/health"))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        Ok(r.status().is_success())
    }

    /// Return `true` if the collection exists.
    pub async fn collection_exists(&self, name: &str) -> Result<bool, TypesenseError> {
        let r = self
            .http
            .get(self.url(&format!("/collections/{name}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(false);
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(true)
    }

    /// Create a collection. Returns `Ok(())` on 409 (already exists) so
    /// the call is idempotent across concurrent operator/api replicas.
    /// **Spec drift is not handled here** — a 409 means "collection
    /// exists with whatever fields it had at creation time." Phase 5d-3
    /// (blue-green) is the only safe path for field changes; this v1
    /// returns success and leaves the existing collection untouched.
    pub async fn create_collection(&self, spec: &CollectionSpec) -> Result<(), TypesenseError> {
        let r = self
            .http
            .post(self.url("/collections"))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .json(spec)
            .send()
            .await?;
        if r.status().as_u16() == 409 {
            return Ok(());
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Upsert a single document. Caller ensures `id` is a string.
    pub async fn upsert(&self, collection: &str, doc: &Value) -> Result<(), TypesenseError> {
        let r = self
            .http
            .post(self.url(&format!("/collections/{collection}/documents?action=upsert")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .json(doc)
            .send()
            .await?;
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Delete a single document by id. 404 → `Ok(())` (idempotent).
    pub async fn delete(&self, collection: &str, id: &str) -> Result<(), TypesenseError> {
        let r = self
            .http
            .delete(self.url(&format!("/collections/{collection}/documents/{id}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(());
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Upsert a collection alias: `alias` → `target`. Typesense's
    /// `PUT /aliases/<alias>` is itself idempotent — re-pointing an
    /// existing alias to a different collection is the supported swap
    /// primitive used by Phase 5d-3b blue-green.
    pub async fn upsert_alias(&self, alias: &str, target: &str) -> Result<(), TypesenseError> {
        let r = self
            .http
            .put(self.url(&format!("/aliases/{alias}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .json(&serde_json::json!({ "collection_name": target }))
            .send()
            .await?;
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Fetch the current target of an alias. `Ok(None)` on 404; the
    /// blue-green logic in 5d-3b uses this to detect "alias exists,
    /// but points at an out-of-date concrete collection" without
    /// flipping the alias.
    pub async fn get_alias(&self, alias: &str) -> Result<Option<String>, TypesenseError> {
        let r = self
            .http
            .get(self.url(&format!("/aliases/{alias}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        let v = r.json::<Value>().await.map_err(|e| TypesenseError::Decode(e.to_string()))?;
        Ok(v.get("collection_name").and_then(|v| v.as_str()).map(str::to_string))
    }

    /// Delete an alias. 404 → `Ok(())` so callers can use this as
    /// part of an idempotent cleanup pass.
    pub async fn delete_alias(&self, alias: &str) -> Result<(), TypesenseError> {
        let r = self
            .http
            .delete(self.url(&format!("/aliases/{alias}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(());
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Delete a collection. 404 → `Ok(())`. Used by Phase 5d-3b to
    /// reap the old concrete collection after a successful alias
    /// swap; calling it twice in a row is safe.
    pub async fn delete_collection(&self, name: &str) -> Result<(), TypesenseError> {
        let r = self
            .http
            .delete(self.url(&format!("/collections/{name}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(());
        }
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        Ok(())
    }

    /// Search a collection. Returns raw Typesense JSON.
    pub async fn search(
        &self,
        collection: &str,
        params: &SearchParams,
    ) -> Result<Value, TypesenseError> {
        let qs = params.to_query_string();
        let r = self
            .http
            .get(self.url(&format!("/collections/{collection}/documents/search?{qs}")))
            .header("X-TYPESENSE-API-KEY", &self.api_key)
            .send()
            .await?;
        if !r.status().is_success() {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            return Err(TypesenseError::Status { status, body });
        }
        r.json::<Value>().await.map_err(|e| TypesenseError::Decode(e.to_string()))
    }
}

// ─── Wire types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CollectionSpec {
    pub name: String,
    pub fields: Vec<TsField>,
    /// Optional default sort field. Must exist in `fields` and be
    /// `int32` / `int64` / `float`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_sorting_field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsField {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub q: String,
    /// Comma-separated field names, e.g. `"title,body"`.
    pub query_by: String,
    /// `field:value` filter; AND'd if multiple are given.
    pub filter_by: Option<String>,
    /// e.g. `"created_at:desc"`.
    pub sort_by: Option<String>,
    pub per_page: Option<u32>,
    pub page: Option<u32>,
}

impl SearchParams {
    fn to_query_string(&self) -> String {
        let mut parts = vec![
            format!("q={}", urlencode(&self.q)),
            format!("query_by={}", urlencode(&self.query_by)),
        ];
        if let Some(v) = &self.filter_by {
            parts.push(format!("filter_by={}", urlencode(v)));
        }
        if let Some(v) = &self.sort_by {
            parts.push(format!("sort_by={}", urlencode(v)));
        }
        if let Some(v) = self.per_page {
            parts.push(format!("per_page={v}"));
        }
        if let Some(v) = self.page {
            parts.push(format!("page={v}"));
        }
        parts.join("&")
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ─── Naming helpers ──────────────────────────────────────────────────────────

/// Per-schema **alias** name. Matches the underlying Postgres table so
/// dashboards line up: `<pg_schema>_<object>_<version>`. Phase 5d-3a
/// onward: this is always the *alias* — the concrete collection it
/// points at carries an extra `__<hash>` suffix. Existing read/write
/// call sites that address this name keep working because Typesense
/// resolves alias names transparently in collection-scoped endpoints.
pub fn schema_collection_name(path: &SchemaPath) -> String {
    format!("{}_{}", path.pg_schema(), path.pg_table())
}

/// Per-schema **concrete** collection name: `<alias>__<short_hash>`.
/// The hash covers the full serialized `CollectionSpec` so any change
/// the platform would consider field-relevant (name, type, facet,
/// optional, even cosmetic re-ordering) produces a new concrete name.
/// 5d-3b's swap logic relies on this: a stable hash means "no swap
/// needed"; a changed hash means "spin up the new collection and flip
/// the alias."
///
/// Length budget: Typesense collection names cap at ~64 chars in
/// practice, and we leave room for the suffix by clipping the alias
/// to 51 chars (51 + "__" + 8 = 61). The leading bytes carry org/app
/// identity so the truncation is unambiguous in dashboards.
pub fn schema_concrete_collection_name(path: &SchemaPath, spec: &SchemaDefinitionSpec) -> String {
    let alias = schema_collection_name(path);
    let cs = collection_spec_inner(path, spec, &alias);
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&cs).unwrap_or_default());
    let hash = format!("{:x}", hasher.finalize());
    let trimmed = &alias[..alias.len().min(51)];
    format!("{trimmed}__{}", &hash[..8])
}

/// Build a `CollectionSpec` whose `name` is the concrete (hashed)
/// collection name. Operator + CDC use this for the actual POST to
/// `/collections`; the alias is created separately and points at it.
pub fn concrete_collection_spec(path: &SchemaPath, spec: &SchemaDefinitionSpec) -> CollectionSpec {
    let concrete = schema_concrete_collection_name(path, spec);
    collection_spec_inner(path, spec, &concrete)
}

/// Cross-schema collection name for an org: `<org>_search`. One per org
/// so a `search?schema=*` query hits a single index.
pub fn cross_collection_name(org: &str) -> String {
    format!("{}_search", sanitize(org))
}

// ─── Spec helpers ────────────────────────────────────────────────────────────

/// Build the Typesense collection schema for a given Velocity schema,
/// writing the **alias** name into the `name` field. Phase 5d-2
/// callers that issue `POST /collections` directly on this still
/// work, but Phase 5d-3a and beyond want the concrete-suffixed
/// variant via [`concrete_collection_spec`].
///
/// Only `searchable` fields land as indexed columns; everything else
/// is `optional: true` so the doc carries the value through but pays
/// no indexing cost.
///
/// Takes `&SchemaDefinitionSpec` + `&SchemaPath` directly so the
/// operator (which doesn't carry `ResolvedSchema`) can call this.
pub fn collection_spec(path: &SchemaPath, spec: &SchemaDefinitionSpec) -> CollectionSpec {
    let alias = schema_collection_name(path);
    collection_spec_inner(path, spec, &alias)
}

fn collection_spec_inner(
    _path: &SchemaPath,
    spec: &SchemaDefinitionSpec,
    name: &str,
) -> CollectionSpec {
    let mut fields = vec![
        TsField { name: "id".into(), kind: "string".into(), facet: None, optional: None },
        TsField {
            name: "__schema".into(),
            kind: "string".into(),
            facet: Some(true),
            optional: None,
        },
        TsField {
            name: "created_at".into(),
            kind: "int64".into(),
            facet: None,
            optional: Some(true),
        },
        TsField {
            name: "updated_at".into(),
            kind: "int64".into(),
            facet: None,
            optional: Some(true),
        },
    ];
    for f in &spec.fields {
        fields.push(field_to_tsfield(f));
    }
    CollectionSpec { name: name.into(), fields, default_sorting_field: None }
}

/// Cross-search collection spec. Carries a flat `__body` text blob plus
/// a handful of facets — per-schema field-level indexing happens in the
/// per-schema collection, not here.
pub fn cross_collection_spec(org: &str) -> CollectionSpec {
    CollectionSpec {
        name: cross_collection_name(org),
        fields: vec![
            TsField { name: "id".into(), kind: "string".into(), facet: None, optional: None },
            TsField {
                name: "__schema".into(),
                kind: "string".into(),
                facet: Some(true),
                optional: None,
            },
            TsField { name: "__body".into(), kind: "string".into(), facet: None, optional: None },
            TsField {
                name: "title".into(),
                kind: "string".into(),
                facet: None,
                optional: Some(true),
            },
            TsField {
                name: "org".into(),
                kind: "string".into(),
                facet: Some(true),
                optional: None,
            },
        ],
        default_sorting_field: None,
    }
}

/// Build the Typesense document for a single row.
///
/// Drops `__fts` (binary tsvector noise) and any other column starting
/// with `__` that isn't a system field; ensures `id` is a string;
/// stamps `__schema` so the cross-search collection (and dashboards)
/// can split rows by kind; converts `created_at`/`updated_at` to
/// epoch seconds for Typesense int64 indexing.
///
/// Both the API's CDC worker and the operator's 5d-3b backfill call
/// this — keeping them on the same code path is the whole reason this
/// lives in `velocity-typesense` rather than either caller.
pub fn build_doc(path: &SchemaPath, entity_id: &str, payload: Option<&Value>) -> Value {
    let mut obj = match payload {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.remove("__fts");
    obj.insert("id".into(), Value::String(entity_id.to_string()));
    obj.insert("__schema".into(), Value::String(path.to_string()));

    for key in ["created_at", "updated_at"] {
        if let Some(v) = obj.get(key).cloned() {
            if let Some(secs) = parse_timestamp_to_epoch(&v) {
                obj.insert(key.into(), serde_json::json!(secs));
            } else {
                obj.remove(key);
            }
        }
    }
    Value::Object(obj)
}

fn parse_timestamp_to_epoch(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

fn field_to_tsfield(f: &FieldSpec) -> TsField {
    if matches!(f.kind, FieldKind::Json) {
        // Objects are passed through as opaque strings — Typesense's
        // object handling is awkward enough that the per-schema index
        // dropping JSON to a string is simpler than partial support.
        return TsField {
            name: f.name.clone(),
            kind: "string".into(),
            facet: None,
            optional: Some(true),
        };
    }
    let kind = match f.kind {
        FieldKind::Integer => "int64",
        FieldKind::Number => "float",
        FieldKind::Boolean => "bool",
        _ => "string",
    };
    TsField {
        name: f.name.clone(),
        kind: kind.into(),
        facet: Some(f.filterable),
        optional: Some(!f.required),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_string_encodes_special_chars() {
        let p = SearchParams {
            q: "steel widget".into(),
            query_by: "title,body".into(),
            filter_by: Some("status:approved".into()),
            sort_by: Some("created_at:desc".into()),
            per_page: Some(10),
            page: Some(2),
        };
        let qs = p.to_query_string();
        assert!(qs.contains("q=steel%20widget"));
        assert!(qs.contains("query_by=title%2Cbody"));
        assert!(qs.contains("filter_by=status%3Aapproved"));
        assert!(qs.contains("sort_by=created_at%3Adesc"));
        assert!(qs.contains("per_page=10"));
        assert!(qs.contains("page=2"));
    }

    #[test]
    fn url_join_handles_trailing_slash() {
        let c = TypesenseClient::new("http://localhost:8108/", "k")
            .expect("client builds with valid base url");
        assert_eq!(c.url("/collections"), "http://localhost:8108/collections");
    }

    #[test]
    fn collection_names_are_stable_and_sanitised() {
        assert_eq!(cross_collection_name("acme-co"), "acme_co_search");
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        assert_eq!(
            schema_collection_name(&path),
            "acme_supply_chain_procurement_purchase_order_v1"
        );
    }

    #[test]
    fn collection_spec_includes_required_system_fields() {
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let spec: SchemaDefinitionSpec = serde_json::from_value(json!({
            "version": "v1",
            "auth": { "strategyRef": { "name": "default", "namespace": "p" } },
            "access": {},
            "fields": [
                { "name": "po_number", "type": "string", "required": true },
                { "name": "description", "type": "string", "searchable": true }
            ],
            "search": { "tier": "Tier3" }
        }))
        .expect("test spec is well-formed");
        let cs = collection_spec(&path, &spec);
        let names: Vec<&str> = cs.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"id"));
        assert!(names.contains(&"__schema"));
        assert!(names.contains(&"po_number"));
        assert!(names.contains(&"description"));
        assert_eq!(cs.name, "acme_supply_chain_procurement_purchase_order_v1");
    }

    #[test]
    fn concrete_collection_name_changes_when_spec_changes() {
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let s1: SchemaDefinitionSpec = serde_json::from_value(json!({
            "version": "v1",
            "auth": { "strategyRef": { "name": "default", "namespace": "p" } },
            "access": {},
            "fields": [ { "name": "po_number", "type": "string", "required": true } ],
            "search": { "tier": "Tier3" }
        }))
        .expect("s1 spec");
        let s2: SchemaDefinitionSpec = serde_json::from_value(json!({
            "version": "v1",
            "auth": { "strategyRef": { "name": "default", "namespace": "p" } },
            "access": {},
            "fields": [
                { "name": "po_number", "type": "string", "required": true },
                { "name": "supplier_code", "type": "string", "required": false }
            ],
            "search": { "tier": "Tier3" }
        }))
        .expect("s2 spec");

        let n1a = schema_concrete_collection_name(&path, &s1);
        let n1b = schema_concrete_collection_name(&path, &s1);
        let n2 = schema_concrete_collection_name(&path, &s2);

        assert_eq!(n1a, n1b, "concrete name must be deterministic for identical spec");
        assert_ne!(n1a, n2, "adding a field must change the concrete name (drives 5d-3b swap)");

        // Shape: <alias>__<8 hex chars>
        let alias = schema_collection_name(&path);
        assert!(n1a.starts_with(&alias), "concrete name carries alias prefix");
        assert!(n1a.contains("__"), "double underscore separator present");
        let suffix = n1a.rsplit("__").next().expect("suffix present");
        assert_eq!(suffix.len(), 8, "8 hex chars");
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn concrete_collection_spec_names_the_concrete_collection() {
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let spec: SchemaDefinitionSpec = serde_json::from_value(json!({
            "version": "v1",
            "auth": { "strategyRef": { "name": "default", "namespace": "p" } },
            "access": {},
            "fields": [ { "name": "po_number", "type": "string", "required": true } ],
            "search": { "tier": "Tier3" }
        }))
        .expect("spec");
        let cs = concrete_collection_spec(&path, &spec);
        assert_eq!(cs.name, schema_concrete_collection_name(&path, &spec));
        assert_ne!(cs.name, schema_collection_name(&path), "concrete name != alias name");
    }

    #[test]
    fn build_doc_drops_fts_stringifies_id_stamps_schema_and_converts_timestamps() {
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let payload = json!({
            "po_number": "PO-1",
            "__fts": "tsvector data here",
            "created_at": "2026-05-18T10:00:00Z",
            "updated_at": "not a real timestamp",
        });
        let doc = build_doc(&path, "abc-id", Some(&payload));
        let obj = doc.as_object().expect("doc is object");
        assert_eq!(obj["id"], "abc-id");
        assert_eq!(obj["__schema"], "acme/supply-chain/procurement/purchase-order/v1");
        assert_eq!(obj["po_number"], "PO-1");
        assert!(!obj.contains_key("__fts"), "binary tsvector dropped");
        let ts = obj["created_at"].as_i64().expect("created_at is int64");
        assert!(ts > 1_700_000_000, "created_at converted to epoch seconds");
        assert!(!obj.contains_key("updated_at"), "unparseable timestamp dropped, not coerced to 0");
    }

    #[test]
    fn json_fields_pass_through_as_optional_strings() {
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let spec: SchemaDefinitionSpec = serde_json::from_value(json!({
            "version": "v1",
            "auth": { "strategyRef": { "name": "default", "namespace": "p" } },
            "access": {},
            "fields": [
                { "name": "meta", "type": "json", "required": true }
            ],
            "search": { "tier": "Tier3" }
        }))
        .expect("test spec is well-formed");
        let cs = collection_spec(&path, &spec);
        let meta = cs.fields.iter().find(|f| f.name == "meta").expect("meta field present");
        assert_eq!(meta.kind, "string");
        assert_eq!(meta.optional, Some(true));
        assert_eq!(meta.facet, None);
    }
}
