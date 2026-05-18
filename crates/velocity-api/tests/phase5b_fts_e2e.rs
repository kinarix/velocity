#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 5b — Tier-2 Postgres FTS end-to-end.
//!
//! Provisions a schema with `search.tier=2` and `searchable: true`
//! fields, inserts rows, runs `q` against POST /query semantics, and
//! verifies websearch_to_tsquery is doing actual ranking — not just
//! returning everything.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase5b_fts_e2e

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Row as _;

use velocity_api::dsl::{self, QueryDsl};
use velocity_api::handlers;
use velocity_api::identity::Identity;
use velocity_api::registry::{ResolvedSchema, SchemaRegistry};
use velocity_api::row_filter;
use velocity_api::session::{with_session_context, RoleClass};
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

fn searchable_field(name: &str) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
    f.searchable = true;
    f.filterable = true;
    f.sortable = true;
    f
}

fn plain_field(name: &str) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
    f.required = true;
    f.filterable = true;
    f.sortable = true;
    f
}

fn tier2_spec(fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
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
        search: SearchSpec { tier: SearchTier::Tier2, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

async fn cleanup(admin: &PgPool, pg_schema: &str) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE"))
        .execute(admin)
        .await;
}

async fn insert(pool: &PgPool, schema: &ResolvedSchema, id: &Identity, payload: Value) {
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
        "INSERT INTO {table} ({}) VALUES ({})",
        cols.join(", "),
        casts.join(", "),
    );
    with_session_context(pool, schema, RoleClass::Writer, id, move |tx| {
        Box::pin(async move {
            let mut q = sqlx::query(&sql);
            for v in &vals {
                q = q.bind(v);
            }
            q.execute(&mut **tx).await?;
            Ok(())
        })
    })
    .await
    .unwrap();
}

async fn query_q(
    pool: &PgPool,
    schema: &ResolvedSchema,
    id: &Identity,
    registry: &SchemaRegistry,
    q: &str,
) -> Vec<Value> {
    let dsl_query = QueryDsl { q: Some(q.into()), ..Default::default() };
    let compiled = dsl::build(schema, &dsl_query, id, registry, None).unwrap();
    let rows = with_session_context(pool, schema, RoleClass::Reader, id, move |tx| {
        Box::pin(async move {
            let mut q = sqlx::query(&compiled.sql);
            for v in &compiled.params {
                q = row_filter::bind_json_param(q, v);
            }
            let rows = q.fetch_all(&mut **tx).await?;
            Ok(rows
                .into_iter()
                .map(|r| r.get::<Value, _>("__row"))
                .collect::<Vec<_>>())
        })
    })
    .await
    .unwrap();
    rows
}

#[tokio::test]
async fn fts_matches_searchable_fields() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let api = api_url();

    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = tier2_spec(vec![
        plain_field("po_number"),
        searchable_field("description"),
        searchable_field("supplier_notes"),
    ]);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    assert!(
        plan.main_table.contains("__fts tsvector"),
        "tier-2 schema must provision __fts"
    );
    prov.sync_schema_tables(&plan, false).await.unwrap();

    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();

    insert(
        &api_pool,
        &schema,
        &identity,
        json!({
            "po_number": "PO-1",
            "description": "Stainless steel widget for the assembly line",
            "supplier_notes": "Tata supplied",
        }),
    )
    .await;
    insert(
        &api_pool,
        &schema,
        &identity,
        json!({
            "po_number": "PO-2",
            "description": "Plastic widget",
            "supplier_notes": "Supplier ABC",
        }),
    )
    .await;
    insert(
        &api_pool,
        &schema,
        &identity,
        json!({
            "po_number": "PO-3",
            "description": "Office chair",
            "supplier_notes": "From IKEA",
        }),
    )
    .await;

    // FTS for "steel widget" should match PO-1 only (PO-2 has "widget" only,
    // but websearch is implicit-AND for unquoted terms — PO-2 is missing
    // "steel").
    let rows = query_q(&api_pool, &schema, &identity, &registry, "steel widget").await;
    let pos: Vec<String> =
        rows.iter().map(|r| r["po_number"].as_str().unwrap().into()).collect();
    assert_eq!(pos, vec!["PO-1"], "steel widget should match only PO-1");

    // FTS for "widget" alone matches PO-1 and PO-2.
    let rows = query_q(&api_pool, &schema, &identity, &registry, "widget").await;
    let pos: std::collections::HashSet<String> =
        rows.iter().map(|r| r["po_number"].as_str().unwrap().into()).collect();
    assert!(pos.contains("PO-1"));
    assert!(pos.contains("PO-2"));
    assert!(!pos.contains("PO-3"));

    // FTS matches against `supplier_notes` too (cross-field search).
    let rows = query_q(&api_pool, &schema, &identity, &registry, "Tata").await;
    let pos: Vec<String> =
        rows.iter().map(|r| r["po_number"].as_str().unwrap().into()).collect();
    assert_eq!(pos, vec!["PO-1"]);

    // The response row must NOT contain the __fts column (raw tsvector).
    let rows = query_q(&api_pool, &schema, &identity, &registry, "widget").await;
    for r in &rows {
        assert!(
            !r.as_object().unwrap().contains_key("__fts"),
            "__fts must not leak to clients"
        );
    }

    cleanup(&admin_pool, &pg_schema).await;
}
