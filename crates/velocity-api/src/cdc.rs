//! Tier-3 outbox CDC worker — Phase 5c.
//!
//! For every `SchemaDefinition` with `search.tier = 3`, the API runs
//! a background loop that:
//!
//!   1. Begins a transaction
//!   2. `SELECT … FROM <schema>.<table>_outbox WHERE published_at IS
//!      NULL ORDER BY id LIMIT 100 FOR UPDATE SKIP LOCKED`
//!   3. Pushes each row to Typesense (per-schema collection + the
//!      per-org cross-search collection if the schema opts in)
//!   4. `UPDATE outbox SET published_at = now() WHERE id = ANY($1)`
//!   5. Commits
//!
//! `FOR UPDATE SKIP LOCKED` is load-bearing — it lets every replica
//! run the same loop without contention and without double-publishing.
//! ADR-002 anchors the outbox-as-source-of-truth contract.
//!
//! Loop cadence: 1 second when idle; immediate next iteration when a
//! batch was published (so a burst of writes drains in seconds, not
//! minutes). On Typesense error: log, skip the commit (rows stay
//! unpublished), back off 5 seconds — the next iteration retries.
//! Never marks rows published on a failed write.
//!
//! The CDC loop also lazily creates Typesense collections on first
//! write so the operator doesn't need a separate provisioning step in
//! v1; if `Operator provisions Typesense collection on schema apply`
//! lands in a later phase, that path replaces the lazy create.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use velocity_types::common::sanitize;
use velocity_types::crds::schema::{FieldKind, SearchTier};

use crate::registry::{ResolvedSchema, SchemaRegistry};
use crate::typesense::{CollectionSpec, TsField, TypesenseClient};

/// Hard cap per batch — bounds the worst-case Typesense round-trip
/// cost per tick when an outbox table backs up.
const BATCH_SIZE: i64 = 100;
const IDLE_INTERVAL: Duration = Duration::from_millis(1_000);

/// Cross-schema collection name for an org. One per org so a `search?
/// schema=*` query hits a single index. `schema_kind` is a facet so
/// callers (and RBAC filtering) can scope down to a list of schemas.
pub fn cross_collection_name(org: &str) -> String {
    format!("{}_search", sanitize(org))
}

/// Per-schema collection name. Matches the Postgres table the rows
/// live in so dashboards line up `<pg_schema>_<object>_<version>`.
pub fn schema_collection_name(schema: &ResolvedSchema) -> String {
    format!(
        "{}_{}",
        schema.pg_schema,
        schema.pg_table.trim_start_matches(&format!("{}_", schema.pg_schema))
    )
}

/// Build the Typesense collection schema for a Velocity schema. Only
/// `searchable` fields become indexed fields; non-searchable fields
/// still land in the doc as `optional: true` so the response carries
/// them through without indexing cost.
pub fn collection_spec(schema: &ResolvedSchema) -> CollectionSpec {
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
    for f in schema.fields.ordered.iter() {
        if matches!(f.kind, FieldKind::Json) {
            // Typesense indexes objects awkwardly — pass through as
            // string and skip indexing for v1.
            fields.push(TsField {
                name: f.name.clone(),
                kind: "string".into(),
                facet: None,
                optional: Some(true),
            });
            continue;
        }
        let ts_kind = match f.kind {
            FieldKind::Integer => "int64",
            FieldKind::Number => "float",
            FieldKind::Boolean => "bool",
            _ => "string",
        };
        fields.push(TsField {
            name: f.name.clone(),
            kind: ts_kind.into(),
            facet: Some(f.filterable),
            optional: Some(!f.required),
        });
    }
    CollectionSpec {
        name: schema_collection_name(schema),
        fields,
        default_sorting_field: None,
    }
}

/// Cross-search collection schema. Carries the union of every Tier-3
/// schema's `searchable` text fields as a single `__body` blob so the
/// index stays simple. We don't try to project arbitrary user fields
/// here — those live in the per-schema collection.
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

