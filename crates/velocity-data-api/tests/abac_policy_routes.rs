#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! HTTP-level wire-through for the Layer-2 ABAC gate.
//!
//! The policy evaluator is unit-tested in `src/policy.rs`; this file
//! proves every write handler actually invokes [`policy::evaluate_for`]
//! before any SQL is built. We drive the production router with a
//! lazy-connect pool (denial paths never touch Postgres) and inject an
//! Identity via a thin middleware — same pattern as `rbac_routes.rs`.

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
    AbacPolicy, AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nope:nope@127.0.0.1:1/never")
        .unwrap()
}

fn spec_with_policies(policies: Vec<AbacPolicy>) -> SchemaDefinitionSpec {
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
        access: AccessSpec { policies, ..AccessSpec::default() },
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

fn build_state(spec: SchemaDefinitionSpec) -> DataState {
    let (registry, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    registry.upsert(ResolvedSchema::from_spec(path, spec));
    DataState::new(Arc::clone(&registry), lazy_pool())
}

fn ident(actor: &str, attrs: &[(&str, &str)]) -> Identity {
    Identity {
        actor_id: actor.into(),
        attributes: attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
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

fn policy(name: &str, action: &str, condition: &str, message: &str) -> AbacPolicy {
    AbacPolicy {
        name: name.into(),
        action: action.into(),
        fields: Vec::new(),
        condition: condition.into(),
        message: Some(message.into()),
    }
}

const COLLECTION: &str = "/api/acme/supply-chain/procurement/purchase-order/v1";
const ITEM: &str =
    "/api/acme/supply-chain/procurement/purchase-order/v1/00000000-0000-0000-0000-000000000001";

#[tokio::test]
async fn create_denied_by_policy() {
    // Identity has no `budget_limit` attribute → CEL raises at runtime →
    // fail-closed deny. Mirrors the real-world misconfiguration where the
    // RoleBinding is missing the attribute the policy expects.
    let p = policy(
        "budget-cap",
        "create",
        "self.po_number == 'PO-CHEAP'",
        "PO must be a cheap one for this test",
    );
    let state = build_state(spec_with_policies(vec![p]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &[]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(COLLECTION)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "po_number": "PO-EXPENSIVE" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "POLICY_DENIED");
    assert!(body["message"].as_str().unwrap().contains("cheap one"));
}

#[tokio::test]
async fn create_admitted_when_policy_matches() {
    // Open path: policy is "self.po_number == 'PO-CHEAP'", payload
    // matches, request goes past the gate. Hits the lazy pool and 5xxs
    // — the assertion is that it didn't *deny* with 403.
    let p = policy("ok", "create", "self.po_number == 'PO-CHEAP'", "must be cheap");
    let state = build_state(spec_with_policies(vec![p]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &[]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(COLLECTION)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&json!({ "po_number": "PO-CHEAP" })).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(body["error"], "POLICY_DENIED");
}

#[tokio::test]
async fn update_runs_policy() {
    let p =
        policy("no-rejected", "update", "self.po_number != 'POISON'", "that PO number is reserved");
    let state = build_state(spec_with_policies(vec![p]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &[]))));
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(ITEM)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "po_number": "POISON", "version": 1 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "POLICY_DENIED");
}

#[tokio::test]
async fn delete_runs_policy_with_null_self() {
    // For delete the policy sees `self = null`. Express the rule against
    // identity attributes instead — same shape an audit-only rule
    // ("only managers may delete") would take in production.
    let p = policy(
        "manager-only-delete",
        "delete",
        "identity.attributes.role == 'manager'",
        "only managers may delete",
    );
    let state = build_state(spec_with_policies(vec![p]));
    let app = router::build(state).layer(from_fn(inject_identity(ident("alice", &[]))));
    let res = app
        .oneshot(Request::builder().method("DELETE").uri(ITEM).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = body_json(res).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "POLICY_DENIED");
}

#[tokio::test]
async fn no_policies_means_no_extra_gate() {
    // Empty policy list → handler proceeds straight to DB. Lazy pool
    // refuses to connect; we verify the failure mode is NOT 403, proving
    // the policy gate didn't block the request.
    let state = build_state(spec_with_policies(vec![]));
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
    let (status, _) = body_json(res).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
}
