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
use velocity_types::crds::schema::SearchTier;

use crate::registry::{ResolvedSchema, SchemaRegistry};
use crate::typesense::{CollectionSpec, TypesenseClient};
pub use velocity_typesense::cross_collection_name;
use velocity_typesense::{
    collection_spec as ts_collection_spec, cross_collection_spec,
    schema_collection_name as ts_schema_collection_name,
};

/// Hard cap per batch — bounds the worst-case Typesense round-trip
/// cost per tick when an outbox table backs up.
const BATCH_SIZE: i64 = 100;
const IDLE_INTERVAL: Duration = Duration::from_millis(1_000);

/// Per-schema collection name for a `ResolvedSchema` — thin wrapper
/// over `velocity_typesense::schema_collection_name(&path)` so callers
/// don't have to spell out the field access.
pub fn schema_collection_name(schema: &ResolvedSchema) -> String {
    ts_schema_collection_name(&schema.path)
}

/// Collection spec built from a `ResolvedSchema`. Forwards to
/// `velocity_typesense::collection_spec(&path, &spec)` — the operator
/// uses the same builder over `SchemaDefinitionSpec` directly.
pub fn collection_spec(schema: &ResolvedSchema) -> CollectionSpec {
    ts_collection_spec(&schema.path, &schema.spec)
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

    // Ensure the per-schema collection (concrete + alias) exists.
    // Phase 5d-3a: writes/reads address the *alias* name; the concrete
    // collection carries a content-hash suffix so 5d-3b can spin up a
    // new one and flip the alias atomically. ensure_aliased_collection
    // is a no-op when the alias already points anywhere — re-reconcile
    // never causes an unintended swap.
    let coll_name = schema_collection_name(schema);
    ensure_aliased_collection(typesense, schema, provisioned).await?;

    // Cross-search collection — opt-in via spec.search.cross_search.
    // Stays un-aliased: its shape is fixed (id, __schema, __body, …),
    // so blue-green doesn't apply.
    let cross_enabled = schema.spec.search.cross_search;
    if cross_enabled {
        let cross_name = cross_collection_name(&schema.path.org);
        ensure_collection(
            typesense,
            &cross_name,
            || cross_collection_spec(&schema.path.org),
            provisioned,
        )
        .await?;
    }

    let mut published_ids: Vec<i64> = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.get("id");
        let op: String = row.get("op");
        let entity_id: String = row.get("entity_id");
        let payload: Option<Value> = row.try_get("payload").ok();

        // Always carry `id` as a string in the Typesense doc.
        let doc = velocity_typesense::build_doc(&schema.path, &entity_id, payload.as_ref());

        match op.as_str() {
            "delete" => {
                typesense.delete(&coll_name, &entity_id).await?;
                if cross_enabled {
                    typesense.delete(&cross_collection_name(&schema.path.org), &entity_id).await?;
                }
            }
            // Treat insert + update + restore as upsert — Typesense's
            // `action=upsert` is idempotent and this is also what the
            // "replay outbox from scratch" recovery path needs.
            _ => {
                typesense.upsert(&coll_name, &doc).await?;
                if cross_enabled {
                    let cross_doc = build_cross_doc(schema, &doc);
                    typesense.upsert(&cross_collection_name(&schema.path.org), &cross_doc).await?;
                }
            }
        }

        published_ids.push(id);
    }

    // Mark rows published. If Typesense succeeded but this UPDATE fails,
    // we'll re-publish on the next loop — idempotent upserts make that
    // safe.
    sqlx::query(&format!("UPDATE {outbox_table} SET published_at = now() WHERE id = ANY($1)"))
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

/// Phase 5d-3a: ensure the per-schema concrete collection exists and
/// the alias points at it. If the alias already exists (regardless of
/// target), leave it alone — re-reconcile must not silently swap
/// search out from under live traffic. The explicit swap belongs to
/// 5d-3b.
async fn ensure_aliased_collection(
    ts: &TypesenseClient,
    schema: &Arc<ResolvedSchema>,
    provisioned: &mut HashSet<String>,
) -> Result<(), crate::typesense::TypesenseError> {
    let alias = schema_collection_name(schema);
    if provisioned.contains(&alias) {
        return Ok(());
    }
    let concrete_name =
        velocity_typesense::schema_concrete_collection_name(&schema.path, &schema.spec);
    let concrete_spec = velocity_typesense::concrete_collection_spec(&schema.path, &schema.spec);
    ts.create_collection(&concrete_spec).await?;
    if ts.get_alias(&alias).await?.is_none() {
        ts.upsert_alias(&alias, &concrete_name).await?;
    }
    provisioned.insert(alias);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_names_are_stable_and_sanitised() {
        // sanitize replaces `-` with `_`, so an org name with hyphens
        // round-trips into a valid Typesense collection name.
        assert_eq!(cross_collection_name("acme-co"), "acme_co_search");
    }

    // build_doc / parse_timestamp_to_epoch live in velocity-typesense and
    // are covered there; collection-spec construction is also exercised
    // there.
}
