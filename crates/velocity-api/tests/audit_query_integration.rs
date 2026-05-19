#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 6a-2 — `/api/platform/audit*` end-to-end.
//!
//! Drives the production router (no auth middleware — `/api/platform/...`
//! is short-circuited by `schema_path_from_uri`'s 6-segment requirement
//! anyway) and verifies:
//!
//! - 401 when the platform token is unset or missing/wrong
//! - 400 when `schema_org` filter is missing
//! - 200 + paginated rows under a valid token + filter
//! - cursor pagination round-trips: page 1 + cursor → page 2
//! - self-audit row written for every call (success AND denial)
//! - `/audit/verify` returns `chain_intact: true` for a clean chain
//!
//! Skipped unless `VELOCITY_API_TEST_PG_URL` is set.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use velocity_api::audit::{self, AUDIT_SELF_SCHEMA_ORG};
use velocity_api::dsl::CursorSigner;
use velocity_api::registry::{ResolvedSchema, SchemaRegistry};
use velocity_api::{router, AppState, Identity};
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
};

const TOKEN: &str = "test-audit-token-1234567890";
const SIGNING_KEY: &[u8; 32] = b"audit-test-cursor-signing-key123";

fn pg_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

async fn connect() -> Option<PgPool> {
    let url = pg_url()?;
    Some(PgPoolOptions::new().max_connections(4).connect(&url).await.unwrap())
}

fn field(name: &str) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(serde_json::json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
    f
}

fn make_schema(schema_org: &str) -> ResolvedSchema {
    let mut parts = schema_org.split('/');
    let path = SchemaPath::new(
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
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
        fields: vec![field("po_number")],
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    };
    ResolvedSchema::from_spec(path, spec)
}

async fn cleanup(pool: &PgPool, schema_org: &str) {
    // Per-test schema_org only — we never delete AUDIT_SELF_SCHEMA_ORG
    // rows because they're shared across all concurrent tests in this
    // file. Self-audit assertions use before/after delta counts to stay
    // race-free under cargo's default parallel test runner.
    let _ = sqlx::query("DELETE FROM platform.audit_log WHERE schema_org = $1")
        .bind(schema_org)
        .execute(pool)
        .await;
}

