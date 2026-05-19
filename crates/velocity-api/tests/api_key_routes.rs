#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Router-level E2E for the ApiKey middleware path — pins the two Phase 2c
//! acceptance lines that have no other coverage:
//!
//! - "API key invalid → 401"
//! - "API key from disallowed IP → 401"
//!
//! Unit-level checks for the format-parser, hash discipline, and IP-allowlist
//! semantics live in `src/auth/api_key.rs`. This file proves the wiring:
//! `Authorization: ApiKey …` → `client_ip_from_request` reads `ConnectInfo` →
//! `authenticate_api_key` translates `ApiKeyError` to the right `ApiError`
//! variant → middleware emits the correct status with `error: UNAUTHENTICATED`.

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Extension, Json, Router};
use http_body_util::BodyExt;
use ipnet::IpNet;
use serde_json::{json, Value};
use tower::ServiceExt;
use velocity_api::auth::api_key::{ApiKeyChecker, ApiKeyError, ApiKeyRecord, ApiKeyScope};
use velocity_api::auth::{authenticate, AuthRegistry, AuthState, JwksCache, ResolvedAuthStrategy};
use velocity_api::registry::ResolvedSchema;
use velocity_api::{Identity, SchemaRegistry};
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::auth::{AuthStrategyConfig, AuthStrategySpec, AuthStrategyType};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
};

const STRATEGY_NS: &str = "acme-platform";
const STRATEGY_NAME: &str = "api-keys";
/// Well-formed plaintext — passes `validate_plaintext` so the mock checker's
/// "not found" branch is what actually trips, not the structural parse.
const VALID_KEY: &str = "vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1";

// ─── Mock checker ──────────────────────────────────────────────────────────

/// Tiny in-process [`ApiKeyChecker`]. Two variants — admit-a-known-record or
/// always-NotFound — cover both tests in this file.
#[derive(Debug)]
struct MockChecker {
    record: Option<ApiKeyRecord>,
}

impl MockChecker {
    fn missing() -> Self {
        Self { record: None }
    }

    fn admit(record: ApiKeyRecord) -> Self {
        Self { record: Some(record) }
    }
}

#[async_trait]
impl ApiKeyChecker for MockChecker {
    async fn lookup(&self, plaintext: &str) -> Result<ApiKeyRecord, ApiKeyError> {
        // Match the production checker's ordering — structural parse first
        // so a malformed plaintext never reaches the "lookup" stage.
        velocity_api::auth::api_key::validate_plaintext(plaintext)?;
        self.record.clone().ok_or(ApiKeyError::NotFound)
    }
}

// ─── Fixtures ──────────────────────────────────────────────────────────────

fn schema_spec() -> SchemaDefinitionSpec {
    let f: FieldSpec =
        serde_json::from_value(json!({ "name": "po_number", "type": "string" })).unwrap();
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: NamespacedRef {
                name: STRATEGY_NAME.into(),
                namespace: STRATEGY_NS.into(),
            },
            overrides: Vec::new(),
        },
        access: AccessSpec::default(),
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

fn api_key_strategy() -> ResolvedAuthStrategy {
    let spec =
        AuthStrategySpec { kind: AuthStrategyType::ApiKey, config: AuthStrategyConfig::default() };
    let strategy_ref = NamespacedRef { namespace: STRATEGY_NS.into(), name: STRATEGY_NAME.into() };
    ResolvedAuthStrategy::from_spec(&strategy_ref, spec)
}

fn build_router(checker: Arc<dyn ApiKeyChecker>) -> Router {
    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));

    let strategies = AuthRegistry::new();
    strategies.upsert(api_key_strategy());

    let auth_state = AuthState::new(schemas, strategies, JwksCache::new()).with_api_keys(checker);

    Router::new()
        .route("/api/{org}/{app}/{domain}/{object}/{version}", get(echo_identity))
        .layer(from_fn_with_state(auth_state, authenticate))
}

async fn echo_identity(identity: Option<Extension<Identity>>) -> impl IntoResponse {
    match identity {
        Some(Extension(id)) => {
            (StatusCode::OK, Json(json!({ "actor_id": id.actor_id }))).into_response()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "no identity").into_response(),
    }
}

