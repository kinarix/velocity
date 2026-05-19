#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 6a-3 — `__fields_changed` lands in the audit row.
//!
//! Drives the production router for CREATE then UPDATE and asserts:
//!
//! - CREATE row's audit `payload->'__fields_changed'` is the
//!   sorted list of TOP-LEVEL fields the caller submitted (id /
//!   timestamps / version filtered out).
//! - UPDATE row's audit `payload->'__fields_changed'` is exactly
//!   the names whose value differed between before-image and
//!   after-image — *not* re-submitted unchanged fields.
//!
//! Skipped unless `VELOCITY_API_TEST_PG_URL` is set (admin / superuser
//! URL — schema provisioning needs CREATE on the database).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use velocity_api::registry::{ResolvedSchema, SchemaRegistry};
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

fn field(name: &str) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
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
        access: AccessSpec::default(),
        fields: vec![field("po_number"), field("supplier")],
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

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn fetch_audit_payload(
    admin: &PgPool,
    schema_org: &str,
    action: &str,
) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT payload FROM platform.audit_log \
         WHERE schema_org = $1 AND action = $2 \
         ORDER BY occurred_at",
    )
    .bind(schema_org)
    .bind(action)
    .fetch_all(admin)
    .await
    .expect("audit_log readable")
}

#[tokio::test]
async fn fields_changed_recorded_for_create_and_update() {
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
    let org = format!("fldchg{suffix}");
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
    let schema_org = format!(
        "{}/{}/{}/{}/{}",
        path.org, path.app, path.domain, path.object, path.version
    );

    let (registry, _ready) = SchemaRegistry::new();
    registry.upsert(schema.clone());
    let app_state = AppState::new(registry, api_pool.clone());
    let app = router::build(app_state);

    // ── CREATE ────────────────────────────────────────────────────────────
    let create_body = json!({ "po_number": "PO-0001", "supplier": "ACME" });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/{}/{}/{}/{}/{}",
                    path.org, path.app, path.domain, path.object, path.version
                ))
                .header("content-type", "application/json")
                .body(Body::from(create_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    let id = body["id"].as_str().unwrap().to_string();

    let create_audits = fetch_audit_payload(&admin_pool, &schema_org, "create").await;
    assert_eq!(create_audits.len(), 1);
    let create_fields = create_audits[0]["__fields_changed"].as_array().unwrap();
    let create_fields: Vec<&str> = create_fields.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(
        create_fields,
        vec!["po_number", "supplier"],
        "CREATE must record exactly the user-submitted fields, sorted; server-managed columns excluded"
    );

    // ── UPDATE: change only `supplier`, keep `po_number` the same ────────
    let update_body = json!({
        "po_number": "PO-0001",        // unchanged
        "supplier": "GLOBEX",          // changed
        "version": 1,
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!(
                    "/api/{}/{}/{}/{}/{}/{}",
                    path.org, path.app, path.domain, path.object, path.version, id
                ))
                .header("content-type", "application/json")
                .body(Body::from(update_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let update_audits = fetch_audit_payload(&admin_pool, &schema_org, "update").await;
    assert_eq!(update_audits.len(), 1);
    let upd_changed = update_audits[0]["__fields_changed"].as_array().unwrap();
    let upd_changed: Vec<&str> = upd_changed.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        upd_changed.contains(&"supplier"),
        "UPDATE must record `supplier` as changed; got {upd_changed:?}"
    );
    assert!(
        upd_changed.contains(&"updated_at") || upd_changed.contains(&"updated_by") || upd_changed.contains(&"version"),
        "UPDATE produces a fresh updated_at/version on every successful write — at least one server-managed timestamp must appear; got {upd_changed:?}"
    );
    assert!(
        !upd_changed.contains(&"po_number"),
        "po_number was re-submitted with the same value — MUST NOT appear in the changed-list; got {upd_changed:?}"
    );

    cleanup(&admin_pool, &pg_schema).await;
    drop(api_pool);
}
