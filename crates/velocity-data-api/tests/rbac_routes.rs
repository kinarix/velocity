#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! HTTP-level wiring check for the Layer-1 RBAC gate.
//!
//! The gate itself is exhaustively covered by unit tests on
//! [`velocity_core::rbac::check_route_access`]. The job here is to prove
//! every CRUD handler actually invokes the gate *before* doing any work —
//! a refactor that forgets one of the call sites would silently regress.
//!
//! Denial paths never touch Postgres, so we use a lazy-connect pool that
//! is never actually opened. Admit paths against a closed schema would hit
//! the DB; those are covered by `phase1_crud.rs` (which runs against a
//! real Postgres in CI).

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
use velocity_core::registry::ResolvedSchema;
use velocity_data_api::router;
use velocity_core::{Identity, SchemaRegistry};
use velocity_data_api::DataState;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, RoleAccess, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

// ─── Fixtures ──────────────────────────────────────────────────────────────

fn open_schema_spec() -> SchemaDefinitionSpec {
    base_spec(AccessSpec::default())
}

fn closed_schema_spec(roles: Vec<RoleAccess>) -> SchemaDefinitionSpec {
    base_spec(AccessSpec { roles, ..AccessSpec::default() })
}

fn base_spec(access: AccessSpec) -> SchemaDefinitionSpec {
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
        access,
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

fn role(name: &str, ops: &[&str]) -> RoleAccess {
    RoleAccess { role: name.into(), operations: ops.iter().map(|s| (*s).into()).collect() }
}

/// Pool that never connects. Sufficient for tests whose handler paths
/// return before any SQL is dispatched.
fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nope:nope@127.0.0.1:1/never")
        .unwrap()
}

fn build_state(spec: SchemaDefinitionSpec) -> DataState {
    let (registry, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    registry.upsert(ResolvedSchema::from_spec(path, spec));
    DataState::new(Arc::clone(&registry), lazy_pool())
}

/// Inject a known Identity into the request extension — substitutes for
/// the real auth middleware so these tests don't drag in JWKS/JWT machinery.
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

fn ident(actor: &str, roles: &[&str]) -> Identity {
    Identity {
        actor_id: actor.into(),
        roles: roles.iter().map(|s| (*s).into()).collect(),
        strategy: "acme-platform/default".into(),
        ..Identity::default()
    }
}

async fn body_json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

fn req(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

const COLLECTION: &str = "/api/acme/supply-chain/procurement/purchase-order/v1";
const ITEM: &str =
    "/api/acme/supply-chain/procurement/purchase-order/v1/00000000-0000-0000-0000-000000000001";

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn closed_schema_denies_anonymous_on_list() {
    let app = router::build(build_state(closed_schema_spec(vec![role("reader", &["read"])])));
    let res = app.oneshot(req("GET", COLLECTION, None)).await.unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "ACCESS_DENIED");
}

#[tokio::test]
async fn closed_schema_denies_anonymous_on_get_one() {
    let app = router::build(build_state(closed_schema_spec(vec![role("reader", &["read"])])));
    let res = app.oneshot(req("GET", ITEM, None)).await.unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "ACCESS_DENIED");
}

#[tokio::test]
async fn closed_schema_denies_anonymous_on_create() {
    let app = router::build(build_state(closed_schema_spec(vec![role("writer", &["create"])])));
    let res =
        app.oneshot(req("POST", COLLECTION, Some(json!({"po_number": "PO-1"})))).await.unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "ACCESS_DENIED");
}

#[tokio::test]
async fn closed_schema_denies_anonymous_on_update() {
    let app = router::build(build_state(closed_schema_spec(vec![role("writer", &["update"])])));
    let res = app
        .oneshot(req("PUT", ITEM, Some(json!({"po_number": "PO-1", "version": 1}))))
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "ACCESS_DENIED");
}

#[tokio::test]
async fn closed_schema_denies_anonymous_on_delete() {
    let app = router::build(build_state(closed_schema_spec(vec![role("admin", &["delete"])])));
    let res = app.oneshot(req("DELETE", ITEM, None)).await.unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "ACCESS_DENIED");
}

#[tokio::test]
async fn closed_schema_denies_role_without_op_grant() {
    // Identity carries "reader", schema's CREATE is reserved for "writer".
    // Confirms the lookup is per-op, not per-schema — a role with *some*
    // grant on the schema must still be rejected for an unrelated op.
    let state = build_state(closed_schema_spec(vec![
        role("reader", &["read"]),
        role("writer", &["create", "update"]),
    ]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &["reader"]))));
    let res =
        app.oneshot(req("POST", COLLECTION, Some(json!({"po_number": "PO-1"})))).await.unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "ACCESS_DENIED");
}

#[tokio::test]
async fn closed_schema_denies_unrelated_role() {
    let state = build_state(closed_schema_spec(vec![role("reader", &["read"])]));
    let app = router::build(state)
        .layer(from_fn(inject_identity(ident("alice", &["stranger", "other"]))));
    let res = app.oneshot(req("GET", COLLECTION, None)).await.unwrap();
    let (status, _) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn open_schema_admits_anonymous_past_gate() {
    // Open schema (no access.roles) must let anonymous through the gate.
    // The handler then tries to hit Postgres against a lazy pool that
    // never connects — we see a 500 with DATABASE_ERROR rather than 403.
    // That's the signal we want: the gate didn't reject, so RBAC is wired
    // correctly for the open-schema branch.
    let app = router::build(build_state(open_schema_spec()));
    let res = app.oneshot(req("GET", COLLECTION, None)).await.unwrap();
    let (status, body) = body_json(res).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "open schema must not return ACCESS_DENIED");
    assert_ne!(body["error"], "ACCESS_DENIED");
}
