#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! JWT middleware round-trip: spins up a tiny axum JWKS sidecar with an
//! RSA keypair we control, registers the issuer on an `AuthRegistry`, then
//! drives the middleware via `tower::Service::oneshot` to assert the
//! verify-and-map flow end-to-end.

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
use pkcs8::EncodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use velocity_api::auth::{authenticate, AuthRegistry, AuthState, JwksCache, ResolvedAuthStrategy};
use velocity_api::registry::ResolvedSchema;
use velocity_api::{Identity, SchemaRegistry};
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::auth::{
    AuthStrategySpec, AuthStrategyType, ClaimMapping, IssuerConfig as CrdIssuer,
};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
};

// pkcs8 is exposed as a re-export from rsa; pull the trait directly to
// avoid an extra dev-dep entry.
mod pkcs8 {
    pub(crate) use rsa::pkcs8::EncodePrivateKey;
}

/// Generate an RSA-2048 keypair and produce: the JWK (public), the PKCS#8
/// PEM (private), and the `kid` we tagged it with.
fn make_keypair(kid: &str) -> (Value, EncodingKey) {
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pub_key = priv_key.to_public_key();
    let n = URL_SAFE_NO_PAD.encode(pub_key.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(pub_key.e().to_bytes_be());
    let jwk = json!({
        "kty": "RSA",
        "use": "sig",
        "alg": "RS256",
        "kid": kid,
        "n": n,
        "e": e,
    });
    let pem = priv_key.to_pkcs8_pem(Default::default()).unwrap().to_string();
    let enc_key = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
    (jwk, enc_key)
}

#[derive(Clone, Default)]
struct JwksFixture {
    body: Arc<Mutex<Value>>,
}

async fn jwks_handler(State(state): State<JwksFixture>) -> Json<Value> {
    Json(state.body.lock().await.clone())
}

async fn spawn_jwks(jwk: Value) -> (String, JwksFixture, JoinHandle<()>) {
    let fixture = JwksFixture::default();
    *fixture.body.lock().await = json!({ "keys": [jwk] });
    let app = Router::new().route("/jwks.json", get(jwks_handler)).with_state(fixture.clone());
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/jwks.json"), fixture, task)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[derive(Debug, Clone)]
struct ClaimsBuilder {
    iss: String,
    aud: Option<String>,
    sub: String,
    exp_delta: i64,
    extra: serde_json::Map<String, Value>,
}

impl ClaimsBuilder {
    fn new(iss: &str, sub: &str) -> Self {
        Self {
            iss: iss.into(),
            aud: None,
            sub: sub.into(),
            exp_delta: 3600,
            extra: serde_json::Map::new(),
        }
    }
    fn aud(mut self, aud: &str) -> Self {
        self.aud = Some(aud.into());
        self
    }
    fn exp_delta(mut self, delta_secs: i64) -> Self {
        self.exp_delta = delta_secs;
        self
    }
    fn with(mut self, key: &str, v: Value) -> Self {
        self.extra.insert(key.into(), v);
        self
    }
    fn build(self) -> Value {
        let now = now() as i64;
        let mut m = self.extra;
        m.insert("iss".into(), Value::String(self.iss));
        m.insert("sub".into(), Value::String(self.sub));
        if let Some(aud) = self.aud {
            m.insert("aud".into(), Value::String(aud));
        }
        m.insert("iat".into(), json!(now));
        m.insert("exp".into(), json!(now + self.exp_delta));
        Value::Object(m)
    }
}

fn mint(claims: Value, kid: &str, key: &EncodingKey) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.into());
    encode(&header, &claims, key).unwrap()
}

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

fn strategy_spec(iss: &str, jwks_url: &str, aud: Option<&str>) -> AuthStrategySpec {
    let claims = ClaimMapping {
        actor_id: Some(Value::String("$.sub".into())),
        roles: Some(json!({
            "path": "$.scope",
            "transform": { "type": "scope_to_roles" }
        })),
        ..Default::default()
    };
    AuthStrategySpec {
        kind: AuthStrategyType::Jwt,
        config: velocity_types::crds::auth::AuthStrategyConfig {
            issuers: vec![CrdIssuer {
                issuer: iss.into(),
                jwks_url: jwks_url.into(),
                audience: aud.map(str::to_string),
                claims,
            }],
            clock_skew: Some(30),
            ..Default::default()
        },
    }
}

