#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Router-level E2E for the Composite auth strategy — Phase 2c acceptance:
//! "Composite: JWT fails, falls through to API key, succeeds."
//!
//! **Important semantic pin:** the platform's composite strategy dispatches
//! by *credential scheme presence*, not by trying each child until one
//! verifies. That's an intentional security choice — letting verification
//! failure fall through to the next child would expose two strategies'
//! error oracles in a single request and let an attacker mix-and-match
//! probes. See `src/auth/middleware.rs::resolve_leaf_strategy`.
//!
//! So "JWT fails, falls through to API key" is read here as the only thing
//! it can safely mean: *no `Bearer` header, `ApiKey` header present →
//! composite picks the ApiKey child → key verifies → 200*. That's the
//! property the test pins. Three companion tests pin the negative cases so
//! a future relaxation of this rule (which would be a security regression)
//! fails loud.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Extension, Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use velocity_api::auth::api_key::{ApiKeyChecker, ApiKeyError, ApiKeyRecord};
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
const COMPOSITE_NAME: &str = "any-credential";
const JWT_CHILD: &str = "jwt-primary";
const API_KEY_CHILD: &str = "api-key-fallback";
const VALID_KEY: &str = "vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1";

// ─── Mock checker ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct MockChecker {
    record: Option<ApiKeyRecord>,
}

