#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Integration test for `audit::write_audit_denial`.
//!
//! Real Postgres, no HTTP layer. Verifies:
//!   - The `platform.audit_insert` SP accepts NULL `p_entity_id` (the
//!     pre-condition we asserted from the migration; this is the
//!     runtime test that proves it).
//!   - The row lands with `outcome = 'denied'` and the error code in
//!     the `payload->>'code'` field — what dashboards will pivot on.
//!   - The fail-mode JSON carries the `AuthDecision` we passed in.
//!
//! Skipped unless `VELOCITY_API_TEST_PG_URL` (or
//! `VELOCITY_OPERATOR_PG_URL`) is set:
//!
//! ```sh
//! make up-pg db-bootstrap migrate
//! VELOCITY_API_TEST_PG_URL=postgres://postgres:postgres@localhost:5434/velocity \
//!   cargo test -p velocity-api --test denial_audit_integration
//! ```

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use velocity_api::audit::{action, outcome, write_audit_denial};
use velocity_api::auth::{AuthDecision, RevocationDecision};
use velocity_api::identity::Identity;
use velocity_api::registry::ResolvedSchema;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec, SearchTier,
};

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

async fn connect() -> Option<PgPool> {
    let url = pg_url()?;
    Some(PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap())
}

fn empty_spec() -> SchemaDefinitionSpec {
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
        fields: Vec::new(),
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

/// Drop any audit rows we wrote so re-runs don't accumulate. Scoped by
/// the unique schema_org we generate per-test.
async fn cleanup(pool: &PgPool, schema_org: &str) {
    let _ = sqlx::query("DELETE FROM platform.audit_log WHERE schema_org = $1")
        .bind(schema_org)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn write_audit_denial_inserts_row_with_null_entity_and_code_payload() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("denialtest{suffix}");
    let path = SchemaPath::new(&org, "supply-chain", "procurement", "purchase-order", "v1");
    let schema = ResolvedSchema::from_spec(path.clone(), empty_spec());
    let schema_org =
        format!("{}/{}/{}/{}/{}", path.org, path.app, path.domain, path.object, path.version);

    cleanup(&pool, &schema_org).await;

    let identity = Identity::anonymous();
    let decision = AuthDecision {
        revocation: RevocationDecision::Allowed,
        revocation_fail_open: false,
        strategy: "acme-platform/default".into(),
    };

    write_audit_denial(
        &pool,
        &schema,
        &identity,
        action::CREATE,
        "ACCESS_DENIED",
        Some(&decision),
        Some("req-denial-test"),
    )
    .await
    .expect("denial audit row should write");

    // Verify a single denial row exists with the expected shape.
    let row: (String, Option<String>, serde_json::Value, serde_json::Value, Option<String>) =
        sqlx::query_as(
            "SELECT outcome, entity_id::text, payload, fail_modes, request_id \
             FROM platform.audit_log \
             WHERE schema_org = $1 \
             ORDER BY occurred_at DESC LIMIT 1",
        )
        .bind(&schema_org)
        .fetch_one(&pool)
        .await
        .expect("denial row visible");

    assert_eq!(row.0, outcome::DENIED, "outcome must be denied");
    assert!(row.1.is_none(), "entity_id must be NULL for denial");
    assert_eq!(row.2, json!({ "code": "ACCESS_DENIED" }), "payload carries error code");
    assert_eq!(row.3["auth"], "verified", "fail_modes carries auth state");
    assert_eq!(row.3["revocation"], "allowed");
    assert_eq!(row.3["strategy"], "acme-platform/default");
    assert_eq!(row.4.as_deref(), Some("req-denial-test"), "request_id propagated");

    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn write_audit_denial_with_unwired_auth_is_distinguishable() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("denialunwired{suffix}");
    let path = SchemaPath::new(&org, "supply-chain", "procurement", "purchase-order", "v1");
    let schema = ResolvedSchema::from_spec(path.clone(), empty_spec());
    let schema_org =
        format!("{}/{}/{}/{}/{}", path.org, path.app, path.domain, path.object, path.version);

    cleanup(&pool, &schema_org).await;

    write_audit_denial(
        &pool,
        &schema,
        &Identity::anonymous(),
        action::UPDATE,
        "FIELD_WRITE_DENIED",
        None, // no AuthDecision — exercise the "unwired" path
        None,
    )
    .await
    .expect("denial audit row with no auth decision should still write");

    let fail_modes: serde_json::Value = sqlx::query_scalar(
        "SELECT fail_modes FROM platform.audit_log \
         WHERE schema_org = $1 ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(&schema_org)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(fail_modes["auth"], "unwired", "no-decision case records 'unwired'");

    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn write_audit_denial_chains_with_existing_audit_rows() {
    // The denial path shares the same SP as success-path audit, so it
    // participates in the chain. Two consecutive denial writes must
    // produce two distinct hashes (the second hash includes the first).
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };

    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("denialchain{suffix}");
    let path = SchemaPath::new(&org, "supply-chain", "procurement", "purchase-order", "v1");
    let schema = ResolvedSchema::from_spec(path.clone(), empty_spec());
    let schema_org =
        format!("{}/{}/{}/{}/{}", path.org, path.app, path.domain, path.object, path.version);

    cleanup(&pool, &schema_org).await;
    let identity = Identity::anonymous();

    write_audit_denial(&pool, &schema, &identity, action::CREATE, "ACCESS_DENIED", None, None)
        .await
        .unwrap();
    write_audit_denial(&pool, &schema, &identity, action::CREATE, "POLICY_DENIED", None, None)
        .await
        .unwrap();

    let hashes: Vec<String> = sqlx::query_scalar(
        "SELECT hash FROM platform.audit_log \
         WHERE schema_org = $1 ORDER BY occurred_at ASC",
    )
    .bind(&schema_org)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(hashes.len(), 2, "two denial rows expected");
    assert_ne!(hashes[0], hashes[1], "chain hashes must differ");

    cleanup(&pool, &schema_org).await;
}