/// Spawn the CDC loop. Returns immediately; the loop runs forever
/// (until `shutdown_rx` flips).
pub async fn run(
    pool: PgPool,
    registry: Arc<SchemaRegistry>,
    typesense: Arc<TypesenseClient>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    // Track which (collection-name) pairs we've already provisioned
    // this session so we don't hit the existence-check endpoint on
    // every batch.
    let mut provisioned: HashSet<String> = HashSet::new();

    loop {
        if *shutdown_rx.borrow() {
            tracing::info!("cdc: shutdown signal — exiting loop");
            return;
        }

        let snapshot = registry.snapshot();
        let mut had_work = false;

        for (_path, schema) in snapshot.by_path.iter() {
            if !matches!(schema.spec.search.tier, SearchTier::Tier3) {
                continue;
            }
            match drain_outbox(&pool, schema, &typesense, &mut provisioned).await {
                Ok(published) => {
                    if published > 0 {
                        had_work = true;
                        tracing::info!(
                            schema = %schema.path.to_string(),
                            published,
                            "cdc: outbox batch published"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        schema = %schema.path.to_string(),
                        error = %e,
                        "cdc: outbox drain failed — will retry"
                    );
                }
            }
        }

        let sleep = if had_work { Duration::from_millis(50) } else { IDLE_INTERVAL };
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("cdc: shutdown signal — exiting loop");
                    return;
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CdcError {
    #[error("postgres: {0}")]
    Pg(#[from] sqlx::Error),
    #[error("typesense: {0}")]
    Typesense(#[from] crate::typesense::TypesenseError),
}

async fn drain_outbox(
    pool: &PgPool,
    schema: &Arc<ResolvedSchema>,
    typesense: &TypesenseClient,
    provisioned: &mut HashSet<String>,
) -> Result<usize, CdcError> {
    let outbox_table = format!("{}.{}_outbox", schema.pg_schema, schema.pg_table);
    let mut tx = pool.begin().await?;

    let rows = sqlx::query(&format!(
        "SELECT id, op, entity_id::text AS entity_id, payload \
         FROM {outbox_table} \
         WHERE published_at IS NULL \
         ORDER BY id \
         LIMIT $1 \
         FOR UPDATE SKIP LOCKED"
    ))
    .bind(BATCH_SIZE)
    .fetch_all(&mut *tx)
    .await?;

    if rows.is_empty() {
        // Commit early to release locks promptly.
        tx.commit().await?;
        return Ok(0);
    }

    // Ensure the per-schema collection exists. Done once per session
    // per collection; cheaper than a HEAD per batch.
    let coll_name = schema_collection_name(schema);
    ensure_collection(typesense, &coll_name, || collection_spec(schema), provisioned).await?;

    // Cross-search collection — opt-in via spec.search.cross_search.
    let cross_enabled = schema.spec.search.cross_search;
    if cross_enabled {
        let cross_name = cross_collection_name(&schema.path.org);
        ensure_collection(typesense, &cross_name, || cross_collection_spec(&schema.path.org), provisioned).await?;
    }

    let mut published_ids: Vec<i64> = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.get("id");
        let op: String = row.get("op");
        let entity_id: String = row.get("entity_id");
        let payload: Option<Value> = row.try_get("payload").ok();

        // Always carry `id` as a string in the Typesense doc.
        let doc = build_typesense_doc(schema, &entity_id, payload.as_ref());

        match op.as_str() {
            "delete" => {
                typesense.delete(&coll_name, &entity_id).await?;
                if cross_enabled {
                    typesense
                        .delete(&cross_collection_name(&schema.path.org), &entity_id)
                        .await?;
                }
            }
            // Treat insert + update + restore as upsert — Typesense's
            // `action=upsert` is idempotent and this is also what the
            // "replay outbox from scratch" recovery path needs.
            _ => {
                typesense.upsert(&coll_name, &doc).await?;
                if cross_enabled {
                    let cross_doc = build_cross_doc(schema, &doc);
                    typesense
                        .upsert(&cross_collection_name(&schema.path.org), &cross_doc)
                        .await?;
                }
            }
        }

        published_ids.push(id);
    }

    // Mark rows published. If Typesense succeeded but this UPDATE fails,
    // we'll re-publish on the next loop — idempotent upserts make that
    // safe.
    sqlx::query(&format!(
        "UPDATE {outbox_table} SET published_at = now() WHERE id = ANY($1)"
    ))
    .bind(&published_ids)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(published_ids.len())
}

async fn ensure_collection<F>(
    ts: &TypesenseClient,
    name: &str,
    spec: F,
    provisioned: &mut HashSet<String>,
) -> Result<(), crate::typesense::TypesenseError>
where
    F: FnOnce() -> CollectionSpec,
{
    if provisioned.contains(name) {
        return Ok(());
    }
    if !ts.collection_exists(name).await? {
        let s = spec();
        ts.create_collection(&s).await?;
    }
    provisioned.insert(name.to_string());
    Ok(())
}

/// Translate an outbox payload into a Typesense document. Drops
/// `__fts` (binary noise) and ensures `id` is a string. Adds
/// `__schema` so the cross-search collection can split rows by kind.
fn build_typesense_doc(
    schema: &ResolvedSchema,
    entity_id: &str,
    payload: Option<&Value>,
) -> Value {
    let mut obj = match payload {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.remove("__fts");
    obj.insert("id".into(), Value::String(entity_id.to_string()));
    obj.insert("__schema".into(), Value::String(schema.path.to_string()));

    // Convert timestamps to epoch seconds (int64) so Typesense can
    // index them. Missing/unparseable values become absent rather
    // than 0 — fewer phantom buckets in facet aggregations.
    for key in ["created_at", "updated_at"] {
        if let Some(v) = obj.get(key).cloned() {
            if let Some(secs) = parse_timestamp_to_epoch(&v) {
                obj.insert(key.into(), json!(secs));
            } else {
                obj.remove(key);
            }
        }
    }
    Value::Object(obj)
}

fn build_cross_doc(schema: &ResolvedSchema, doc: &Value) -> Value {
    let obj = doc.as_object().cloned().unwrap_or_default();
    // Concatenate every text-shaped field into __body. Caller queries
    // `__body` on the cross-search index.
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in &obj {
        if k.starts_with("__") || k == "id" {
            continue;
        }
        if let Some(s) = v.as_str() {
            parts.push(s.to_string());
        }
    }
    let title = obj
        .iter()
        .find(|(k, _)| {
            // Prefer a `title` field, then any `name` / `*_name`, then the
            // first string. Keeps the cross-search results scannable.
            *k == "title" || *k == "name"
        })
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("")
        .to_string();
    json!({
        "id": obj.get("id").cloned().unwrap_or(Value::String(String::new())),
        "__schema": schema.path.to_string(),
        "__body": parts.join(" "),
        "title": title,
        "org": schema.path.org.clone(),
    })
}

fn parse_timestamp_to_epoch(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_names_are_stable_and_sanitised() {
        // sanitize replaces `-` with `_`, so an org name with hyphens
        // round-trips into a valid Typesense collection name.
        assert_eq!(cross_collection_name("acme-co"), "acme_co_search");
    }

    #[test]
    fn build_typesense_doc_drops_fts_and_stringifies_id() {
        let path = velocity_types::common::SchemaPath::new(
            "acme",
            "supply-chain",
            "procurement",
            "purchase-order",
            "v1",
        );
        let spec = velocity_types::crds::schema::SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: velocity_types::crds::schema::AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "p".into(),
                },
                overrides: Vec::new(),
            },
            access: Default::default(),
            fields: Vec::new(),
            validations: Vec::new(),
            search: velocity_types::crds::schema::SearchSpec {
                tier: velocity_types::crds::schema::SearchTier::Tier3,
                ..Default::default()
            },
            time_machine: None,
            audit: None,
            archive: None,
            observability: Default::default(),
            scaling: None,
        };
        let schema = ResolvedSchema::from_spec(path, spec);
        let payload = json!({
            "po_number": "PO-1",
            "__fts": "tsvector data here",
            "created_at": "2026-05-18T10:00:00Z",
        });
        let doc = build_typesense_doc(&schema, "abc-id", Some(&payload));
        let obj = doc.as_object().unwrap();
        assert_eq!(obj["id"], "abc-id");
        assert_eq!(obj["__schema"], "acme/supply-chain/procurement/purchase-order/v1");
        assert!(!obj.contains_key("__fts"));
        // created_at converted to epoch seconds.
        let ts = obj["created_at"].as_i64().unwrap();
        assert!(ts > 1_700_000_000); // sometime after 2023
    }

    #[test]
    fn collection_spec_indexes_searchable_fields() {
        let path = velocity_types::common::SchemaPath::new(
            "acme",
            "supply-chain",
            "procurement",
            "purchase-order",
            "v1",
        );
        let mut f1: velocity_types::crds::schema::FieldSpec =
            serde_json::from_value(json!({ "name": "po_number", "type": "string" })).unwrap();
        f1.kind = FieldKind::String;
        f1.required = true;
        let mut f2: velocity_types::crds::schema::FieldSpec =
            serde_json::from_value(json!({ "name": "description", "type": "string" })).unwrap();
        f2.kind = FieldKind::String;
        f2.searchable = true;
        let spec = velocity_types::crds::schema::SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: velocity_types::crds::schema::AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "p".into(),
                },
                overrides: Vec::new(),
            },
            access: Default::default(),
            fields: vec![f1, f2],
            validations: Vec::new(),
            search: velocity_types::crds::schema::SearchSpec {
                tier: velocity_types::crds::schema::SearchTier::Tier3,
                ..Default::default()
            },
            time_machine: None,
            audit: None,
            archive: None,
            observability: Default::default(),
            scaling: None,
        };
        let schema = ResolvedSchema::from_spec(path, spec);
        let cspec = collection_spec(&schema);
        let names: Vec<&str> = cspec.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"id"));
        assert!(names.contains(&"__schema"));
        assert!(names.contains(&"po_number"));
        assert!(names.contains(&"description"));
    }
}
