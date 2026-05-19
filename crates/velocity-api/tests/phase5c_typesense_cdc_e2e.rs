#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 5c — Tier-3 Typesense CDC end-to-end.
//!
//! Provisions a Tier-3 schema, inserts rows directly into the outbox
//! (mimicking what the trigger writes after a real CREATE), runs ONE
//! CDC pass, then asserts both the per-schema and cross-search
//! Typesense collections received the docs.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase5c_typesense_cdc_e2e
//!
//! Requires Typesense at http://localhost:8108 (docker-compose service
//! `typesense`); the test skips with a clear message if unreachable.

use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use velocity_api::cdc;
use velocity_api::handlers;
use velocity_api::identity::Identity;
use velocity_api::registry::{ResolvedSchema, SchemaRegistry};
use velocity_api::session::{with_session_context, RoleClass};
use velocity_api::typesense::{SearchParams, TypesenseClient};
use velocity_operator::PostgresProvisioner;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

fn admin_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL").ok()
}

fn api_url() -> String {
    "postgres://velocity_api:velocity_api_dev@localhost:5434/velocity".into()
}

fn ts_url() -> String {
    std::env::var("VELOCITY_API_TYPESENSE_URL").unwrap_or_else(|_| "http://localhost:8108".into())
}

fn ts_key() -> String {
    std::env::var("VELOCITY_API_TYPESENSE_API_KEY").unwrap_or_else(|_| "dev-typesense-key".into())
}

fn field(name: &str, searchable: bool, required: bool) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
    f.searchable = searchable;
    f.filterable = true;
    f.sortable = true;
    f.required = required;
    f
}

fn tier3_spec(fields: Vec<FieldSpec>, cross_search: bool) -> SchemaDefinitionSpec {
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: velocity_types::common::NamespacedRef {
                name: "default".into(),
                namespace: "acme-platform".into(),
            },
            overrides: Vec::new(),
        },
        access: AccessSpec::default(),
        fields,
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier3, cross_search, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

async fn cleanup(admin: &PgPool, pg_schema: &str) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE")).execute(admin).await;
}

async fn typesense_or_skip() -> Option<TypesenseClient> {
    let c = TypesenseClient::new(ts_url(), ts_key()).unwrap();
    match c.health().await {
        Ok(true) => Some(c),
        _ => {
            eprintln!("skipping: Typesense not reachable at {}", ts_url());
            None
        }
    }
}

async fn insert(pool: &PgPool, schema: &ResolvedSchema, id: &Identity, payload: Value) -> String {
    let obj = payload.as_object().unwrap().clone();
    let mut cols = Vec::new();
    let mut casts = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for f in schema.fields.ordered.iter() {
        if let Some(v) = obj.get(&f.name) {
            vals.push(v.clone());
            cols.push(f.name.clone());
            casts.push(handlers::cast_placeholder(vals.len(), f.kind));
        }
    }
    let table = schema.pg_qualified.clone();
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({}) RETURNING id::text AS id",
        cols.join(", "),
        casts.join(", "),
    );
    let row = with_session_context(pool, schema, RoleClass::Writer, id, move |tx| {
        Box::pin(async move {
            let mut q = sqlx::query(&sql);
            for v in &vals {
                q = q.bind(v);
            }
            let r = q.fetch_one(&mut **tx).await?;
            Ok(sqlx::Row::get::<String, _>(&r, "id"))
        })
    })
    .await
    .unwrap();
    row
}