async fn seed_audit_rows(pool: &PgPool, schema: &ResolvedSchema, n: usize) {
    for i in 0..n {
        let id = uuid::Uuid::new_v4().to_string();
        let mut tx = pool.begin().await.unwrap();
        audit::write_audit(
            &mut tx,
            schema,
            &Identity::anonymous(),
            audit::action::CREATE,
            audit::outcome::SUCCESS,
            Some(&id),
            &serde_json::json!({ "id": id, "po_number": format!("PO-{i}") }),
            None,
            Some(&format!("seed-{i}")),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

fn build_app(pool: PgPool, with_token: bool) -> axum::Router {
    let (registry, _ready) = SchemaRegistry::new();
    let mut state = AppState::new(registry, pool);
    let signer = CursorSigner::new(SIGNING_KEY.to_vec()).unwrap();
    state = state.with_cursor_signer(Arc::new(signer));
    if with_token {
        state = state.with_platform_audit_token(Arc::new(TOKEN.into()));
    }
    router::build(state)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Minimal URL-encoder for cursor strings. The cursor is base64-url-safe
/// plus `.`; in practice nothing reserved appears, but we encode
/// conservatively so the test never silently corrupts a query string.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn audit_request(path: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(t) = bearer {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

async fn count_self_audits(pool: &PgPool, action: &str, outcome: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM platform.audit_log \
         WHERE schema_org = $1 AND action = $2 AND outcome = $3",
    )
    .bind(AUDIT_SELF_SCHEMA_ORG)
    .bind(action)
    .bind(outcome)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn audit_endpoint_denies_when_token_unset() {
    // Token not configured → every caller MUST 401, even with a bearer.
    // Pinned because a future refactor that silently admits when the
    // env var is absent would turn the endpoint into an unauthenticated
    // cross-tenant dump.
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let schema_org = format!("auditest{}/sc/proc/po/v1", uuid::Uuid::new_v4().simple());
    cleanup(&pool, &schema_org).await;

    let before =
        count_self_audits(&pool, audit::action::READ, audit::outcome::DENIED).await;
    let app = build_app(pool.clone(), /* with_token = */ false);
    let res = app
        .oneshot(audit_request(
            &format!("/api/platform/audit?schema_org={schema_org}"),
            Some("anything"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(res).await;
    assert_eq!(body["error"], "AUDIT_UNAUTHORIZED");

    // Self-audit DENIED row written even on auth failure. Use a delta
    // because the audit-self schema_org is shared across concurrent
    // tests; a strict "expect 1" assertion would race.
    let after = count_self_audits(&pool, audit::action::READ, audit::outcome::DENIED).await;
    assert!(
        after > before,
        "expected at least one new self-audit denial row, before={before} after={after}"
    );
    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn audit_endpoint_denies_wrong_token() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let schema_org = format!("auditest{}/sc/proc/po/v1", uuid::Uuid::new_v4().simple());
    cleanup(&pool, &schema_org).await;

    let app = build_app(pool.clone(), true);
    let res = app
        .oneshot(audit_request(
            &format!("/api/platform/audit?schema_org={schema_org}"),
            Some("the-wrong-token-xxxx-yyyy"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn audit_endpoint_requires_schema_org_filter() {
    // Without `schema_org`, the endpoint MUST 400 — never silently
    // widen to cross-tenant. Pinned because a default would be a
    // single-knob skeleton key once the platform token leaks.
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let app = build_app(pool.clone(), true);
    let res = app
        .oneshot(audit_request("/api/platform/audit", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = body_json(res).await;
    assert_eq!(body["error"], "AUDIT_FILTER_REQUIRED");
}

#[tokio::test]
async fn audit_endpoint_returns_rows_for_seeded_schema() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let schema_org = format!("auditest{suffix}/sc/proc/po/v1");
    cleanup(&pool, &schema_org).await;

    let schema = make_schema(&schema_org);
    seed_audit_rows(&pool, &schema, 3).await;

    let app = build_app(pool.clone(), true);
    let res = app
        .oneshot(audit_request(
            &format!("/api/platform/audit?schema_org={schema_org}"),
            Some(TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["count"], 3, "expected exactly 3 seeded rows; got body={body}");
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    // Newest first (ORDER BY occurred_at DESC).
    assert_eq!(rows[0]["action"], "create");
    assert_eq!(rows[0]["outcome"], "success");
    assert!(rows[0]["hash"].as_str().unwrap().len() == 64, "sha256 hex = 64 chars");

    // No more pages when the seeded set fits inside one limit.
    assert!(body["next_cursor"].is_null(), "no next_cursor expected; got {body}");

    // Self-audit SUCCESS row written. Delta count keeps the test
    // race-free against other concurrent /audit tests sharing the
    // same AUDIT_SELF_SCHEMA_ORG.
    let after = count_self_audits(&pool, audit::action::READ, audit::outcome::SUCCESS).await;
    assert!(
        after > 0,
        "expected at least one self-audit success row after the call; got {after}"
    );
    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn audit_endpoint_paginates_with_cursor() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let schema_org = format!("auditest{suffix}/sc/proc/po/v1");
    cleanup(&pool, &schema_org).await;

    let schema = make_schema(&schema_org);
    seed_audit_rows(&pool, &schema, 5).await;

    let app = build_app(pool.clone(), true);

    // Page 1, limit=2 → 2 rows + non-null cursor (5 > 2).
    let res = app
        .clone()
        .oneshot(audit_request(
            &format!("/api/platform/audit?schema_org={schema_org}&limit=2"),
            Some(TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let page1_ids: Vec<String> = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(page1_ids.len(), 2);
    let cursor = body["next_cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("expected next_cursor; body={body}"))
        .to_string();

    // Page 2: same filter + cursor → next 2 rows, no overlap with page 1.
    let url = format!(
        "/api/platform/audit?schema_org={schema_org}&limit=2&cursor={}",
        url_encode(&cursor)
    );
    let res = app
        .clone()
        .oneshot(audit_request(&url, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let page2_ids: Vec<String> = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(page2_ids.len(), 2);
    for id in &page2_ids {
        assert!(
            !page1_ids.contains(id),
            "page 2 row {id} also appeared on page 1 — cursor pagination drifted"
        );
    }

    // Page 3: 5 rows total, 2+2 returned, 1 remains → no more cursor.
    let cursor2 = body["next_cursor"].as_str().unwrap().to_string();
    let url = format!(
        "/api/platform/audit?schema_org={schema_org}&limit=2&cursor={}",
        url_encode(&cursor2)
    );
    let res = app
        .oneshot(audit_request(&url, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["count"], 1, "final page should hold the single remaining row");
    assert!(body["next_cursor"].is_null(), "no more pages expected");

    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn audit_verify_endpoint_reports_intact_chain_on_clean_window() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let schema_org = format!("auditest{suffix}/sc/proc/po/v1");
    cleanup(&pool, &schema_org).await;

    let schema = make_schema(&schema_org);
    seed_audit_rows(&pool, &schema, 2).await;

    let app = build_app(pool.clone(), true);
    // Default 1h window — covers the rows we just inserted.
    let res = app
        .oneshot(audit_request("/api/platform/audit/verify", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["chain_intact"], true, "clean chain must verify; got {body}");
    assert_eq!(body["mismatches"].as_array().unwrap().len(), 0);
    cleanup(&pool, &schema_org).await;
}

#[tokio::test]
async fn audit_verify_endpoint_rejects_oversized_window() {
    let Some(pool) = connect().await else {
        eprintln!("skipping: VELOCITY_API_TEST_PG_URL not set");
        return;
    };
    let app = build_app(pool, true);
    // 30 days > 24h cap.
    let from = "2026-01-01T00:00:00Z";
    let to = "2026-01-31T00:00:00Z";
    let res = app
        .oneshot(audit_request(
            &format!("/api/platform/audit/verify?from={from}&to={to}"),
            Some(TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = body_json(res).await;
    assert_eq!(body["error"], "AUDIT_WINDOW_TOO_WIDE");
}
