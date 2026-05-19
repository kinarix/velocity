#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! HTTP-level wire-through for the Layer-4 row-filter gate.
//!
//! The compiler and predicate semantics are unit-tested in
//! `src/row_filter.rs`; the job here is to prove every handler that builds
//! a WHERE clause (LIST / GET / UPDATE / DELETE) actually consults the
//! row-filter index and refuses traffic when it's broken.
//!
//! "Broken" is the cleanest pure-HTTP denial signal: a CRD with a typo'd
//! field name or operator → 500 INTERNAL_ERROR before any SQL is built.
//! Happy-path "scoped reader sees a subset" lives in the Postgres-backed
//! `phase2b_row_filter_e2e.rs` (TODO) and isn't expressible against the
//! lazy pool.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use velocity_api::registry::ResolvedSchema;
use velocity_api::router;
use velocity_api::{AppState, Identity, SchemaRegistry};
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, RowFilter, RowFilterRule,
    SchemaDefinitionSpec, SearchSpec, SearchTier,
};

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nope:nope@127.0.0.1:1/never")
        .unwrap()
}

fn spec_with_row_filter(rules: Vec<RowFilterRule>) -> SchemaDefinitionSpec {
    let f: FieldSpec =
        serde_json::from_value(json!({ "name": "po_number", "type": "string" })).unwrap();
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: NamespacedRef {
                name: "default".into(),
                namespace: "acme-platform".into(),
            },
            overrides: Vec::new(),
        },
        access: AccessSpec { row_filter: rules, ..AccessSpec::default() },
        fields: vec![f],
        validations: Vec::new(),
        search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
        time_machine: None,
        audit: None,
        archive: None,
        observability: ObservabilitySpec::default(),
        scaling: None,
    }
}

fn build_state(spec: SchemaDefinitionSpec) -> AppState {
    let (registry, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    registry.upsert(ResolvedSchema::from_spec(path, spec));
    AppState::new(Arc::clone(&registry), lazy_pool())
}

fn ident(actor: &str, roles: &[&str]) -> Identity {
    Identity {
        actor_id: actor.into(),
        roles: roles.iter().map(|s| (*s).to_string()).collect(),
        strategy: "acme-platform/default".into(),
        ..Identity::default()
    }
}

fn inject_identity(
    id: Identity,
) -> impl Clone + Fn(Request<Body>, Next) -> futures::future::BoxFuture<'static, Response> {
    move |mut req: Request<Body>, next: Next| {
        let id = id.clone();
        Box::pin(async move {
            req.extensions_mut().insert(id);
            next.run(req).await
        })
    }
}

async fn body_json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

fn rule(role: &str, field: &str, op: &str, value: Value) -> RowFilterRule {
    RowFilterRule {
        role: role.into(),
        filter: RowFilter { field: field.into(), op: op.into(), value },
    }
}

const COLLECTION: &str = "/api/acme/supply-chain/procurement/purchase-order/v1";
const ITEM: &str =
    "/api/acme/supply-chain/procurement/purchase-order/v1/00000000-0000-0000-0000-000000000001";

/// A broken row filter (field that doesn't exist) must page an operator
/// rather than silently admit — every verb that builds a WHERE should
/// surface it as 500 INTERNAL_ERROR. The four asserts collectively pin
/// the four call sites where the gate is wired.

#[tokio::test]
async fn list_500s_on_broken_row_filter() {
    let state =
        build_state(spec_with_row_filter(vec![rule("reader", "ghost_field", "eq", json!("x"))]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res = app
        .oneshot(Request::builder().method("GET").uri(COLLECTION).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "INTERNAL_ERROR");
}

#[tokio::test]
async fn get_500s_on_broken_row_filter() {
    let state =
        build_state(spec_with_row_filter(vec![rule("reader", "ghost_field", "eq", json!("x"))]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res = app
        .oneshot(Request::builder().method("GET").uri(ITEM).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "INTERNAL_ERROR");
}

#[tokio::test]
async fn update_500s_on_broken_row_filter() {
    let state =
        build_state(spec_with_row_filter(vec![rule("reader", "ghost_field", "eq", json!("x"))]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(ITEM)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "po_number": "PO-1", "version": 1 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "INTERNAL_ERROR");
}

#[tokio::test]
async fn delete_500s_on_broken_row_filter() {
    let state =
        build_state(spec_with_row_filter(vec![rule("reader", "ghost_field", "eq", json!("x"))]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res = app
        .oneshot(Request::builder().method("DELETE").uri(ITEM).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "INTERNAL_ERROR");
}

/// Unknown-op typo on a row-filter rule is also a broken predicate —
/// pinning this separately because it goes through a different branch
/// of `RowFilterIndex::from_spec` (the FilterOp::parse fallback) than
/// the unknown-field path above.
#[tokio::test]
async fn unknown_op_typo_500s_at_runtime() {
    let state = build_state(spec_with_row_filter(vec![rule(
        "reader",
        "po_number",
        "bogus_op",
        json!("x"),
    )]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res = app
        .oneshot(Request::builder().method("GET").uri(COLLECTION).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "INTERNAL_ERROR");
}

/// Empty rowFilter — handler must run as if Layer-4 isn't there. We hit
/// the lazy pool downstream; the assertion is "didn't 500 with the
/// row-filter code path", i.e. a different failure mode.
#[tokio::test]
async fn empty_row_filter_does_not_gate() {
    let state = build_state(spec_with_row_filter(vec![]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res = app
        .oneshot(Request::builder().method("GET").uri(COLLECTION).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    // With no row filter and a lazy pool, the request fails at the DB.
    // The only assertion that matters here: the error is NOT our specific
    // "rowFilter broken on role" message.
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        let msg = body["message"].as_str().unwrap_or("");
        assert!(
            !msg.contains("rowFilter"),
            "empty rowFilter should not surface as a broken-filter error, got: {msg}"
        );
    }
}
