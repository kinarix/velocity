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
        fts_weight: None,
        default: None,
        min: None,
        max: None,
        max_length: None,
        pattern: None,
        enum_values: Vec::new(),
        r#ref: None,
        sensitivity: None,
        access: None,
        mask: None,
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
    // Domain roles hold grants on platform.* tables (event_log, audit_insert,
    // idempotency_keys) and may be granted to velocity_api / velocity_operator.
    // DROP ROLE fails if those grants are not revoked first, so REASSIGN
    // OWNED + DROP OWNED scrubs all dependencies before DROP ROLE. Without
    // this, pg_authid accumulates one role per integration test run and
    // eventually pushes pg_authid rows past the 8 KB limit.
    for suffix in ["_reader", "_writer", "_admin"] {
        let role = format!("{schema}{suffix}");
        let _ = sqlx::query(&format!("REASSIGN OWNED BY {role} TO postgres"))
            .execute(pool)
            .await;
        let _ = sqlx::query(&format!("DROP OWNED BY {role} CASCADE"))
            .execute(pool)
            .await;
        let _ = sqlx::query(&format!("DROP ROLE IF EXISTS {role}"))
            .execute(pool)
            .await;
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

/// Phase 5d — when the FTS expression changes (e.g. a weight is added
/// to a previously default-weight field), the operator must drop the
/// `__fts` column and re-add it with the new expression. The column
/// COMMENT carries the hash that drives the detection.
#[tokio::test]
async fn sync_schema_tables_rebuilds_fts_when_weight_changes() {
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

    // v1 — `title` searchable, no weight (defaults to D).
    let s1 = spec(
        vec![{
            let mut f = field("title", FieldKind::String);
            f.required = true;
            f.searchable = true;
            f
        }],
        SearchTier::Tier2,
    );
    let plan1 = build_ddl(&s1, &path).unwrap();
    prov.sync_schema_tables(&plan1, false).await.unwrap();

    // Confirm the expression Postgres actually persists carries 'D'.
    let live_expr_v1: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE n.nspname = $1 AND c.relname = 'purchase_order_v1' AND a.attname = '__fts'",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(live_expr_v1.contains("'D'"), "expected weight D in {live_expr_v1}");

    // The hash COMMENT exists.
    let comment_v1: Option<String> = sqlx::query_scalar(
        "SELECT pgd.description \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_description pgd ON pgd.objoid = a.attrelid AND pgd.objsubid = a.attnum \
         WHERE n.nspname = $1 AND c.relname = 'purchase_order_v1' AND a.attname = '__fts'",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    let comment_v1 = comment_v1.expect("__fts must carry a velocity hash comment");
    assert!(
        comment_v1.starts_with("velocity:fts_hash:"),
        "expected velocity hash prefix, got `{comment_v1}`"
    );

    // v2 — same field, but bumped to weight A.
    let s2 = spec(
        vec![{
            let mut f = field("title", FieldKind::String);
            f.required = true;
            f.searchable = true;
            f.fts_weight = Some(velocity_types::crds::schema::FtsWeight::A);
            f
        }],
        SearchTier::Tier2,
    );
    let plan2 = build_ddl(&s2, &path).unwrap();
    prov.sync_schema_tables(&plan2, false).await.unwrap();

    // Expression now carries 'A'.
    let live_expr_v2: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE n.nspname = $1 AND c.relname = 'purchase_order_v1' AND a.attname = '__fts'",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(live_expr_v2.contains("'A'"), "expected weight A in {live_expr_v2}");

    // Comment hash also changed.
    let comment_v2: String = sqlx::query_scalar(
        "SELECT pgd.description \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_description pgd ON pgd.objoid = a.attrelid AND pgd.objsubid = a.attnum \
         WHERE n.nspname = $1 AND c.relname = 'purchase_order_v1' AND a.attname = '__fts'",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(comment_v1, comment_v2, "hash comment must change with the expression");

    // The GIN index was dropped by CASCADE and recreated by the standard
    // index pass — verify it exists.
    let idx_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
         WHERE schemaname = $1 AND tablename = 'purchase_order_v1' \
           AND indexname = 'idx_purchase_order_v1_fts')",
    )
    .bind(&pg_schema)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(idx_exists, "GIN index must be re-created after CASCADE drop");

    // Third reconcile with the same v2 plan must be a no-op for FTS
    // (hash matches → no DROP/ADD).
    prov.sync_schema_tables(&plan2, false).await.unwrap();

    cleanup(&pool, &pg_schema).await;
}

/// Phase 5d — end-to-end proof that per-field weights actually
/// influence `ts_rank()`. Provisions a Tier-2 schema with `title` at
/// weight A and `body` at weight D, inserts a row whose match is in
/// title and a row whose match is in body, and asserts the title-match
/// row ranks higher.
#[tokio::test]
async fn fts_weighted_fields_produce_different_ts_rank() {
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

    let path = SchemaPath::new(&org, app, domain, "article", "v1");
    let s = spec(
        vec![
            {
                let mut f = field("title", FieldKind::String);
                f.required = true;
                f.searchable = true;
                f.fts_weight = Some(velocity_types::crds::schema::FtsWeight::A);
                f
            },
            {
                let mut f = field("body", FieldKind::String);
                f.searchable = true;
                f.fts_weight = Some(velocity_types::crds::schema::FtsWeight::D);
                f
            },
        ],
        SearchTier::Tier2,
    );
    let plan = build_ddl(&s, &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    // Insert directly — bypassing the API. Pin a single connection so
    // `app.current_user` (consumed by the DEFAULT on created_by /
    // updated_by) is visible to the INSERT. `set_config()` accepts
    // the dotted GUC name unquoted, unlike SET which trips on the
    // `current_user` reserved keyword inside the right-hand side.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SELECT set_config('app.current_user', 'fts-rank-test', false)")
        .execute(&mut *conn)
        .await
        .unwrap();
    let table = format!("{pg_schema}.article_v1");
    sqlx::query(&format!(
        "INSERT INTO {table} (title, body) VALUES \
         ('rust programming', 'a long article'), \
         ('cooking', 'a long article about rust programming')"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();

    let ranks: Vec<(String, f32)> = sqlx::query_as(&format!(
        "SELECT title, ts_rank(__fts, websearch_to_tsquery('english', 'rust programming')) AS rank \
         FROM {table} \
         ORDER BY rank DESC"
    ))
    .fetch_all(&mut *conn)
    .await
    .unwrap();

    assert_eq!(ranks.len(), 2);
    // Title-weighted match comes first; the body-only match comes second.
    assert_eq!(ranks[0].0, "rust programming");
    assert_eq!(ranks[1].0, "cooking");
    // And the gap is real — the title hit's ts_rank must strictly exceed
    // the body hit's.
    assert!(
        ranks[0].1 > ranks[1].1,
        "title-weighted hit must outrank body-weighted hit ({} <= {})",
        ranks[0].1,
        ranks[1].1
    );

    cleanup(&pool, &pg_schema).await;
}
