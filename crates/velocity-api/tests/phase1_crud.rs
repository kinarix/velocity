#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 1 CRUD round-trip — drives the handler functions directly against
//! a docker-compose Postgres, bypassing the HTTP layer. The HTTP wiring is
//! exercised by the unit + router tests; this verifies the end-to-end SQL
//! shape (INSERT/UPDATE/DELETE with the ADR-007 session prelude) is correct.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase1_crud
//!
//! The URL must point to a superuser role; the operator provisioner needs
//! CREATE ROLE / CREATE SCHEMA. The connection role for the actual handler
//! calls is `velocity_api` (also created by db/init/01-roles.sql).

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use velocity_api::handlers;
use velocity_api::identity::Identity;
use velocity_api::query::{build_list, ListQuery};
use velocity_api::registry::ResolvedSchema;
use velocity_api::session::{with_session_context, RoleClass};
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
    // Handler txns must run as `velocity_api` so RLS / role switching work.
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

fn unique_field(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f = field(name, kind, true, true);
    f.unique = true;
    f.required = true;
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

#[tokio::test]
async fn crud_round_trip() {
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

    // Unique org so concurrent test runs don't collide.
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    // 1. Provision the Postgres schema/table via the operator's provisioner.
    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = spec(vec![
        {
            let mut f = field("po_number", FieldKind::String, true, true);
            f.required = true;
            f
        },
        field("supplier_code", FieldKind::String, true, true),
        field("total", FieldKind::Number, true, true),
        field("status", FieldKind::String, true, true),
    ]);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    // 2. Build a ResolvedSchema (same shape the informer would feed the
    //    registry) and drive the handlers directly.
    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();

    // CREATE
    let created = handler_create(&api_pool, &schema, &identity, &json!({
        "po_number": "PO-0001",
        "supplier_code": "TATA001",
        "total": 1500,
        "status": "draft",
    }))
    .await
    .expect("create");
    let id = created["id"].as_str().expect("inserted id").to_string();
    assert_eq!(created["po_number"], "PO-0001");
    assert_eq!(created["version"], 1);

    // GET
    let got = handler_get(&api_pool, &schema, &identity, &id).await.unwrap();
    assert_eq!(got["po_number"], "PO-0001");

    // LIST
    let list_items = handler_list(&api_pool, &schema, &identity, &ListQuery::default())
        .await
        .unwrap();
    assert!(list_items.iter().any(|r| r["id"].as_str() == Some(&id)));

    // UPDATE — happy path
    let updated = handler_update(
        &api_pool,
        &schema,
        &identity,
        &id,
        &json!({ "status": "approved", "version": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(updated["status"], "approved");
    assert_eq!(updated["version"], 2);

    // UPDATE — stale version → conflict
    let stale = handler_update(
        &api_pool,
        &schema,
        &identity,
        &id,
        &json!({ "status": "rejected", "version": 1 }),
    )
    .await;
    assert!(matches!(stale, Err(velocity_api::ApiError::VersionConflict)));

    // DELETE — soft
    handler_delete(&api_pool, &schema, &identity, &id).await.unwrap();
    let missing = handler_get(&api_pool, &schema, &identity, &id).await;
    assert!(matches!(missing, Err(velocity_api::ApiError::NotFound)));

    cleanup(&admin_pool, &pg_schema).await;
}

/// Phase-1 acceptance: a soft-deleted row must not block re-creating a row with
/// the same unique value. DdlBuilder emits partial unique indexes
/// (`WHERE deleted_at IS NULL`) for `unique: true` fields — this proves the
/// constraint actually behaves that way end-to-end.
#[tokio::test]
async fn soft_delete_releases_unique_value() {
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
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = spec(vec![unique_field("po_number", FieldKind::String)]);
    let plan = velocity_operator::build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    let schema = ResolvedSchema::from_spec(path.clone(), s);
    let identity = Identity::anonymous();

    let first = handler_create(&api_pool, &schema, &identity, &json!({ "po_number": "PO-UNIQUE" }))
        .await
        .expect("first insert");
    let first_id = first["id"].as_str().unwrap().to_string();

    // Re-insert before delete → must fail on the partial unique index.
    let dup = handler_create(&api_pool, &schema, &identity, &json!({ "po_number": "PO-UNIQUE" }))
        .await;
    assert!(dup.is_err(), "second insert should violate the active unique index");

    // Soft delete.
    handler_delete(&api_pool, &schema, &identity, &first_id).await.unwrap();

    // Now re-creation with the same unique value must succeed.
    let revived =
        handler_create(&api_pool, &schema, &identity, &json!({ "po_number": "PO-UNIQUE" }))
            .await
            .expect("re-insert after soft delete");
    assert_eq!(revived["po_number"], "PO-UNIQUE");
    assert_ne!(revived["id"].as_str().unwrap(), first_id, "must be a new row");

    cleanup(&admin_pool, &pg_schema).await;
}

// ── Thin handler-shaped helpers that mirror the HTTP entry points but skip
// Axum extraction. They re-use the same `with_session_context` + SQL paths
// the real handlers use so the SQL contract is exercised end-to-end.

async fn handler_create(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    payload: &Value,
) -> Result<Value, velocity_api::ApiError> {
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
            Ok(sqlx::Row::get::<Value, _>(&row, "row"))
        })
    })
    .await?;
    Ok(row)
}

