#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 6a-1 — every read produces an audit row.
//!
//! Drives the production router (no auth middleware — handlers fall
//! back to `Identity::anonymous()`) for GET /{id} and GET /, then
//! SELECTs `platform.audit_log` and asserts:
//!
//! - `get_one` emits exactly one `action="read"` row with `entity_id`
//!   set to the row's UUID; `payload->>'id'` matches.
//! - `list`    emits exactly one `action="read"` row with NULL
//!   `entity_id` and `payload->>'count'` reflecting the result size.
//!
//! The unit + integration tests in `audit_redaction_integration.rs`
//! pin the redaction shape; this test pins the *coverage* side of the
//! acceptance criterion ("every request → audit entry") for reads.
//!
//! Skipped unless `VELOCITY_API_TEST_PG_URL` is set.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use velocity_api::audit;
use velocity_api::registry::{ResolvedSchema, SchemaRegistry};
use velocity_api::session::{with_session_context, RoleClass};
use velocity_api::{router, AppState};
use velocity_operator::PostgresProvisioner;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
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

fn field(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f.filterable = true;
    f.sortable = true;
    f
}

fn schema_spec() -> SchemaDefinitionSpec {
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
        // Open schema — no roles required. Auth middleware is not
        // installed in this test, so identity is anonymous and the
        // open-access fast path admits.
        access: AccessSpec::default(),
        fields: vec![field("po_number", FieldKind::String)],
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
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE"))
        .execute(admin)
        .await;
    for role in [
        format!("{pg_schema}_reader"),
        format!("{pg_schema}_writer"),
        format!("{pg_schema}_admin"),
    ] {
        let _ = sqlx::query(&format!("DROP ROLE IF EXISTS {role}")).execute(admin).await;
    }
}

/// `(audit row count, payload of latest row)`.
async fn read_audits(admin: &PgPool, schema_org: &str, action: &str) -> Vec<(Option<String>, Value)> {
    sqlx::query_as::<_, (Option<String>, Value)>(
        "SELECT entity_id::text, payload \
         FROM platform.audit_log \
         WHERE schema_org = $1 AND action = $2 \
         ORDER BY occurred_at",
    )
    .bind(schema_org)
    .bind(action)
    .fetch_all(admin)
    .await
    .expect("audit_log readable")
}

fn schema_org_of(path: &SchemaPath) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        path.org, path.app, path.domain, path.object, path.version
    )
}

#[tokio::test]
async fn get_one_and_list_each_emit_a_read_audit_row() {
    let Some(admin) = admin_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let Some(api) = api_url() else {
        eprintln!("skipping: VELOCITY_API_TEST_API_URL not set");
        return;
    };

    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("readaud{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = schema_spec();
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let schema_org = schema_org_of(&path);

    // Seed one row directly so we have something to GET. Going through
    // the create handler would itself emit a `create` audit row, which
    // would complicate the assertions below — we want to test the read
    // path in isolation.
    let identity = velocity_api::Identity::anonymous();
    let table = schema.pg_qualified.clone();
    let inserted_id: String = {
        let table = table.clone();
        with_session_context(
            &api_pool,
            &schema,
            RoleClass::Writer,
            &identity,
            move |tx| {
                Box::pin(async move {
                    let sql = format!(
                        "INSERT INTO {table} (po_number) VALUES ($1) \
                         RETURNING id::text AS id"
                    );
                    let row = sqlx::query(&sql).bind("PO-0001").fetch_one(&mut **tx).await?;
                    Ok(sqlx::Row::get::<String, _>(&row, "id"))
                })
            },
        )
        .await
        .unwrap()
    };

    // Build router + AppState. No auth layer — `identity_from_ext`
    // returns anonymous; the schema is `open`, so reads admit.
    let (registry, _ready) = SchemaRegistry::new();
    registry.upsert(schema.clone());
    let app_state = AppState::new(registry, api_pool.clone());
    let app = router::build(app_state);

    // GET /{id}
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/{}/{}/{}/{}/{}/{}",
                    path.org, path.app, path.domain, path.object, path.version, inserted_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "get_one must 200 for an existing row");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["po_number"], "PO-0001");

    // GET /
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/{}/{}/{}/{}/{}",
                    path.org, path.app, path.domain, path.object, path.version
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "list must 200");

    // Assert the audit rows.
    let rows = read_audits(&admin_pool, &schema_org, audit::action::READ).await;
    assert_eq!(rows.len(), 2, "exactly one read-audit per request (get_one + list)");

    // get_one (first by occurred_at) — entity_id matches, payload has id
    let (eid0, payload0) = &rows[0];
    assert_eq!(eid0.as_deref(), Some(inserted_id.as_str()), "get_one binds entity_id");
    assert_eq!(payload0["id"], inserted_id, "get_one payload echoes id");

    // list — entity_id NULL, payload carries count
    let (eid1, payload1) = &rows[1];
    assert!(eid1.is_none(), "list has no single entity, entity_id must be NULL");
    assert_eq!(payload1["count"], 1, "list count reflects seeded row");

    cleanup(&admin_pool, &pg_schema).await;
}
