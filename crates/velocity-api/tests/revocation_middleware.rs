#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! ADR-003 fail-mode matrix integration tests for the revocation gate.
//!
//! Each test builds a real Axum router with the auth middleware wired up
//! and drives it via `tower::Service::oneshot`. We reuse the same RSA JWKS
//! sidecar approach as `jwt_middleware.rs` to make the JWT itself genuine —
//! the only thing varying between tests is the revocation backend's
//! behaviour and the strategy's `failOpen` flag.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::BodyExt;
use jsonwebtoken::{encode, EncodingKey, Header};
use rsa::pkcs8::EncodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use velocity_api::auth::{
    authenticate, AuthDecision, AuthRegistry, AuthState, JwksCache, MockChecker,
    ResolvedAuthStrategy,
};
use velocity_api::registry::ResolvedSchema;
use velocity_api::{Identity, SchemaRegistry};
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::auth::{
    AuthStrategySpec, AuthStrategyType, ClaimMapping, IssuerConfig as CrdIssuer, RevocationConfig,
};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
};

// ─── JWKS sidecar + token helpers (mirror jwt_middleware.rs) ──────────────

fn make_keypair(kid: &str) -> (Value, EncodingKey) {
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pub_key = priv_key.to_public_key();
    let n = URL_SAFE_NO_PAD.encode(pub_key.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(pub_key.e().to_bytes_be());
    let jwk = json!({
        "kty": "RSA", "use": "sig", "alg": "RS256",
        "kid": kid, "n": n, "e": e,
    });
    let pem = priv_key.to_pkcs8_pem(Default::default()).unwrap().to_string();
    let enc = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
    (jwk, enc)
}

#[derive(Clone, Default)]
struct JwksFixture {
    body: Arc<Mutex<Value>>,
}

async fn jwks_handler(State(s): State<JwksFixture>) -> Json<Value> {
    Json(s.body.lock().await.clone())
}

async fn spawn_jwks(jwk: Value) -> (String, JoinHandle<()>) {
    let fixture = JwksFixture::default();
    *fixture.body.lock().await = json!({ "keys": [jwk] });
    let app = Router::new().route("/jwks.json", get(jwks_handler)).with_state(fixture);
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/jwks.json"), task)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn mint(iss: &str, sub: &str, kid: &str, key: &EncodingKey) -> String {
    let now = now() as i64;
    let claims = json!({ "iss": iss, "sub": sub, "iat": now, "exp": now + 3600 });
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.into());
    encode(&header, &claims, key).unwrap()
}

// ─── Schema + strategy specs ───────────────────────────────────────────────

fn schema_spec(strategy_ns: &str, strategy_name: &str) -> SchemaDefinitionSpec {
    let f: FieldSpec =
        serde_json::from_value(json!({ "name": "po_number", "type": "string" })).unwrap();
    SchemaDefinitionSpec {
        version: "v1".into(),
        partitioning: None,
        auth: AuthSpec {
            strategy_ref: NamespacedRef {
                name: strategy_name.into(),
                namespace: strategy_ns.into(),
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

fn strategy_spec(iss: &str, jwks_url: &str, fail_open: bool) -> AuthStrategySpec {
    let claims = ClaimMapping {
        actor_id: Some(Value::String("$.sub".into())),
        ..Default::default()
    };
    AuthStrategySpec {
        kind: AuthStrategyType::Jwt,
        config: velocity_types::crds::auth::AuthStrategyConfig {
            issuers: vec![CrdIssuer {
                issuer: iss.into(),
                jwks_url: jwks_url.into(),
                audience: None,
                claims,
            }],
            clock_skew: Some(30),
            revocation: Some(RevocationConfig {
                backend: "redis".into(),
                key: None,
                fail_open,
                ttl: None,
            }),
            ..Default::default()
        },
    }
}

async fn echo_decision(
    identity: Option<Extension<Identity>>,
    decision: Option<Extension<AuthDecision>>,
) -> impl IntoResponse {
    let (Some(Extension(id)), Some(Extension(dec))) = (identity, decision) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "missing extensions").into_response();
    };
    (
        StatusCode::OK,
        Json(json!({
            "actor_id": id.actor_id,
            "decision": dec.revocation.as_audit_str(),
            "fail_open": dec.revocation_fail_open,
            "strategy": dec.strategy,
        })),
    )
        .into_response()
}

async fn build_router(
    iss: &str,
    jwks_url: String,
    fail_open: bool,
    checker: MockChecker,
) -> Router {
    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    schemas.upsert(ResolvedSchema::from_spec(
        path.clone(),
        schema_spec("acme-platform", "default"),
    ));

    let strategies = AuthRegistry::new();
    let strategy_ref = NamespacedRef { name: "default".into(), namespace: "acme-platform".into() };
    let resolved =
        ResolvedAuthStrategy::from_spec(&strategy_ref, strategy_spec(iss, &jwks_url, fail_open));
    let jwks = JwksCache::new();
    resolved.prime_jwks(&jwks).await;
    strategies.upsert(resolved.clone());

    let auth_state = AuthState::new(schemas, strategies, jwks)
        .with_revocation(Arc::new(checker));
    auth_state.prime_strategy(&resolved).unwrap();

    Router::new()
        .route("/api/{org}/{app}/{domain}/{object}/{version}", get(echo_decision))
        .layer(from_fn_with_state(auth_state, authenticate))
}

async fn read_body(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn auth_request(token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn not_revoked_admits_and_records_allowed() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    let app = build_router(iss, jwks_url, /*fail_open=*/ false, checker).await;

    let token = mint(iss, "alice", "k1", &enc);
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "alice");
    assert_eq!(body["decision"], "allowed");
    assert_eq!(body["fail_open"], false);
    assert_eq!(body["strategy"], "acme-platform/default");
}

#[tokio::test]
async fn revoked_actor_is_rejected_with_403() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    checker.revoke("mallory");
    let app = build_router(iss, jwks_url, /*fail_open=*/ false, checker).await;

    let token = mint(iss, "mallory", "k1", &enc);
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "ACTOR_REVOKED");
}

