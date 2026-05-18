#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Tower-driven E2E for the OIDC redirect flow.
//!
//! Drives the *first* half of task #34's acceptance criterion — "browser
//! end-to-end redirect flow succeeds" — against an in-process fake IdP
//! (JWKS endpoint + token endpoint) so the test doesn't need a network
//! IdP. The second half ("subsequent requests with session cookie
//! attach Identity") is covered in `oidc_middleware.rs`.
//!
//! Flow exercised:
//!
//! 1. `GET /auth/login/{ns}/{name}` → 302, Set-Cookie velocity_oidc_flow,
//!    Location → fake IdP authorize URL with `state` + `nonce` query
//!    params we capture here.
//! 2. Test pins the captured `nonce` on the fake token endpoint so the
//!    minted ID token includes it.
//! 3. `GET /auth/callback?code=...&state=...` + flow cookie → 302,
//!    Set-Cookie velocity_session, Location → flow.return_to.
//! 4. `GET /api/...` + velocity_session cookie → 200 with the Identity
//!    derived from the ID-token claims.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::BodyExt;
use jsonwebtoken::{encode, EncodingKey, Header};
use parking_lot::Mutex;
use rsa::pkcs8::EncodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tower::ServiceExt;
use velocity_api::auth::{
    authenticate, AuthRegistry, AuthState, JwksCache, MockSessionStore, ResolvedAuthStrategy,
    SessionStore,
};
use velocity_api::auth_handlers::{AuthHandlersState, StaticClientSecretResolver};
use velocity_api::registry::ResolvedSchema;
use velocity_api::router as api_router;
use velocity_api::{Identity, SchemaRegistry};
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::auth::{
    AuthStrategyConfig, AuthStrategySpec, AuthStrategyType, ClaimMapping, IssuerConfig as CrdIssuer,
    OidcConfig, SecretRef,
};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
};

const ISSUER: &str = "https://idp.test";
const CLIENT_ID: &str = "vel-client";
const CLIENT_SECRET: &str = "vel-client-secret";
const KID: &str = "k1";
const STRATEGY_NS: &str = "acme-platform";
const STRATEGY_NAME: &str = "default";

// ─── Fake IdP fixtures ─────────────────────────────────────────────────────