async fn build_authenticated_router(iss: &str, aud: Option<&str>, jwks_url: String) -> Router {
    // Schema registry with one schema.
    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    let spec = schema_spec("acme-platform", "default");
    schemas.upsert(ResolvedSchema::from_spec(path.clone(), spec));

    // Auth registry with one strategy.
    let strategies = AuthRegistry::new();
    let strategy_ref = NamespacedRef { name: "default".into(), namespace: "acme-platform".into() };
    let resolved =
        ResolvedAuthStrategy::from_spec(&strategy_ref, strategy_spec(iss, &jwks_url, aud));

    let jwks = JwksCache::new();
    resolved.prime_jwks(&jwks).await;
    strategies.upsert(resolved.clone());

    let auth_state = AuthState::new(schemas, strategies, jwks);
    auth_state.prime_strategy(&resolved).unwrap();

    Router::new()
        .route("/api/{org}/{app}/{domain}/{object}/{version}", get(echo_identity))
        .route("/healthz", get(|| async { "ok" }))
        .layer(from_fn_with_state(auth_state, authenticate))
}

async fn echo_identity(identity: Option<Extension<Identity>>) -> impl IntoResponse {
    match identity {
        Some(Extension(id)) => (
            StatusCode::OK,
            Json(json!({
                "actor_id": id.actor_id,
                "roles": id.roles,
                "strategy": id.strategy,
                "issuer": id.issuer,
            })),
        )
            .into_response(),
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

#[tokio::test]
async fn valid_token_is_admitted_and_identity_is_attached() {
    let iss = "https://idp.test";
    let aud = "velocity-api";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk).await;
    let app = build_authenticated_router(iss, Some(aud), jwks_url).await;

    let claims =
        ClaimsBuilder::new(iss, "alice").aud(aud).with("scope", json!("read:po write:po")).build();
    let token = mint(claims, "k1", &enc);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "alice");
    assert_eq!(body["roles"], json!(["read:po", "write:po"]));
    assert_eq!(body["issuer"], iss);
    assert_eq!(body["strategy"], "acme-platform/default");
}

#[tokio::test]
async fn missing_authorization_header_returns_unauthenticated() {
    let iss = "https://idp.test";
    let (jwk, _enc) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk).await;
    let app = build_authenticated_router(iss, None, jwks_url).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "UNAUTHENTICATED");
}

#[tokio::test]
async fn expired_token_returns_invalid_token() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk).await;
    let app = build_authenticated_router(iss, None, jwks_url).await;

    // exp_delta well past the configured 30s clock_skew.
    let claims = ClaimsBuilder::new(iss, "alice").exp_delta(-3600).build();
    let token = mint(claims, "k1", &enc);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "INVALID_TOKEN");
}

#[tokio::test]
async fn wrong_issuer_returns_invalid_token() {
    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk).await;
    let app = build_authenticated_router(iss, None, jwks_url).await;

    let claims = ClaimsBuilder::new("https://evil.test", "alice").build();
    let token = mint(claims, "k1", &enc);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "INVALID_TOKEN");
}

#[tokio::test]
async fn wrong_audience_returns_invalid_token() {
    let iss = "https://idp.test";
    let aud = "velocity-api";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk).await;
    let app = build_authenticated_router(iss, Some(aud), jwks_url).await;

    let claims = ClaimsBuilder::new(iss, "alice").aud("other-service").build();
    let token = mint(claims, "k1", &enc);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "INVALID_TOKEN");
}

#[tokio::test]
async fn token_signed_by_wrong_key_returns_invalid_token() {
    let iss = "https://idp.test";
    let (jwk_trusted, _enc_trusted) = make_keypair("k1");
    let (_jwk_attacker, enc_attacker) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk_trusted).await;
    let app = build_authenticated_router(iss, None, jwks_url).await;

    // Attacker mints a token claiming the trusted kid but with their key.
    let claims = ClaimsBuilder::new(iss, "alice").build();
    let token = mint(claims, "k1", &enc_attacker);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "INVALID_TOKEN");
}

#[tokio::test]
async fn healthz_bypasses_auth_layer() {
    let iss = "https://idp.test";
    let (jwk, _enc) = make_keypair("k1");
    let (jwks_url, _fix, _srv) = spawn_jwks(jwk).await;
    let app = build_authenticated_router(iss, None, jwks_url).await;

    let req = Request::builder().method("GET").uri("/healthz").body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