async fn handler_get(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    id: &str,
) -> Result<Value, velocity_api::ApiError> {
    let table = schema.pg_qualified.clone();
    let id = id.to_string();
    let row = with_session_context(pool, schema, RoleClass::Reader, identity, move |tx| {
        Box::pin(async move {
            let sql = format!(
                "SELECT row_to_json(t) AS row FROM {table} t \
                 WHERE id = $1::uuid AND deleted_at IS NULL"
            );
            sqlx::query(&sql).bind(&id).fetch_optional(&mut **tx).await
        })
    })
    .await?;
    row.map(|r| sqlx::Row::get::<Value, _>(&r, "row"))
        .ok_or(velocity_api::ApiError::NotFound)
}

async fn handler_list(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    q: &ListQuery,
) -> Result<Vec<Value>, velocity_api::ApiError> {
    let compiled = build_list(schema, q, identity)?;
    let rows = with_session_context(pool, schema, RoleClass::Reader, identity, move |tx| {
        Box::pin(async move {
            let sql = compiled
                .sql
                .replacen("SELECT * FROM", "SELECT row_to_json(t.*) AS row FROM", 1);
            let mut q = sqlx::query(&sql);
            for v in &compiled.params {
                q = q.bind(v);
            }
            q.fetch_all(&mut **tx).await
        })
    })
    .await?;
    Ok(rows.into_iter().map(|r| sqlx::Row::get::<Value, _>(&r, "row")).collect())
}

async fn handler_update(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    id: &str,
    payload: &Value,
) -> Result<Value, velocity_api::ApiError> {
    let obj = payload.as_object().unwrap();
    let expected_version: i32 = obj["version"].as_i64().unwrap() as i32;
    let mut sets = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for f in schema.fields.ordered.iter() {
        if f.name == "version" {
            continue;
        }
        if let Some(v) = obj.get(&f.name) {
            vals.push(v.clone());
            sets.push(format!(
                "{} = {}",
                f.name,
                handlers::cast_placeholder(vals.len(), f.kind)
            ));
        }
    }
    sets.push("updated_at = now()".into());
    sets.push("updated_by = current_setting('app.current_user', true)".into());
    sets.push("version = version + 1".into());
    let id_idx = vals.len() + 1;
    let ver_idx = vals.len() + 2;
    let table = schema.pg_qualified.clone();
    let sql = format!(
        "UPDATE {table} SET {} WHERE id = ${id_idx}::uuid AND version = ${ver_idx} AND deleted_at IS NULL \
         RETURNING row_to_json({table}.*) AS row",
        sets.join(", ")
    );

    let id_owned = id.to_string();
    let probe_table = table.clone();
    let id_probe = id.to_string();

    let outcome = with_session_context(pool, schema, RoleClass::Writer, identity, move |tx| {
        Box::pin(async move {
            let mut q = sqlx::query(&sql);
            for v in &vals {
                q = q.bind(v);
            }
            let result = q.bind(&id_owned).bind(expected_version).fetch_optional(&mut **tx).await?;
            if let Some(r) = result {
                return Ok(Some(sqlx::Row::get::<Value, _>(&r, "row")));
            }
            // probe: NotFound vs VersionConflict
            let probe = sqlx::query(&format!(
                "SELECT id FROM {probe_table} WHERE id = $1::uuid LIMIT 1"
            ))
            .bind(&id_probe)
            .fetch_optional(&mut **tx)
            .await?;
            if probe.is_some() {
                Err(sqlx::Error::Protocol("__version_conflict__".into()))
            } else {
                Err(sqlx::Error::RowNotFound)
            }
        })
    })
    .await;

    match outcome {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(velocity_api::ApiError::NotFound),
        Err(sqlx::Error::RowNotFound) => Err(velocity_api::ApiError::NotFound),
        Err(sqlx::Error::Protocol(msg)) if msg == "__version_conflict__" => {
            Err(velocity_api::ApiError::VersionConflict)
        }
        Err(e) => Err(velocity_api::ApiError::Database(e)),
    }
}

async fn handler_delete(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    id: &str,
) -> Result<(), velocity_api::ApiError> {
    let table = schema.pg_qualified.clone();
    let id = id.to_string();
    let result = with_session_context(pool, schema, RoleClass::Admin, identity, move |tx| {
        Box::pin(async move {
            let sql = format!(
                "UPDATE {table} SET deleted_at = now(), updated_at = now(), \
                 updated_by = current_setting('app.current_user', true) \
                 WHERE id = $1::uuid AND deleted_at IS NULL"
            );
            let r = sqlx::query(&sql).bind(&id).execute(&mut **tx).await?;
            if r.rows_affected() == 0 {
                return Err(sqlx::Error::RowNotFound);
            }
            Ok(())
        })
    })
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(sqlx::Error::RowNotFound) => Err(velocity_api::ApiError::NotFound),
        Err(e) => Err(velocity_api::ApiError::Database(e)),
    }
}
