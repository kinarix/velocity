#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 5a — POST /query DSL end-to-end.
//!
//! Drives the DSL compiler + the actual handler SQL against a docker-compose
//! Postgres. The unit tests (`velocity-api lib`) cover SQL shape, validation,
//! and cursor signing; this file proves the compiled SQL is one Postgres
//! will accept and that keyset pagination + cross-schema RBAC behave as
//! advertised.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase5a_query_dsl_e2e

use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Row as _;

use velocity_data_api::dsl::{self, DslOp, QueryDsl, SortField, WhereNode};
use velocity_data_api::handlers;
use velocity_core::CursorSigner;
use velocity_core::identity::Identity;
use velocity_core::registry::{ResolvedSchema, SchemaRegistry};
use velocity_core::row_filter;
use velocity_data_api::session::{with_session_context, RoleClass};
use velocity_operator::PostgresProvisioner;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

fn admin_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

fn api_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_API_URL")
        .ok()
        .or_else(|| Some("postgres://velocity_api:velocity_api_dev@localhost:5434/velocity".into()))
}

fn field(name: &str, kind: FieldKind, filterable: bool, sortable: bool) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f.filterable = filterable;
    f.sortable = sortable;
    f
}

fn ref_field(name: &str, target: &SchemaPath) -> FieldSpec {
    let mut f = field(name, FieldKind::Ref, true, false);
    f.r#ref = Some(velocity_types::common::ObjectRef {
        org: target.org.clone(),
        app: target.app.clone(),
        domain: target.domain.clone(),
        object: target.object.clone(),
        version: target.version.clone(),
        key: "id".into(),
    });
    f
}

fn spec(fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
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
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
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

fn signer() -> CursorSigner {
    CursorSigner::new(b"phase5a-test-cursor-key-32-bytes!!!".to_vec()).unwrap()
}

async fn insert_row(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    payload: Value,
) -> Value {
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
        "INSERT INTO {table} ({}) VALUES ({}) RETURNING row_to_json({table}.*) AS row",
        cols.join(", "),
        casts.join(", ")
    );
    let row = with_session_context(pool, schema, RoleClass::Writer, identity, move |tx| {
        Box::pin(async move {
            let mut q = sqlx::query(&sql);
            for v in &vals {
                q = q.bind(v);
            }
            let row = q.fetch_one(&mut **tx).await?;
            Ok(row.get::<Value, _>("row"))
        })
    })
    .await
    .expect("insert");
    row
}

/// Run a compiled DSL query against Postgres and return the rows the
/// handler would produce (with includes folded back into the main row).
async fn run_query(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    registry: &SchemaRegistry,
    dsl_query: &QueryDsl,
    signer: Option<&CursorSigner>,
) -> (Vec<Value>, Option<String>) {
    let compiled = dsl::build(schema, dsl_query, identity, registry, signer).unwrap();
    let include_names: Vec<String> = dsl_query.include.clone();
    let page_limit = compiled.limit;
    let cursor_sort_sig = compiled.cursor_sort_sig.clone();
    let cursor_sort_fields = compiled.cursor_sort_fields.clone();
    let schema_key = compiled.schema_key.clone();

    let rows: Vec<Value> =
        with_session_context(pool, schema, RoleClass::Reader, identity, move |tx| {
            Box::pin(async move {
                let mut q = sqlx::query(&compiled.sql);
                for v in &compiled.params {
                    q = row_filter::bind_json_param(q, v);
                }
                let rows = q.fetch_all(&mut **tx).await?;
                let mut out: Vec<Value> = Vec::with_capacity(rows.len());
                for r in rows {
                    let mut obj = r.get::<Value, _>("__row");
                    for inc in &include_names {
                        let alias = format!("__inc_{inc}");
                        if let Ok(v) = r.try_get::<Value, _>(alias.as_str()) {
                            if let Some(m) = obj.as_object_mut() {
                                m.insert(inc.clone(), v);
                            }
                        }
                    }
                    out.push(obj);
                }
                Ok(out)
            })
        })
        .await
        .expect("query");

    let mut rows = rows;
    let has_more = rows.len() as u32 > page_limit;
    let next_cursor = if has_more {
        rows.truncate(page_limit as usize);
        match (signer, rows.last()) {
            (Some(s), Some(last)) => Some(
                dsl::mint_cursor(s, &schema_key, &cursor_sort_sig, &cursor_sort_fields, last)
                    .unwrap(),
            ),
            _ => None,
        }
    } else {
        None
    };
    (rows, next_cursor)
}

