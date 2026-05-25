#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! HTTP-level wire-through for the Layer-5 field-filter gate.
//!
//! The semantics are unit-tested in `src/field_filter.rs`; this file
//! proves every write handler actually calls `check_writes` and that
//! `FieldWriteDenied` is shaped the way a client expects.
//!
//! Read-strip is harder to cover purely at the HTTP layer (it runs on
//! rows the SQL returned — and our lazy pool never returns any). The
//! pinning for read-strip lives in unit tests; the Postgres-backed
//! `phase2b_field_filter_e2e.rs` will exercise the LIST/GET round trip
//! once that target is wired up.

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
    AccessSpec, AuthSpec, FieldAccess, FieldKind, FieldSpec, ObservabilitySpec,
    SchemaDefinitionSpec, SearchSpec, SearchTier,
};

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nope:nope@127.0.0.1:1/never")
        .unwrap()
}

fn field(name: &str, read: &[&str], write: &[&str]) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = FieldKind::String;
    if !read.is_empty() || !write.is_empty() {
        f.access = Some(FieldAccess {
            read: read.iter().map(|s| (*s).to_string()).collect(),
            write: write.iter().map(|s| (*s).to_string()).collect(),
        });
    }
    f
}

fn spec(fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
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

fn build_state(spec: SchemaDefinitionSpec) -> DataState {
    let (registry, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    registry.upsert(ResolvedSchema::from_spec(path, spec));
    DataState::new(Arc::clone(&registry), lazy_pool())
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

const COLLECTION: &str = "/api/acme/supply-chain/procurement/purchase-order/v1";
const ITEM: &str =
    "/api/acme/supply-chain/procurement/purchase-order/v1/00000000-0000-0000-0000-000000000001";

#[tokio::test]
async fn create_denied_when_writing_gated_field_without_role() {
    let state = build_state(spec(vec![
        field("po_number", &[], &[]),
        field("price", &[], &["pricing-admin"]),
    ]));
    let app =
        router::build(state).layer(from_fn(inject_identity(ident("alice", &["pricing-reader"]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(COLLECTION)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "po_number": "PO-1", "price": 42 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "FIELD_WRITE_DENIED");
    // Surface the field name so the integrator can fix their payload.
    assert!(body["message"].as_str().unwrap().contains("price"));
}

#[tokio::test]
async fn create_admitted_when_role_grants_field_write() {
    let state = build_state(spec(vec![
        field("po_number", &[], &[]),
        field("price", &[], &["pricing-admin"]),
    ]));
    let app =
        router::build(state).layer(from_fn(inject_identity(ident("alice", &["pricing-admin"]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(COLLECTION)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "po_number": "PO-1", "price": 42 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    // Field gate passed; downstream hits lazy pool. Assertion is just that
    // we did NOT 403 with FIELD_WRITE_DENIED.
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(body["error"], "FIELD_WRITE_DENIED");
}

#[tokio::test]
async fn update_denied_when_writing_gated_field() {
    let state = build_state(spec(vec![
        field("po_number", &[], &[]),
        field("price", &[], &["pricing-admin"]),
    ]));
    let app =
        router::build(state).layer(from_fn(inject_identity(ident("alice", &["pricing-reader"]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(ITEM)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "price": 99, "version": 1 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "FIELD_WRITE_DENIED");
}

#[tokio::test]
async fn update_passes_when_only_open_fields_in_payload() {
    // `price` is gated, but the payload only touches `po_number` (open).
    // Update should pass the field gate even though the actor has no
    // `pricing-admin` role.
    let state = build_state(spec(vec![
        field("po_number", &[], &[]),
        field("price", &[], &["pricing-admin"]),
    ]));
    let app =
        router::build(state).layer(from_fn(inject_identity(ident("alice", &["pricing-reader"]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(ITEM)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "po_number": "PO-9", "version": 1 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(body["error"], "FIELD_WRITE_DENIED");
}

#[tokio::test]
async fn create_passes_when_no_fields_are_gated() {
    let state = build_state(spec(vec![field("po_number", &[], &[])]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &[]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(COLLECTION)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&json!({ "po_number": "PO-1" })).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(body["error"], "FIELD_WRITE_DENIED");
}
