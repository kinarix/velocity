//! Integration tests for `PostgresProvisioner::sync_schema_tables`.
//!
//! Skipped unless `VELOCITY_OPERATOR_PG_URL` is set. From repo root:
//!
//! ```sh
//! make up-pg db-bootstrap migrate
//! VELOCITY_OPERATOR_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!   cargo test -p velocity-operator --test schema_provisioner_integration -- --nocapture
//! ```

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use sqlx::postgres::PgPoolOptions;
use velocity_operator::{build_ddl, PostgresProvisioner, ProvisionError};
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_OPERATOR_PG_URL").ok()
}

fn auth() -> AuthSpec {
    AuthSpec {
        strategy_ref: velocity_types::common::NamespacedRef {
            name: "default".into(),
            namespace: "acme-platform".into(),
        },
        overrides: Vec::new(),
    }
}

fn field(name: &str, kind: FieldKind) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        kind,
        required: false,
        unique: false,
        indexed: false,
        filterable: false,
        sortable: false,
        searchable: false,
        default: None,
        min: None,
        max: None,
        max_length: None,
        pattern: None,
        enum_values: Vec::new(),
        r#ref: None,
        sensitivity: None,
        access: None,
    }
}

fn spec(fields: Vec<FieldSpec>, tier: SearchTier) -> SchemaDefinitionSpec {
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: auth(),
        access: AccessSpec::default(),
        fields,
        validations: Vec::new(),
        search: SearchSpec { tier, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

async fn cleanup(pool: &sqlx::PgPool, schema: &str) {
    let stmt = format!("DROP SCHEMA IF EXISTS {schema} CASCADE");
    let _ = sqlx::query(&stmt).execute(pool).await;
    for suffix in ["_reader", "_writer", "_admin"] {
        let role = format!("{schema}{suffix}");
        let drop_sql = format!("DROP ROLE IF EXISTS {role}");
        let _ = sqlx::query(&drop_sql).execute(pool).await;
    }
}

#[tokio::test]
async fn sync_schema_tables_creates_and_is_idempotent() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");

    cleanup(&pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(pool.clone());

    // Parent Domain must exist first — otherwise sync_schema_tables refuses.
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s = spec(
        vec![
            {
                let mut f = field("po_number", FieldKind::String);
                f.required = true;
                f.unique = true;
                f.max_length = Some(32);
                f
            },
            {
                let mut f = field("supplier_code", FieldKind::String);
                f.required = true;
                f.filterable = true;
                f
            },
        ],
        SearchTier::Tier3, // exercise the outbox path
    );
    let plan = build_ddl(&s, &path).unwrap();

    // First run — create.
    let p1 = prov.sync_schema_tables(&plan, false).await.unwrap();
    assert_eq!(p1.pg_schema, pg_schema);
    assert_eq!(p1.pg_table, "purchase_order_v1");
    assert_eq!(p1.qualified, format!("{pg_schema}.purchase_order_v1"));

    // Main, history, and outbox tables exist.
    for t in ["purchase_order_v1", "purchase_order_v1_history", "purchase_order_v1_outbox"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2)",
        )
        .bind(&pg_schema)
        .bind(t)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists, "{pg_schema}.{t} should exist");
    }

    // Second run — diff path produces no operations; must not error.
    let p2 = prov.sync_schema_tables(&plan, false).await.unwrap();
    assert_eq!(p1.qualified, p2.qualified);

    cleanup(&pool, &pg_schema).await;
}

#[tokio::test]
async fn sync_schema_tables_applies_safe_add_column() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s1 = spec(
        vec![{
            let mut f = field("po_number", FieldKind::String);
            f.required = true;
            f
        }],
        SearchTier::Tier1,
    );
    let plan1 = build_ddl(&s1, &path).unwrap();
    prov.sync_schema_tables(&plan1, false).await.unwrap();

    // Add a new nullable column → safe migration.
    let s2 = spec(
        vec![
            {
                let mut f = field("po_number", FieldKind::String);
                f.required = true;
                f
            },
            field("notes", FieldKind::String),
        ],
        SearchTier::Tier1,
    );
    let plan2 = build_ddl(&s2, &path).unwrap();
    prov.sync_schema_tables(&plan2, false).await.unwrap();

    // Verify the new column landed.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'purchase_order_v1' AND column_name = 'notes')",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(exists, "notes column should have been added");

    cleanup(&pool, &pg_schema).await;
}

#[tokio::test]
async fn sync_schema_tables_blocks_breaking_drop_column_without_annotation() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let app = "supply-chain";
    let domain = "procurement";
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(pool.clone());
    prov.sync_domain(&org, app, domain).await.unwrap();

    let path = SchemaPath::new(&org, app, domain, "purchase-order", "v1");
    let s1 = spec(
        vec![
            {
                let mut f = field("po_number", FieldKind::String);
                f.required = true;
                f
            },
            field("notes", FieldKind::String),
        ],
        SearchTier::Tier1,
    );
    let plan1 = build_ddl(&s1, &path).unwrap();
    prov.sync_schema_tables(&plan1, false).await.unwrap();

    // Drop the `notes` column → breaking.
    let s2 = spec(
        vec![{
            let mut f = field("po_number", FieldKind::String);
            f.required = true;
            f
        }],
        SearchTier::Tier1,
    );
    let plan2 = build_ddl(&s2, &path).unwrap();

    let err = prov.sync_schema_tables(&plan2, false).await;
    assert!(
        matches!(err, Err(ProvisionError::BreakingChange(_))),
        "drop column without approval must be rejected, got: {err:?}"
    );

    // Approval lifts the per-op safety gate, but DROP COLUMN has no executor
    // yet — we refuse with `BreakingChangeDeferred` rather than silently
    // succeeding while leaving the column in place. This is intentional:
    // "approved + no-op" is the worst possible UX for destructive intent.
    let err = prov.sync_schema_tables(&plan2, true).await;
    assert!(
        matches!(err, Err(ProvisionError::BreakingChangeDeferred(_))),
        "approved drop column must be deferred (not silently no-op), got: {err:?}"
    );
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'purchase_order_v1' AND column_name = 'notes')",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(exists, "deferred drop leaves the column untouched");

    cleanup(&pool, &pg_schema).await;
}

#[tokio::test]
async fn sync_schema_tables_refuses_when_domain_missing() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: VELOCITY_OPERATOR_PG_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let prov = PostgresProvisioner::new(pool);

    // Note: org has no matching schema — sync_domain was never called.
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("missingorg{suffix}");
    let path = SchemaPath::new(&org, "supply-chain", "procurement", "purchase-order", "v1");
    let s = spec(
        vec![{
            let mut f = field("po_number", FieldKind::String);
            f.required = true;
            f
        }],
        SearchTier::Tier1,
    );
    let plan = build_ddl(&s, &path).unwrap();

    let err = prov.sync_schema_tables(&plan, false).await;
    assert!(
        matches!(err, Err(ProvisionError::DomainNotProvisioned(_))),
        "must refuse when parent Domain schema is missing, got: {err:?}"
    );
}