#[tokio::test]
async fn nested_where_and_in_and_between() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(api) = api_url() else { return };

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
    let s = spec(vec![
        {
            let mut f = field("po_number", FieldKind::String, true, true);
            f.required = true;
            f
        },
        field("status", FieldKind::String, true, true),
        field("total", FieldKind::Number, true, true),
    ]);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();

    for (po, st, amt) in [
        ("PO-1", "draft", 100),
        ("PO-2", "approved", 500),
        ("PO-3", "approved", 1500),
        ("PO-4", "shipped", 200),
        ("PO-5", "approved", 2500),
    ] {
        insert_row(
            &api_pool,
            &schema,
            &identity,
            json!({ "po_number": po, "status": st, "total": amt }),
        )
        .await;
    }

    // status IN ("approved", "shipped") AND total BETWEEN 400 AND 2000
    let q = QueryDsl {
        where_node: Some(WhereNode::And {
            children: vec![
                WhereNode::Cmp {
                    field: "status".into(),
                    op: DslOp::In,
                    value: json!(["approved", "shipped"]),
                },
                WhereNode::Cmp {
                    field: "total".into(),
                    op: DslOp::Between,
                    value: json!([400, 2000]),
                },
            ],
        }),
        ..Default::default()
    };
    let (rows, _) = run_query(&api_pool, &schema, &identity, &registry, &q, None).await;
    let pos: Vec<String> =
        rows.iter().map(|r| r["po_number"].as_str().unwrap().to_string()).collect();
    assert!(pos.contains(&"PO-2".to_string()));
    assert!(pos.contains(&"PO-3".to_string()));
    assert!(!pos.contains(&"PO-1".to_string()));
    assert!(!pos.contains(&"PO-4".to_string()));
    assert!(!pos.contains(&"PO-5".to_string()));

    // NOT (status = "approved") OR status IS NULL → captures draft + shipped.
    let q = QueryDsl {
        where_node: Some(WhereNode::Or {
            children: vec![
                WhereNode::Not {
                    child: Box::new(WhereNode::Cmp {
                        field: "status".into(),
                        op: DslOp::Eq,
                        value: json!("approved"),
                    }),
                },
                WhereNode::Cmp { field: "status".into(), op: DslOp::IsNull, value: Value::Null },
            ],
        }),
        ..Default::default()
    };
    let (rows, _) = run_query(&api_pool, &schema, &identity, &registry, &q, None).await;
    let statuses: std::collections::HashSet<String> =
        rows.iter().map(|r| r["status"].as_str().unwrap().to_string()).collect();
    assert!(statuses.contains("draft"));
    assert!(statuses.contains("shipped"));
    assert!(!statuses.contains("approved"));

    cleanup(&admin_pool, &pg_schema).await;
}

#[tokio::test]
async fn cursor_paginates_across_pages() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(api) = api_url() else { return };

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
    let s = spec(vec![{
        let mut f = field("po_number", FieldKind::String, true, true);
        f.required = true;
        f
    }]);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();
    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();
    let cs = signer();

    // 12 rows; pages of 5.
    for i in 1..=12 {
        insert_row(&api_pool, &schema, &identity, json!({ "po_number": format!("PO-{i:02}") }))
            .await;
    }

    let q1 = QueryDsl {
        sort: vec![SortField { field: "po_number".into(), desc: false }],
        limit: Some(5),
        ..Default::default()
    };
    let (page1, cursor1) =
        run_query(&api_pool, &schema, &identity, &registry, &q1, Some(&cs)).await;
    assert_eq!(page1.len(), 5);
    assert!(cursor1.is_some(), "more pages exist; cursor must mint");

    let q2 = QueryDsl {
        sort: vec![SortField { field: "po_number".into(), desc: false }],
        limit: Some(5),
        cursor: cursor1.clone(),
        ..Default::default()
    };
    let (page2, cursor2) =
        run_query(&api_pool, &schema, &identity, &registry, &q2, Some(&cs)).await;
    assert_eq!(page2.len(), 5);
    assert!(cursor2.is_some());

    let q3 = QueryDsl {
        sort: vec![SortField { field: "po_number".into(), desc: false }],
        limit: Some(5),
        cursor: cursor2,
        ..Default::default()
    };
    let (page3, cursor3) =
        run_query(&api_pool, &schema, &identity, &registry, &q3, Some(&cs)).await;
    assert_eq!(page3.len(), 2, "trailing page");
    assert!(cursor3.is_none(), "last page must not mint a cursor");

    // No duplicates across pages, ordered ascending.
    let mut all: Vec<String> = Vec::new();
    for p in [&page1, &page2, &page3] {
        for r in p {
            all.push(r["po_number"].as_str().unwrap().to_string());
        }
    }
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted);
    let uniq: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(uniq.len(), 12);

    cleanup(&admin_pool, &pg_schema).await;
}