#[tokio::test]
async fn revoked_actor_is_rejected_even_when_fail_open() {
    // failOpen only changes what happens when the *backend* is unreachable.
    // An explicit revocation must still bite — this guards against a wiring
    // bug where the middleware short-circuits the check under fail-open.
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    checker.revoke("mallory");
    let app = build_router(iss, jwks_url, /*fail_open=*/ true, checker).await;

    let token = mint(iss, "mallory", "k1", &enc);
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn backend_down_fail_closed_returns_503() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    checker.set_failing(true);
    let app = build_router(iss, jwks_url, /*fail_open=*/ false, checker).await;

    let token = mint(iss, "alice", "k1", &enc);
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "REVOCATION_UNAVAILABLE");
}

#[tokio::test]
async fn backend_down_fail_open_admits_and_records_decision() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    checker.set_failing(true);
    let app = build_router(iss, jwks_url, /*fail_open=*/ true, checker).await;

    let token = mint(iss, "alice", "k1", &enc);
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    // The audit pipeline (task #26) will key off this string to flag a
    // burst of admitted-but-unverified traffic. Changing it is a breaking
    // change for dashboards.
    assert_eq!(body["decision"], "backend_down_admitted");
    assert_eq!(body["fail_open"], true);
}

#[tokio::test]
async fn revocation_lifts_when_actor_is_un_revoked() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    checker.revoke("oscar");
    let app = build_router(iss, jwks_url.clone(), false, checker.clone()).await;

    let token = mint(iss, "oscar", "k1", &enc);
    // Build a clone-per-request router because `oneshot` consumes Service.
    // The auth state itself is `Clone` so we can rebuild cheaply.
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Un-revoke and rebuild the router with the same checker (the MockChecker
    // is `Arc`-shared internally, so the second router observes the change).
    let app2 = build_router(iss, jwks_url, false, checker.clone()).await;
    checker.unrevoke("oscar");
    let res2 = app2.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res2.status(), StatusCode::OK);
    let body = read_body(res2.into_body()).await;
    assert_eq!(body["decision"], "allowed");
}

#[tokio::test]
async fn no_checker_configured_admits_and_records_allowed() {
    // The middleware must not gate when `with_revocation` was never called
    // — otherwise tests that don't care about revocation would have to
    // wire a checker just to pass. Production startup configures one
    // explicitly; the absence is a deliberate test affordance.
    use velocity_api::auth::authenticate as authn;
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;

    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    schemas.upsert(ResolvedSchema::from_spec(
        path.clone(),
        schema_spec("acme-platform", "default"),
    ));
    let strategies = AuthRegistry::new();
    let strategy_ref = NamespacedRef { name: "default".into(), namespace: "acme-platform".into() };
    let resolved =
        ResolvedAuthStrategy::from_spec(&strategy_ref, strategy_spec(iss, &jwks_url, false));
    let jwks = JwksCache::new();
    resolved.prime_jwks(&jwks).await;
    strategies.upsert(resolved.clone());

    let auth_state = AuthState::new(schemas, strategies, jwks); // no .with_revocation
    auth_state.prime_strategy(&resolved).unwrap();

    let app: Router = Router::new()
        .route("/api/{org}/{app}/{domain}/{object}/{version}", get(echo_decision))
        .layer(from_fn_with_state(auth_state, authn));

    let token = mint(iss, "alice", "k1", &enc);
    let res = app.oneshot(auth_request(&token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["decision"], "allowed");
}