fn make_keypair() -> (Value, EncodingKey) {
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pub_key = priv_key.to_public_key();
    let n = URL_SAFE_NO_PAD.encode(pub_key.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(pub_key.e().to_bytes_be());
    let jwk = json!({
        "kty": "RSA",
        "use": "sig",
        "alg": "RS256",
        "kid": KID,
        "n": n,
        "e": e,
    });
    let pem = priv_key.to_pkcs8_pem(Default::default()).unwrap().to_string();
    let enc_key = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
    (jwk, enc_key)
}

#[derive(Clone)]
struct JwksFixture {
    body: Arc<parking_lot::Mutex<Value>>,
}

async fn jwks_handler(State(state): State<JwksFixture>) -> Json<Value> {
    Json(state.body.lock().clone())
}

async fn spawn_jwks(jwk: Value) -> (String, JoinHandle<()>) {
    let fixture = JwksFixture { body: Arc::new(Mutex::new(json!({ "keys": [jwk] }))) };
    let app = Router::new().route("/jwks.json", get(jwks_handler)).with_state(fixture);
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/jwks.json"), task)
}

/// What the fake token endpoint will mint into the next ID token.
/// The test pins `nonce` (and optionally extra claims) just before
/// driving `/auth/callback` so the issued token survives the nonce check.
#[derive(Clone)]
struct TokenFixture {
    enc_key: Arc<EncodingKey>,
    nonce: Arc<Mutex<Option<String>>>,
    extra_claims: Arc<Mutex<serde_json::Map<String, Value>>>,
    expected_client_secret: String,
    expected_redirect_uri: String,
}

#[derive(Debug, serde::Deserialize)]
struct TokenForm {
    grant_type: String,
    code: String,
    redirect_uri: String,
    code_verifier: String,
}

async fn token_handler(
    State(state): State<TokenFixture>,
    headers: axum::http::HeaderMap,
    Form(form): Form<TokenForm>,
) -> impl IntoResponse {
    // RFC 6749 §2.3.1 — client_secret_basic.
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(basic) = auth.strip_prefix("Basic ") else {
        return (StatusCode::UNAUTHORIZED, "expected Basic auth").into_response();
    };
    let decoded = base64::engine::general_purpose::STANDARD.decode(basic).unwrap_or_default();
    let decoded = String::from_utf8(decoded).unwrap_or_default();
    let Some((client_id, client_secret)) = decoded.split_once(':') else {
        return (StatusCode::UNAUTHORIZED, "malformed Basic").into_response();
    };
    if client_id != CLIENT_ID || client_secret != state.expected_client_secret {
        return (StatusCode::UNAUTHORIZED, "bad client credentials").into_response();
    }
    if form.grant_type != "authorization_code" {
        return (StatusCode::BAD_REQUEST, "unexpected grant_type").into_response();
    }
    if form.redirect_uri != state.expected_redirect_uri {
        return (StatusCode::BAD_REQUEST, "unexpected redirect_uri").into_response();
    }
    if form.code.is_empty() || form.code_verifier.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing code or verifier").into_response();
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let nonce = state.nonce.lock().clone();
    let extras = state.extra_claims.lock().clone();

    let mut claims = serde_json::Map::new();
    claims.insert("iss".into(), Value::String(ISSUER.into()));
    claims.insert("aud".into(), Value::String(CLIENT_ID.into()));
    claims.insert("sub".into(), Value::String("ravi".into()));
    claims.insert("iat".into(), json!(now));
    claims.insert("exp".into(), json!(now + 3600));
    if let Some(n) = nonce {
        claims.insert("nonce".into(), Value::String(n));
    }
    for (k, v) in extras {
        claims.insert(k, v);
    }

    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let id_token = encode(&header, &Value::Object(claims), &state.enc_key).unwrap();

    let body = json!({
        "access_token": "fake-access",
        "id_token": id_token,
        "token_type": "Bearer",
        "expires_in": 3600,
    });
    (StatusCode::OK, Json(body)).into_response()
}

async fn spawn_token_endpoint(
    enc_key: EncodingKey,
    expected_redirect_uri: String,
) -> (String, TokenFixture, JoinHandle<()>) {
    let fixture = TokenFixture {
        enc_key: Arc::new(enc_key),
        nonce: Arc::new(Mutex::new(None)),
        extra_claims: Arc::new(Mutex::new(serde_json::Map::new())),
        expected_client_secret: CLIENT_SECRET.into(),
        expected_redirect_uri,
    };
    let app = Router::new().route("/token", post(token_handler)).with_state(fixture.clone());
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/token"), fixture, task)
}

// ─── Velocity-side fixtures ────────────────────────────────────────────────

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

fn strategy_spec(jwks_url: String, token_endpoint: String, redirect_uri: String) -> AuthStrategySpec {
    let claims = ClaimMapping {
        actor_id: Some(Value::String("$.sub".into())),
        roles: Some(json!({
            "path": "$.scope",
            "transform": { "type": "scope_to_roles" }
        })),
        ..Default::default()
    };
    let oidc = OidcConfig {
        authorization_endpoint: "https://idp.test/authorize".into(),
        token_endpoint,
        userinfo_endpoint: None,
        client_id: CLIENT_ID.into(),
        client_secret_ref: SecretRef { name: "x".into(), key: "y".into() },
        redirect_uri,
        scopes: vec![],
        issuer: ISSUER.into(),
        session_ttl: Some(3600),
    };
    AuthStrategySpec {
        kind: AuthStrategyType::Oidc,
        config: AuthStrategyConfig {
            issuers: vec![CrdIssuer {
                issuer: ISSUER.into(),
                jwks_url,
                audience: None,
                claims,
            }],
            oidc: Some(oidc),
            clock_skew: Some(30),
            ..Default::default()
        },
    }
}

struct TestApp {
    router: Router,
    #[allow(dead_code)]
    sessions: Arc<MockSessionStore>,
}

async fn build_app(
    jwks_url: String,
    token_endpoint: String,
    redirect_uri: String,
    client_secret: String,
) -> TestApp {
    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));

    let strategies = AuthRegistry::new();
    let strategy_ref =
        NamespacedRef { name: STRATEGY_NAME.into(), namespace: STRATEGY_NS.into() };
    let resolved = ResolvedAuthStrategy::from_spec(
        &strategy_ref,
        strategy_spec(jwks_url, token_endpoint, redirect_uri),
    );

    let jwks = JwksCache::new();
    resolved.prime_jwks(&jwks).await;
    strategies.upsert(resolved.clone());

    let sessions: Arc<MockSessionStore> = Arc::new(MockSessionStore::new());

    let auth_state =
        AuthState::new(schemas, strategies.clone(), jwks.clone())
            .with_sessions(sessions.clone() as Arc<dyn SessionStore>);
    auth_state.prime_strategy(&resolved).unwrap();

    let handlers_state = AuthHandlersState {
        auth_registry: strategies,
        sessions: sessions.clone() as Arc<dyn SessionStore>,
        flow_cookie_key: Arc::new(b"velocity-test-flow-cookie-hmac-key-32b".to_vec()),
        jwks,
        claim_mappings: auth_state.claim_mappings.clone(),
        http: reqwest::Client::new(),
        client_secret_resolver: Arc::new(
            StaticClientSecretResolver::default()
                .with(&format!("{STRATEGY_NS}/{STRATEGY_NAME}"), &client_secret),
        ),
    };

    let api = Router::new()
        .route(
            "/api/{org}/{app}/{domain}/{object}/{version}",
            get(echo_identity),
        )
        .layer(from_fn_with_state(auth_state, authenticate));

    let router = api.merge(api_router::build_auth(handlers_state));
    TestApp { router, sessions }
}