async fn read_body(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// `oneshot` requests don't have a backing TCP socket, so the middleware's
/// `ConnectInfo<SocketAddr>` extractor sees `None` unless we inject it
/// ourselves. The production wiring uses
/// `into_make_service_with_connect_info::<SocketAddr>` to populate this on
/// real connections — see `crates/velocity-api/src/main.rs`.
fn inject_connect_info(mut req: Request<Body>, peer: SocketAddr) -> Request<Body> {
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

fn req_with_auth(header: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header(axum::http::header::AUTHORIZATION, header)
        .body(Body::empty())
        .unwrap()
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn invalid_api_key_returns_unauthenticated() {
    // Phase 2c acceptance line: "API key invalid → 401".
    // Well-formed plaintext, but the checker has no row → NotFound →
    // ApiError::Unauthenticated → 401 with body { "error": "UNAUTHENTICATED" }.
    let app = build_router(Arc::new(MockChecker::missing()));

    let req = req_with_auth(&format!("ApiKey {VALID_KEY}"));
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "UNAUTHENTICATED");
}

#[tokio::test]
async fn malformed_api_key_returns_unauthenticated() {
    // Structural parse rejects before any DB I/O — even the format check
    // surfaces as 401, never as 400 or 500. (A 400 would let a probe
    // distinguish "key shape was right but lookup missed" from "key shape
    // was wrong" and feed brute-force iteration.)
    let app = build_router(Arc::new(MockChecker::missing()));

    let req = req_with_auth("ApiKey not-a-valid-key-shape");
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_key_from_disallowed_ip_returns_unauthenticated() {
    // Phase 2c acceptance line: "API key from disallowed IP → 401".
    // Allowlist 10.0.0.0/8; ConnectInfo says 192.0.2.7 (TEST-NET-1, never
    // a routable corporate prefix). Middleware enforces against ConnectInfo
    // only — XFF / X-Real-IP are spoofable and not honoured.
    let record = ApiKeyRecord {
        key: format!("{STRATEGY_NS}/erp-sync"),
        actor: "erp-sync-service".into(),
        actor_type: "service".into(),
        scopes: vec![ApiKeyScope {
            schema: "purchase-order".into(),
            version: Some("v1".into()),
            operations: vec!["read".into()],
        }],
        ip_allowlist: vec![IpNet::from_str("10.0.0.0/8").unwrap()],
    };
    let app = build_router(Arc::new(MockChecker::admit(record)));

    let req = inject_connect_info(
        req_with_auth(&format!("ApiKey {VALID_KEY}")),
        SocketAddr::from(([192, 0, 2, 7], 49152)),
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "UNAUTHENTICATED");
}

#[tokio::test]
async fn api_key_with_no_connect_info_is_denied_when_allowlist_present() {
    // Belt-and-braces: even if `ConnectInfo` somehow isn't populated (e.g.
    // a future change to the server wiring), a key with a non-empty
    // allowlist MUST default to deny rather than admit. The production
    // server inserts `ConnectInfo` for every connection via
    // `into_make_service_with_connect_info::<SocketAddr>`; this test pins
    // the fail-closed branch in the absence of that.
    let record = ApiKeyRecord {
        key: format!("{STRATEGY_NS}/erp-sync"),
        actor: "erp-sync-service".into(),
        actor_type: "service".into(),
        scopes: vec![],
        ip_allowlist: vec![IpNet::from_str("10.0.0.0/8").unwrap()],
    };
    let app = build_router(Arc::new(MockChecker::admit(record)));

    let req = req_with_auth(&format!("ApiKey {VALID_KEY}"));
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_key_from_allowed_ip_is_admitted() {
    // Positive sanity: the same allowlist that rejects 192.0.2.7 admits
    // 10.1.2.3. Without this, the disallow test could pass for the wrong
    // reason (e.g. the allowlist match was broken, not the deny).
    let record = ApiKeyRecord {
        key: format!("{STRATEGY_NS}/erp-sync"),
        actor: "erp-sync-service".into(),
        actor_type: "service".into(),
        // Scopes empty here is intentional — Layer-1 scope-check runs in
        // the handler, not the middleware. Middleware only proves the
        // credential; admit = 200 from the dummy `echo_identity` handler.
        scopes: vec![],
        ip_allowlist: vec![IpNet::from_str("10.0.0.0/8").unwrap()],
    };
    let app = build_router(Arc::new(MockChecker::admit(record)));

    let peer = SocketAddr::new(IpAddr::from_str("10.1.2.3").unwrap(), 49152);
    let req = inject_connect_info(req_with_auth(&format!("ApiKey {VALID_KEY}")), peer);
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "erp-sync-service");
}