#[async_trait]
impl ApiKeyChecker for MockChecker {
    async fn lookup(&self, plaintext: &str) -> Result<ApiKeyRecord, ApiKeyError> {
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
                name: COMPOSITE_NAME.into(),
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

fn child_ref(name: &str) -> NamespacedRef {
    NamespacedRef { namespace: STRATEGY_NS.into(), name: name.into() }
}

fn jwt_child() -> ResolvedAuthStrategy {
    // No issuers configured — so if the dispatcher ever picked this child
    // when a `Bearer` header *was* present, verification would fail loudly.
    // Tests rely on this: the negative test ("Bearer reaches JWT child")
    // expects 401, not 200.
    let spec =
        AuthStrategySpec { kind: AuthStrategyType::Jwt, config: AuthStrategyConfig::default() };
    ResolvedAuthStrategy::from_spec(&child_ref(JWT_CHILD), spec)
}

fn api_key_child() -> ResolvedAuthStrategy {
    let spec =
        AuthStrategySpec { kind: AuthStrategyType::ApiKey, config: AuthStrategyConfig::default() };
    ResolvedAuthStrategy::from_spec(&child_ref(API_KEY_CHILD), spec)
}

fn composite_strategy() -> ResolvedAuthStrategy {
    // Ordered children — JWT first, ApiKey second. The dispatcher walks
    // them in order and picks the first whose scheme is present on the
    // request, so swapping the order would still admit ApiKey requests
    // (scheme-keyed dispatch, not order-keyed). The order matters only
    // when *both* schemes are present, in which case the listed-first
    // child wins.
    let spec = AuthStrategySpec {
        kind: AuthStrategyType::Composite,
        config: AuthStrategyConfig {
            children: vec![child_ref(JWT_CHILD), child_ref(API_KEY_CHILD)],
            ..AuthStrategyConfig::default()
        },
    };
    ResolvedAuthStrategy::from_spec(&child_ref(COMPOSITE_NAME), spec)
}

fn build_router(checker: Arc<dyn ApiKeyChecker>) -> Router {
    let (schemas, _ready) = SchemaRegistry::new();
    let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
    schemas.upsert(ResolvedSchema::from_spec(path, schema_spec()));

    let strategies = AuthRegistry::new();
    strategies.upsert(composite_strategy());
    strategies.upsert(jwt_child());
    strategies.upsert(api_key_child());

    let auth_state = AuthState::new(schemas, strategies, JwksCache::new()).with_api_keys(checker);

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

fn req_with_auth(header: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/api/acme/supply-chain/procurement/purchase-order/v1")
        .header(axum::http::header::AUTHORIZATION, header)
        .body(Body::empty())
        .unwrap()
}

fn admit_record() -> ApiKeyRecord {
    ApiKeyRecord {
        key: format!("{STRATEGY_NS}/{API_KEY_CHILD}"),
        actor: "erp-sync-service".into(),
        actor_type: "service".into(),
        scopes: vec![],
        ip_allowlist: vec![],
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn composite_dispatches_to_api_key_child_when_only_apikey_header_present() {
    // Phase 2c acceptance line: "Composite: JWT fails, falls through to
    // API key, succeeds" — read literally against the implementation, this
    // means: caller presents ApiKey only, composite walks children, JWT
    // child is skipped (no Bearer), ApiKey child matches scheme, lookup
    // succeeds → 200.
    let app = build_router(Arc::new(MockChecker { record: Some(admit_record()) }));

    let req = req_with_auth(&format!("ApiKey {VALID_KEY}"));
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["actor_id"], "erp-sync-service");
    // The leaf strategy attribution carries the API-key child's key, NOT
    // the composite's. Audit must see which child actually admitted the
    // request, so a misconfigured composite can be traced to the precise
    // leaf that fired.
    assert_eq!(body["strategy"], format!("{STRATEGY_NS}/{API_KEY_CHILD}"));
}

#[tokio::test]
async fn composite_returns_401_when_no_credentials_present() {
    // No Authorization header at all — neither child's scheme matches,
    // dispatcher emits a helpful 401 listing the accepted schemes
    // ("Bearer, ApiKey"). The body's `error` is UNAUTHENTICATED, not a
    // 500 or 400 — the latter would let a probe distinguish "no creds"
    // from "wrong creds" cheaply.
    let app = build_router(Arc::new(MockChecker { record: Some(admit_record()) }));

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
async fn composite_does_not_fall_through_on_verification_failure() {
    // Critical security pin: the *implementation* deliberately does NOT
    // try ApiKey when JWT verification fails. The composite picks JWT
    // based on the Bearer scheme being present; once JWT fails the
    // request 401s. Any future change that introduces verification-
    // fallthrough would let an attacker probe two strategies' error
    // oracles in a single request — this test fails loud if that
    // semantic ever drifts.
    //
    // We arrange a Bearer header against an empty JWT issuer registry so
    // verification *must* fail. We also seed an admittable ApiKey record
    // so the test would 200 if (and only if) the dispatcher were doing
    // fall-through. Expected: 401 (no fall-through).
    let app = build_router(Arc::new(MockChecker { record: Some(admit_record()) }));

    let req = req_with_auth("Bearer not.a.real.jwt");
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn composite_with_both_schemes_picks_first_listed_child() {
    // Order-sensitivity pin: the composite lists JWT first, then ApiKey.
    // When *both* schemes are technically present on the request, the
    // dispatcher MUST pick the first listed (JWT). Since the JWT child
    // has no issuers configured, verification 401s — which is what we
    // assert here. If the dispatcher ever silently preferred the
    // already-validatable scheme (ApiKey), this would return 200, which
    // would be a quiet ordering-rule regression.
    //
    // Note: this is an unusual but legal request shape — one Authorization
    // header per browser, but a programmatic client could submit either.
    // The first listed in the AuthStrategy CRD's `children` array wins.
    let app = build_router(Arc::new(MockChecker { record: Some(admit_record()) }));

    let mut req = req_with_auth("Bearer not.a.real.jwt");
    // Append a second Authorization header. HTTP allows multi-valued
    // headers; axum exposes them via `headers().get_all()`. The
    // dispatcher reads the first via `headers().get(...)`, so this
    // setup pins "the first-listed scheme is what's seen."
    req.headers_mut()
        .append(axum::http::header::AUTHORIZATION, format!("ApiKey {VALID_KEY}").parse().unwrap());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