async fn echo_identity(identity: Option<Extension<Identity>>) -> impl IntoResponse {
    match identity {
        Some(Extension(id)) => (
            StatusCode::OK,
            Json(json!({
                "actor_id": id.actor_id,
                "roles": id.roles,
                "issuer": id.issuer,
                "strategy": id.strategy,
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

/// Pull `velocity_oidc_flow=...` out of a `Set-Cookie` header on the
/// /auth/login response. axum's HeaderMap exposes append-many, so we
/// iterate.
fn extract_flow_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    for value in headers.get_all(header::SET_COOKIE) {
        let s = value.to_str().ok()?;
        if let Some(rest) = s.strip_prefix("velocity_oidc_flow=") {
            let value = rest.split(';').next().unwrap_or("");
            return Some(value.to_string());
        }
    }
    None
}

/// Pull `velocity_session=...` out of a `Set-Cookie` header on the
/// /auth/callback response.
fn extract_session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    for value in headers.get_all(header::SET_COOKIE) {
        let s = value.to_str().ok()?;
        if let Some(rest) = s.strip_prefix("velocity_session=") {
            let value = rest.split(';').next().unwrap_or("");
            // Skip the clear directive (Max-Age=0 / empty value) on logout-shaped headers.
            if value.is_empty() {
                continue;
            }
            return Some(value.to_string());
        }
    }
    None
}

/// Parse `?foo=...&state=...` out of a Location URL and return the
/// query parameters as a map.
fn parse_query(url: &str) -> HashMap<String, String> {
    let Some(q) = url.split_once('?').map(|(_, q)| q) else {
        return HashMap::new();
    };
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16).unwrap_or(0);
            let lo = (bytes[i + 2] as char).to_digit(16).unwrap_or(0);
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Decode the flow-cookie payload (base64url-no-pad JSON before the
/// `.signature` suffix) so the test can pin the nonce on the token
/// endpoint. The HMAC isn't verified here — the test is its own trust
/// boundary; the *handler* still verifies on the way back in.
fn decode_flow_payload(cookie: &str) -> Value {
    let (payload_b64, _sig) = cookie.split_once('.').unwrap();
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ─── Test ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn full_redirect_flow_issues_session_and_attaches_identity() {
    // 1. Fake IdP — JWKS + token endpoint with our RSA keypair.
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;

    // 2. Velocity router + auth state, wired to the fake IdP and a
    //    StaticClientSecretResolver that knows our client_secret.
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri.clone(), CLIENT_SECRET.into()).await;

    // 3. GET /auth/login/{ns}/{name}?return_to=/portal
    let login_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/login/{STRATEGY_NS}/{STRATEGY_NAME}?return_to=/portal"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(login_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FOUND, "login should 302");

    let headers = res.headers().clone();
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap().to_string();
    let flow_cookie = extract_flow_cookie(&headers).expect("flow cookie set on login");

    // Sanity-check the IdP redirect carries the expected params.
    let qs = parse_query(&location);
    assert_eq!(qs.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(qs.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert!(qs.contains_key("state"));
    assert!(qs.contains_key("nonce"));
    assert!(qs.contains_key("code_challenge"));

    // 4. Pin the cookie's nonce on the fake token endpoint so the minted
    //    id_token survives the nonce check.
    let flow_payload = decode_flow_payload(&flow_cookie);
    let nonce = flow_payload["nonce"].as_str().unwrap().to_string();
    let state_param = flow_payload["state"].as_str().unwrap().to_string();
    // Sanity: the state on the redirect URL matches the state pinned in
    // the cookie. If these ever drift, the round-trip can't work.
    assert_eq!(qs.get("state").map(String::as_str), Some(state_param.as_str()));
    *token_fixture.nonce.lock() = Some(nonce);
    // Also include `scope` so the claim mapping fills in `roles`.
    token_fixture
        .extra_claims
        .lock()
        .insert("scope".into(), Value::String("read:po write:po".into()));

    // 5. GET /auth/callback?code=...&state=... + Cookie: velocity_oidc_flow=...
    let callback_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/callback?code=fake-code&state={state_param}"))
        .header("cookie", format!("velocity_oidc_flow={flow_cookie}"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FOUND, "callback should 302");
    let headers = res.headers().clone();
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
    assert_eq!(location, "/portal", "callback should redirect to flow.return_to");
    let session_cookie =
        extract_session_cookie(&headers).expect("velocity_session cookie set on callback");

    // 6. GET /api/... with the session cookie → 200 + Identity.
    let api_req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("cookie", format!("velocity_session={session_cookie}"))
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(api_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "ravi");
    assert_eq!(body["roles"], json!(["read:po", "write:po"]));
    assert_eq!(body["issuer"], ISSUER);
    assert_eq!(body["strategy"], format!("{STRATEGY_NS}/{STRATEGY_NAME}"));
}

#[tokio::test]
async fn callback_rejects_when_nonce_mismatches() {
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri, CLIENT_SECRET.into()).await;

    // Drive login.
    let login_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/login/{STRATEGY_NS}/{STRATEGY_NAME}"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(login_req).await.unwrap();
    let headers = res.headers().clone();
    let flow_cookie = extract_flow_cookie(&headers).unwrap();
    let flow_payload = decode_flow_payload(&flow_cookie);
    let state_param = flow_payload["state"].as_str().unwrap().to_string();

    // Pin the WRONG nonce on the IdP so the id_token's nonce mismatches.
    *token_fixture.nonce.lock() = Some("attacker-replayed-nonce".into());

    let callback_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/callback?code=fake-code&state={state_param}"))
        .header("cookie", format!("velocity_oidc_flow={flow_cookie}"))
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn callback_rejects_when_id_token_aud_is_wrong() {
    // OIDC Core §3.1.3.7 step 3: the ID token's `aud` MUST match the
    // client_id from `oidc.client_id`. The API-call `IssuerConfig.audience`
    // is a *different* value (used when middleware validates Bearer JWTs),
    // and a future refactor that conflates the two would be a real
    // vulnerability. Pinning the behaviour here so that regression fails
    // loud.
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri, CLIENT_SECRET.into()).await;

    let login_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/login/{STRATEGY_NS}/{STRATEGY_NAME}"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(login_req).await.unwrap();
    let flow_cookie = extract_flow_cookie(res.headers()).unwrap();
    let flow_payload = decode_flow_payload(&flow_cookie);
    let state_param = flow_payload["state"].as_str().unwrap().to_string();
    let nonce = flow_payload["nonce"].as_str().unwrap().to_string();

    // Pin the nonce so it survives the nonce check — we want the audience
    // check to be what trips the failure.
    *token_fixture.nonce.lock() = Some(nonce);
    // Override `aud` to a value the validation MUST reject. The extras
    // loop in token_handler runs after the default `aud=CLIENT_ID`, so
    // this overwrites it.
    token_fixture
        .extra_claims
        .lock()
        .insert("aud".into(), Value::String("wrong-service".into()));

    let callback_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/callback?code=fake-code&state={state_param}"))
        .header("cookie", format!("velocity_oidc_flow={flow_cookie}"))
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn callback_rejects_on_bad_client_secret() {
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, _token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;

    // Resolver hands the callback a secret the IdP won't accept → token
    // endpoint returns 401, callback turns that into 401 too.
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri, "wrong-secret".into()).await;

    let login_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/login/{STRATEGY_NS}/{STRATEGY_NAME}"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(login_req).await.unwrap();
    let flow_cookie = extract_flow_cookie(res.headers()).unwrap();
    let flow_payload = decode_flow_payload(&flow_cookie);
    let state_param = flow_payload["state"].as_str().unwrap().to_string();

    let callback_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/callback?code=fake-code&state={state_param}"))
        .header("cookie", format!("velocity_oidc_flow={flow_cookie}"))
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn callback_rejects_when_idp_returns_error_param() {
    // No need for fake endpoints — the handler short-circuits on `?error=`
    // before touching anything else.
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, _token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri, CLIENT_SECRET.into()).await;

    let callback_req = Request::builder()
        .method("GET")
        .uri("/auth/callback?error=access_denied")
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn callback_rejects_when_state_mismatches_cookie() {
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, _token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri, CLIENT_SECRET.into()).await;

    let login_req = Request::builder()
        .method("GET")
        .uri(format!("/auth/login/{STRATEGY_NS}/{STRATEGY_NAME}"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(login_req).await.unwrap();
    let flow_cookie = extract_flow_cookie(res.headers()).unwrap();

    // Hand the callback a state value that doesn't match the cookie's
    // pinned state. decode_flow_cookie rejects with StateMismatch.
    let callback_req = Request::builder()
        .method("GET")
        .uri("/auth/callback?code=fake-code&state=tampered-state")
        .header("cookie", format!("velocity_oidc_flow={flow_cookie}"))
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn callback_rejects_when_flow_cookie_missing() {
    let (jwk, enc_key) = make_keypair();
    let (jwks_url, _jwks_task) = spawn_jwks(jwk).await;
    let redirect_uri = "http://localhost/auth/callback".to_string();
    let (token_endpoint, _token_fixture, _token_task) =
        spawn_token_endpoint(enc_key, redirect_uri.clone()).await;
    let TestApp { router, .. } =
        build_app(jwks_url, token_endpoint, redirect_uri, CLIENT_SECRET.into()).await;

    let callback_req = Request::builder()
        .method("GET")
        .uri("/auth/callback?code=fake-code&state=anything")
        .body(Body::empty())
        .unwrap();
    let res = router.oneshot(callback_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
