#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Integration test for sensitive-field redaction on the audit write path.
//!
//! The unit tests in `audit.rs` pin `redact_sensitive` in isolation.
//! This one drives `write_audit` end-to-end against the real
//! `platform.audit_insert` SP and SELECTs the row back, so a refactor
//! that bypasses the helper (e.g. binds the raw payload directly) gets
//! caught instead of silently leaking PII into `platform.audit_log`.
//!
//! Skipped unless `VELOCITY_API_TEST_PG_URL` (or
//! `VELOCITY_OPERATOR_PG_URL`) is set — same env contract as
//! `denial_audit_integration.rs`.

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use velocity_api::audit::{action, outcome, write_audit, REDACTED};
use velocity_api::identity::Identity;
use velocity_api::registry::ResolvedSchema;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier, Sensitivity,
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

fn field(name: &str, sensitivity: Option<Sensitivity>) -> FieldSpec {
    let mut f: FieldSpec = serde_json::from_value(json!({
        "name": name,
        "type": "string",
    }))
    .unwrap();
    f.kind = FieldKind::String;
    f.sensitivity = sensitivity;
    f
}

fn schema_with_sensitive_fields() -> (SchemaPath, ResolvedSchema, String) {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("redacttest{suffix}");
    let path = SchemaPath::new(&org, "supply-chain", "procurement", "purchase-order", "v1");
    let spec = SchemaDefinitionSpec {
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
        fields: vec![
            field("po_number", None),
            field("supplier_ssn", Some(Sensitivity::Pii)),
            field("invoice_amount", Some(Sensitivity::Financial)),
            field("notes", Some(Sensitivity::Internal)),
        ],
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    };
    let schema = ResolvedSchema::from_spec(path.clone(), spec);
    let schema_org = format!(
        "{}/{}/{}/{}/{}",
        path.org, path.app, path.domain, path.object, path.version
    );
    (path, schema, schema_org)
}

async fn cleanup(pool: &PgPool, schema_org: &str) {
    let _ = sqlx::query("DELETE FROM platform.audit_log WHERE schema_org = $1")
        .bind(schema_org)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn write_audit_redacts_pii_and_financial_in_stored_payload() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let (_path, schema, schema_org) = schema_with_sensitive_fields();
    cleanup(&pool, &schema_org).await;

    let id = uuid::Uuid::new_v4().to_string();
    let raw_payload = json!({
        "id": id,
        "po_number": "PO-12345",
        "supplier_ssn": "123-45-6789",
        "invoice_amount": "150000.00",
        "notes": "internal commentary",
    });

    let mut tx = pool.begin().await.unwrap();
    write_audit(
        &mut tx,
        &schema,
        &Identity::anonymous(),
        action::CREATE,
        outcome::SUCCESS,
        Some(&id),
        &raw_payload,
        None,
        Some("req-redact-test"),
    )
    .await
    .expect("audit write should succeed");
    tx.commit().await.unwrap();

    let stored: Value = sqlx::query_scalar(
        "SELECT payload FROM platform.audit_log \
         WHERE schema_org = $1 ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(&schema_org)
    .fetch_one(&pool)
    .await
    .expect("audit row visible");

    // Keys preserved: SOC analysts still see *which* fields were
    // written, even when they can't see the values.
    assert_eq!(stored["id"], id, "non-sensitive id field passes through");
    assert_eq!(stored["po_number"], "PO-12345", "public field passes through");
    assert_eq!(stored["notes"], "internal commentary", "internal-class field passes through");

    // Sensitive values must be scrubbed. The literal "***" is what
    // Grafana / SIEM rules pattern-match on — pin it so a renaming
    // refactor breaks loudly.
    assert_eq!(stored["supplier_ssn"], REDACTED, "PII value redacted");
    assert_eq!(stored["invoice_amount"], REDACTED, "financial value redacted");

    // Belt-and-suspenders: scan the JSON serialisation for the raw
    // values so a future bug that, say, double-encodes the payload
    // (and bypasses our key-level check) still trips this test.
    let serialised = serde_json::to_string(&stored).unwrap();
    assert!(
        !serialised.contains("123-45-6789"),
        "raw SSN must not appear anywhere in the stored payload; got: {serialised}"
    );
    assert!(
        !serialised.contains("150000.00"),
        "raw invoice amount must not appear anywhere; got: {serialised}"
    );

    cleanup(&pool, &schema_org).await;
}
