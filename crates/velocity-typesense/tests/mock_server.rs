//! Mock-server tests for `TypesenseClient::create_collection`.
//!
//! Phase 5d-2: the operator now calls `create_collection` at reconcile
//! time. Three behaviours matter to the rest of the platform:
//!
//! 1. 201 → Ok, and the body we POST matches the `CollectionSpec` JSON
//!    the operator is going to feed Typesense in prod.
//! 2. 409 → Ok (idempotent: two replicas racing on the same schema
//!    must both succeed).
//! 3. 503 → `Err(TypesenseError::Status { status: 503, .. })`. This is
//!    the ADR-003 fail-loud guarantee: the operator surfaces the
//!    failure to kube-runtime, which requeues. No silent success.
//!
//! Implemented as a tiny axum server bound to `127.0.0.1:0` so each
//! test gets its own port. `axum` is already a workspace dep — no new
//! transitive dependencies pulled in for tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use velocity_types::common::SchemaPath;
use velocity_typesense::{collection_spec, TypesenseClient, TypesenseError};

#[derive(Clone, Default)]
struct CapturedBody(Arc<Mutex<Option<Bytes>>>);

async fn spawn(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service()).await.unwrap();
    });
    addr
}

fn sample_spec() -> velocity_types::crds::schema::SchemaDefinitionSpec {
    serde_json::from_value(serde_json::json!({
        "version": "v1",
        "auth":    { "strategyRef": { "name": "default", "namespace": "p" } },
        "access":  {},
        "fields":  [
            { "name": "po_number", "type": "string", "required": true, "filterable": true },
            { "name": "description", "type": "string", "searchable": true }
        ],
        "search":  { "tier": "Tier3" }
    }))
    .unwrap()
}

fn sample_path() -> SchemaPath {
    SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1")
}

#[tokio::test]
async fn create_collection_201_sends_collection_spec_body() {
    let captured = CapturedBody::default();
    let app = Router::new()
        .route(
            "/collections",
            post(|State(c): State<CapturedBody>, body: Bytes| async move {
                *c.0.lock().unwrap() = Some(body);
                (StatusCode::CREATED, "{}")
            }),
        )
        .with_state(captured.clone());
    let addr = spawn(app).await;

    let client = TypesenseClient::new(format!("http://{addr}"), "xyz").unwrap();
    let path = sample_path();
    let spec = collection_spec(&path, &sample_spec());

    client.create_collection(&spec).await.expect("201 → Ok");

    let body = captured.0.lock().unwrap().clone().expect("server received POST body");
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["name"], "acme_supply_chain_procurement_purchase_order_v1");
    let fields = parsed["fields"].as_array().expect("fields is array");
    let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"id"));
    assert!(names.contains(&"__schema"));
    assert!(names.contains(&"po_number"));
    assert!(names.contains(&"description"));
}

#[tokio::test]
async fn create_collection_409_is_idempotent_ok() {
    let app = Router::new().route(
        "/collections",
        post(|| async {
            (StatusCode::CONFLICT, r#"{"message":"A collection with name `x` already exists."}"#)
        }),
    );
    let addr = spawn(app).await;

    let client = TypesenseClient::new(format!("http://{addr}"), "xyz").unwrap();
    let spec = collection_spec(&sample_path(), &sample_spec());

    client
        .create_collection(&spec)
        .await
        .expect("409 must be treated as Ok — two replicas may race on same schema");
}

#[tokio::test]
async fn upsert_alias_puts_collection_name_body() {
    let captured = CapturedBody::default();
    let app = Router::new()
        .route(
            "/aliases/{alias}",
            put(|State(c): State<CapturedBody>, body: Bytes| async move {
                *c.0.lock().unwrap() = Some(body);
                (StatusCode::OK, r#"{"name":"a","collection_name":"a__deadbeef"}"#)
            }),
        )
        .with_state(captured.clone());
    let addr = spawn(app).await;

    let client = TypesenseClient::new(format!("http://{addr}"), "xyz").unwrap();
    client.upsert_alias("a", "a__deadbeef").await.expect("alias upsert succeeds");

    let body = captured.0.lock().unwrap().clone().expect("body captured");
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["collection_name"], "a__deadbeef");
}

#[tokio::test]
async fn get_alias_200_returns_target_404_returns_none() {
    let app = Router::new().route(
        "/aliases/{alias}",
        get(|Path(alias): Path<String>| async move {
            if alias == "present" {
                (
                    StatusCode::OK,
                    Json(json!({ "name": "present", "collection_name": "present__deadbeef" })),
                )
                    .into_response()
            } else {
                (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" }))).into_response()
            }
        }),
    );
    let addr = spawn(app).await;

    let client = TypesenseClient::new(format!("http://{addr}"), "xyz").unwrap();
    let target = client.get_alias("present").await.expect("200 ok");
    assert_eq!(target.as_deref(), Some("present__deadbeef"));
    let absent = client.get_alias("missing").await.expect("404 → None");
    assert_eq!(absent, None);
}

#[tokio::test]
async fn delete_alias_404_is_idempotent_ok() {
    let app =
        Router::new().route("/aliases/{alias}", delete(|| async { (StatusCode::NOT_FOUND, "{}") }));
    let addr = spawn(app).await;

    let client = TypesenseClient::new(format!("http://{addr}"), "xyz").unwrap();
    client.delete_alias("gone").await.expect("404 on delete is idempotent ok");
}

#[tokio::test]
async fn create_collection_503_surfaces_status_error() {
    let app = Router::new().route(
        "/collections",
        post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "upstream down") }),
    );
    let addr = spawn(app).await;

    let client = TypesenseClient::new(format!("http://{addr}"), "xyz").unwrap();
    let spec = collection_spec(&sample_path(), &sample_spec());

    let err = client
        .create_collection(&spec)
        .await
        .expect_err("503 must surface as Err so kube-runtime requeues (ADR-003 fail-loud)");
    match err {
        TypesenseError::Status { status, body } => {
            assert_eq!(status, 503);
            assert!(body.contains("upstream down"), "body propagates: {body}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
}
