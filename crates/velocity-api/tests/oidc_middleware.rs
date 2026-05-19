#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! OIDC middleware path: cookie → session lookup → claim mapping → Identity.
//!
//! `/auth/callback` ultimately persists the ID-token claims into the
//! [`SessionStore`] and hands the user a `velocity_session=<uuid>` cookie.
//! These tests seed that row directly via [`MockSessionStore`] so the
//! middleware path can be proven *independently* of the redirect dance.
//! This is the second half of task #34's acceptance criterion ("subsequent
//! requests with session cookie attach Identity").

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Extension, Json, Router};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use velocity_api::auth::{
    authenticate, AuthRegistry, AuthState, JwksCache, MockSessionStore, ResolvedAuthStrategy,
    SessionRecord, SESSION_COOKIE_NAME,
};
use velocity_api::registry::ResolvedSchema;
use velocity_api::{Identity, SchemaRegistry};
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::auth::{
    AuthStrategyConfig, AuthStrategySpec, AuthStrategyType, ClaimMapping,
    IssuerConfig as CrdIssuer, OidcConfig, SecretRef,
};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec,
    SearchTier,
};

const ISSUER: &str = "https://idp.test";
const STRATEGY_NS: &str = "acme-platform";
const STRATEGY_NAME: &str = "default";

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

/// Minimal OIDC strategy spec: one issuer (claims map sub→actor_id,
/// scope→roles) plus an `oidc` block so the middleware's defensive
/// "kind: oidc but no oidc block" check doesn't trip.
fn strategy_spec() -> AuthStrategySpec {
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
        token_endpoint: "https://idp.test/token".into(),
        userinfo_endpoint: None,
        client_id: "vel-client".into(),
        client_secret_ref: SecretRef { name: "x".into(), key: "y".into() },
        redirect_uri: "https://api.test/auth/callback".into(),
        scopes: vec![],
        issuer: ISSUER.into(),
        session_ttl: None,
    };
    AuthStrategySpec {
        kind: AuthStrategyType::Oidc,
        config: AuthStrategyConfig {
            issuers: vec![CrdIssuer {
                issuer: ISSUER.into(),
                // jwks_url unused on the middleware path (no token verify),
                // but ResolvedAuthStrategy requires the issuer key be
                // present so claim mapping is hooked up.
                jwks_url: "http://unused.test/jwks.json".into(),
                audience: None,
                claims,
            }],
            oidc: Some(oidc),
            clock_skew: Some(30),
            ..Default::default()
        },
    }
}

/// Build a router with one schema + an OIDC strategy + an injected
/// MockSessionStore. The caller seeds the session store before firing
/// requests.
fn build_router(store: Arc<MockSessionStore>) -> Router {
    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));

    let strategies = AuthRegistry::new();
    let strategy_ref = NamespacedRef { name: STRATEGY_NAME.into(), namespace: STRATEGY_NS.into() };
    let resolved = ResolvedAuthStrategy::from_spec(&strategy_ref, strategy_spec());
    strategies.upsert(resolved.clone());

    let auth_state = AuthState::new(schemas, strategies, JwksCache::new())
        .with_sessions(store as Arc<dyn velocity_api::auth::SessionStore>);
    auth_state.prime_strategy(&resolved).unwrap();

    Router::new()
        .route("/api/{org}/{app}/{domain}/{object}/{version}", get(echo_identity))
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

fn seed_session(store: &MockSessionStore, claims: Value) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let now = Utc::now();
    store.insert(SessionRecord {
        id,
        actor_id: claims["sub"].as_str().unwrap_or("unknown").to_string(),
        issuer: ISSUER.into(),
        id_token_claims: claims,
        created_at: now,
        expires_at: now + chrono::Duration::hours(1),
    });
    id
}

#[tokio::test]
async fn valid_session_cookie_attaches_identity() {
    let store = Arc::new(MockSessionStore::new());
    let id = seed_session(&store, json!({ "sub": "ravi", "scope": "read:po write:po" }));
    let app = build_router(store);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("cookie", format!("{SESSION_COOKIE_NAME}={id}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "ravi");
    assert_eq!(body["roles"], json!(["read:po", "write:po"]));
    assert_eq!(body["issuer"], ISSUER);
    assert_eq!(body["strategy"], format!("{STRATEGY_NS}/{STRATEGY_NAME}"));
}

#[tokio::test]
async fn missing_session_cookie_returns_unauthenticated() {
    let store = Arc::new(MockSessionStore::new());
    let app = build_router(store);

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
async fn malformed_session_cookie_returns_unauthenticated() {
    let store = Arc::new(MockSessionStore::new());
    let app = build_router(store);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("cookie", format!("{SESSION_COOKIE_NAME}=not-a-uuid"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_session_returns_unauthenticated() {
    let store = Arc::new(MockSessionStore::new());
    let app = build_router(store);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("cookie", format!("{SESSION_COOKIE_NAME}={}", uuid::Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revoked_session_returns_unauthenticated() {
    let store = Arc::new(MockSessionStore::new());
    let id = seed_session(&store, json!({ "sub": "ravi" }));
    // Revoke directly via the store (POST /auth/logout is exercised in
    // unit tests; here we just need the post-condition).
    velocity_api::auth::SessionStore::revoke(&*store, id).await.unwrap();

    let app = build_router(store);
    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("cookie", format!("{SESSION_COOKIE_NAME}={id}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn session_cookie_alongside_other_cookies_is_extracted() {
    // The cookie parser must pick out `velocity_session` from a multi-
    // cookie header. Browsers concatenate everything that matches the
    // domain into a single `Cookie:` line.
    let store = Arc::new(MockSessionStore::new());
    let id = seed_session(&store, json!({ "sub": "ravi", "scope": "read:po" }));
    let app = build_router(store);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("cookie", format!("other_cookie=foo; {SESSION_COOKIE_NAME}={id}; trailing=bar"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "ravi");
}

#[tokio::test]
async fn bearer_header_on_oidc_strategy_is_ignored() {
    // OIDC strategies are cookie-based, not Bearer-based — a stray
    // Authorization header MUST NOT be parsed as a JWT. The middleware
    // dispatches on `leaf.kind`, so a Bearer on an OIDC route falls
    // through to the cookie check; with no cookie present, that's a 401.
    let store = Arc::new(MockSessionStore::new());
    let app = build_router(store);

    let req = Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header("authorization", "Bearer some.jwt.token")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