#[tokio::test]
async fn include_left_joins_target_schema() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(api) = api_url() else { return };

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

    // Target: supplier
    let supplier_path = SchemaPath::new(&org, app, domain, "supplier", "v1");
    let supplier_spec = spec(vec![{
        let mut f = field("name", FieldKind::String, true, true);
        f.required = true;
        f
    }]);
    let supplier_plan = velocity_operator::build_ddl(&supplier_spec, &supplier_path).unwrap();
    prov.sync_schema_tables(&supplier_plan, false).await.unwrap();
    let supplier_schema = ResolvedSchema::from_spec(supplier_path.clone(), supplier_spec);

    // Main: purchase-order with ref → supplier
    let po_path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let po_spec = spec(vec![
        {
            let mut f = field("po_number", FieldKind::String, true, true);
            f.required = true;
            f
        },
        ref_field("supplier_id", &supplier_path),
    ]);
    let po_plan = velocity_operator::build_ddl(&po_spec, &po_path).unwrap();
    prov.sync_schema_tables(&po_plan, false).await.unwrap();
    let po_schema = ResolvedSchema::from_spec(po_path.clone(), po_spec);

    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();
    registry.replace_all(vec![supplier_schema.clone(), po_schema.clone()]);

    // Insert a supplier and a PO referencing it.
    let supp =
        insert_row(&api_pool, &supplier_schema, &identity, json!({ "name": "Tata Steel" })).await;
    let supp_id = supp["id"].as_str().unwrap().to_string();
    insert_row(
        &api_pool,
        &po_schema,
        &identity,
        json!({ "po_number": "PO-001", "supplier_id": supp_id }),
    )
    .await;

    let q = QueryDsl { include: vec!["supplier_id".into()], ..Default::default() };
    let (rows, _) = run_query(&api_pool, &po_schema, &identity, &registry, &q, None).await;
    assert_eq!(rows.len(), 1);
    let inc = &rows[0]["supplier_id"];
    assert_eq!(inc["name"], "Tata Steel");

    cleanup(&admin_pool, &pg_schema).await;
}

#[tokio::test]
async fn cursor_tamper_rejected() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(api) = api_url() else { return };

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
    let s = spec(vec![{
        let mut f = field("po_number", FieldKind::String, true, true);
        f.required = true;
        f
    }]);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();
    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();
    let (registry, _rx) = SchemaRegistry::new();

    let real = CursorSigner::new(b"phase5a-test-cursor-key-32-bytes!!!".to_vec()).unwrap();
    let other = CursorSigner::new(b"different-key-also-32-bytes-long.....".to_vec()).unwrap();

    for i in 1..=6 {
        insert_row(&api_pool, &schema, &identity, json!({ "po_number": format!("PO-{i:02}") }))
            .await;
    }

    let q = QueryDsl {
        sort: vec![SortField { field: "po_number".into(), desc: false }],
        limit: Some(3),
        ..Default::default()
    };
    let (_page, cursor) =
        run_query(&api_pool, &schema, &identity, &registry, &q, Some(&real)).await;
    let cursor = cursor.unwrap();

    // Use a cursor minted with `real` against the API configured with `other` —
    // signature must fail.
    let q2 = QueryDsl {
        sort: vec![SortField { field: "po_number".into(), desc: false }],
        limit: Some(3),
        cursor: Some(cursor),
        ..Default::default()
    };
    let err = dsl::build(&schema, &q2, &identity, &registry, Some(&other)).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("signature") || msg.contains("BadRequest"));

    let _ = Arc::new(()); // silence unused-arc lint
    cleanup(&admin_pool, &pg_schema).await;
}