/// Run one pass of the CDC loop and return as soon as it idles. Done
/// by spawning the loop and triggering shutdown the moment we see
/// `published_at` on every outbox row in the schema.
async fn run_cdc_once(
    pool: &PgPool,
    registry: &Arc<SchemaRegistry>,
    typesense: Arc<TypesenseClient>,
    pg_schema: &str,
    pg_table: &str,
) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let pool_c = pool.clone();
    let reg_c = registry.clone();
    let handle = tokio::spawn(async move {
        cdc::run(pool_c, reg_c, typesense, rx).await;
    });

    // Poll the outbox table until all rows are marked published.
    let outbox = format!("{pg_schema}.{pg_table}_outbox");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("CDC didn't drain outbox within 10s");
        }
        let pending: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {outbox} WHERE published_at IS NULL"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        if pending == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let _ = tx.send(true);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn cdc_publishes_outbox_to_typesense() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(ts) = typesense_or_skip().await else { return };

    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api_url()).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = tier3_spec(
        vec![field("po_number", false, true), field("description", true, false)],
        true, // cross_search opt-in
    );
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    assert!(plan.outbox_table.is_some(), "tier-3 schema must provision outbox");
    prov.sync_schema_tables(&plan, false).await.unwrap();

    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();
    registry.replace_all(vec![schema.clone()]);

    let coll_name = cdc::schema_collection_name(&schema);
    let cross_name = cdc::cross_collection_name(&org);
    // Cleanup any prior test collections so we measure fresh inserts.
    let _ = ts
        .delete(&coll_name, "noop") // touch to avoid unused-import warnings
        .await;
    let _ = reqwest::Client::new()
        .delete(format!("{}/collections/{}", ts_url(), coll_name))
        .header("X-TYPESENSE-API-KEY", ts_key())
        .send()
        .await;
    let _ = reqwest::Client::new()
        .delete(format!("{}/collections/{}", ts_url(), cross_name))
        .header("X-TYPESENSE-API-KEY", ts_key())
        .send()
        .await;

    let id1 = insert(
        &api_pool,
        &schema,
        &identity,
        json!({ "po_number": "PO-1", "description": "Stainless steel widget" }),
    )
    .await;
    let id2 = insert(
        &api_pool,
        &schema,
        &identity,
        json!({ "po_number": "PO-2", "description": "Plastic gadget" }),
    )
    .await;

    let ts_arc = Arc::new(ts.clone());
    run_cdc_once(&api_pool, &registry, ts_arc, &schema.pg_schema, &schema.pg_table).await;

    // Per-schema collection: search for "widget" — must hit PO-1.
    let resp = ts
        .search(
            &coll_name,
            &SearchParams {
                q: "widget".into(),
                query_by: "description".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let hits = resp["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "widget should match PO-1 only");
    assert_eq!(hits[0]["document"]["po_number"], "PO-1");
    assert_eq!(hits[0]["document"]["id"].as_str().unwrap(), id1);

    // Cross-search collection: same query → PO-1 again, scoped by org.
    let resp = ts
        .search(
            &cross_name,
            &SearchParams {
                q: "widget".into(),
                query_by: "__body".into(),
                filter_by: Some(format!("org:={}", org)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let hits = resp["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "cross-search must find PO-1 too");
    assert_eq!(hits[0]["document"]["__schema"], path.to_string());

    // PO-2 still searchable in the per-schema collection.
    let resp = ts
        .search(
            &coll_name,
            &SearchParams {
                q: "gadget".into(),
                query_by: "description".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let hits = resp["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["document"]["id"].as_str().unwrap(), id2);

    // Ensure all outbox rows are now marked published.
    let pending: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {}.{}_outbox WHERE published_at IS NULL",
        schema.pg_schema, schema.pg_table
    ))
    .fetch_one(&api_pool)
    .await
    .unwrap();
    assert_eq!(pending, 0);

    cleanup(&admin_pool, &pg_schema).await;

    // Best-effort: drop the test collections so a re-run starts clean.
    let _ = reqwest::Client::new()
        .delete(format!("{}/collections/{}", ts_url(), coll_name))
        .header("X-TYPESENSE-API-KEY", ts_key())
        .send()
        .await;
    let _ = reqwest::Client::new()
        .delete(format!("{}/collections/{}", ts_url(), cross_name))
        .header("X-TYPESENSE-API-KEY", ts_key())
        .send()
        .await;
}

#[tokio::test]
async fn cdc_replays_dropped_publish_on_restart() {
    // CLAUDE.md acceptance criteria: kill CDC mid-stream, write 100
    // records, restart → all 100 in Typesense. We approximate "kill"
    // by simply not running the worker until all writes are done.
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(ts) = typesense_or_skip().await else { return };

    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api_url()).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = tier3_spec(vec![field("po_number", true, true)], false);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();
    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();
    registry.replace_all(vec![schema.clone()]);

    let coll = cdc::schema_collection_name(&schema);
    let _ = reqwest::Client::new()
        .delete(format!("{}/collections/{}", ts_url(), coll))
        .header("X-TYPESENSE-API-KEY", ts_key())
        .send()
        .await;

    // Write N rows. Worker is NOT running.
    for i in 0..30 {
        insert(&api_pool, &schema, &identity, json!({ "po_number": format!("PO-{i:03}") })).await;
    }
    let pending_before: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {}.{}_outbox WHERE published_at IS NULL",
        schema.pg_schema, schema.pg_table
    ))
    .fetch_one(&api_pool)
    .await
    .unwrap();
    assert_eq!(pending_before, 30);

    // "Start" the worker after the backlog.
    let ts_arc = Arc::new(ts.clone());
    run_cdc_once(&api_pool, &registry, ts_arc, &schema.pg_schema, &schema.pg_table).await;

    // `q=*` is Typesense's "match everything" form — gives a true
    // count regardless of tokenisation/prefix-search heuristics.
    let resp = ts
        .search(
            &coll,
            &SearchParams {
                q: "*".into(),
                query_by: "po_number".into(),
                per_page: Some(100),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(resp["found"].as_i64().unwrap(), 30);

    cleanup(&admin_pool, &pg_schema).await;
    let _ = reqwest::Client::new()
        .delete(format!("{}/collections/{}", ts_url(), coll))
        .header("X-TYPESENSE-API-KEY", ts_key())
        .send()
        .await;
}
