#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 2a end-to-end — JWT round-trip + Redis revocation + audit fail-mode.
//!
//! Drives the *full* HTTP stack: real Axum router with auth middleware in
//! front of the production handlers, against a real Postgres so the audit
//! row that the data-write transaction emits actually lands and can be
//! verified.
//!
//! Run with:
//!     VELOCITY_API_TEST_PG_URL='postgres://postgres:postgres@localhost:5434/velocity' \
//!     cargo test -p velocity-api --test phase2a_e2e
//!
//! Three scenarios, each in its own test so a Postgres state leak between
//! them is easy to spot:
//!
//! 1. happy path → 201, audit row with `fail_modes.revocation = "allowed"`
//! 2. revoked actor → 403 ACTOR_REVOKED, **no** audit row written
//! 3. revocation backend down + `failOpen: true` → 201,
//!    `fail_modes.revocation = "backend_down_admitted"` on the audit row
//!
//! ADR-003 is the load-bearing decision being tested here. The fail-mode
//! strings on the audit row are what Grafana keys off to flag bursts of
//! admitted-but-unverified traffic; if the auth middleware silently
//! collapses the cases, that signal is lost downstream.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::BodyExt;
use jsonwebtoken::{encode, EncodingKey, Header};
use rsa::pkcs8::EncodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use velocity_core::auth::{
    authenticate, AuthRegistry, AuthState, JwksCache, MockChecker, ResolvedAuthStrategy,
};
use velocity_core::registry::ResolvedSchema;
use velocity_data_api::router;
use velocity_core::SchemaRegistry;
use velocity_data_api::DataState;
use velocity_operator::PostgresProvisioner;
use velocity_types::common::{NamespacedRef, SchemaPath};
use velocity_types::crds::auth::{
    AuthStrategySpec, AuthStrategyType, ClaimMapping, IssuerConfig as CrdIssuer, RevocationConfig,
};
use velocity_types::crds::schema::{
    AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
    SearchSpec, SearchTier,
};

// ─── env shims (mirror phase1_crud.rs) ────────────────────────────────────

fn admin_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_PG_URL")
        .ok()
        .or_else(|| std::env::var("VELOCITY_OPERATOR_PG_URL").ok())
}

fn api_url() -> Option<String> {
    std::env::var("VELOCITY_API_TEST_API_URL")
        .ok()
        .or_else(|| Some("postgres://velocity_api:velocity_api_dev@localhost:5434/velocity".into()))
}

// ─── JWKS sidecar ──────────────────────────────────────────────────────────

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
    body: Arc<TokioMutex<Value>>,
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
    let n = now() as i64;
    let claims = json!({ "iss": iss, "sub": sub, "iat": n, "exp": n + 3600 });
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.into());
    encode(&header, &claims, key).unwrap()
}

// ─── DB + schema fixtures ──────────────────────────────────────────────────

fn field(name: &str, kind: FieldKind) -> FieldSpec {
    let mut f: FieldSpec =
        serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
    f.kind = kind;
    f
}

fn schema_spec() -> SchemaDefinitionSpec {
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
        // Open schema for this e2e — Layer-1 RBAC is exercised by
        // `tests/rbac_routes.rs`. Here we want auth + audit, not RBAC,
        // and a closed schema would force us to mint roles too.
        access: AccessSpec::default(),
        fields: vec![field("po_number", FieldKind::String)],
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
    let claims =
        ClaimMapping { actor_id: Some(Value::String("$.sub".into())), ..Default::default() };
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

async fn cleanup(admin: &PgPool, pg_schema: &str) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE")).execute(admin).await;
    for role in
        [format!("{pg_schema}_reader"), format!("{pg_schema}_writer"), format!("{pg_schema}_admin")]
    {
        let _ = sqlx::query(&format!("DROP ROLE IF EXISTS {role}")).execute(admin).await;
    }
}

struct Harness {
    admin_pool: PgPool,
    api_pool: PgPool,
    pg_schema: String,
    path: SchemaPath,
}

async fn setup_db(org: &str) -> Option<Harness> {
    let admin_url = admin_url()?;
    let api_url = api_url()?;
    let admin_pool = PgPoolOptions::new().max_connections(4).connect(&admin_url).await.unwrap();
    let api_pool = PgPoolOptions::new().max_connections(4).connect(&api_url).await.unwrap();
    let pg_schema = format!("{org}_supply_chain_procurement");
    cleanup(&admin_pool, &pg_schema).await;

    let prov = PostgresProvisioner::new(admin_pool.clone());
    prov.sync_domain(org, "supply-chain", "procurement").await.unwrap();
    let path = SchemaPath::new(org, "supply-chain", "procurement", "purchase-order", "v1");
    let plan = velocity_operator::build_ddl(&schema_spec(), &path).unwrap();
    prov.sync_schema_tables(&plan, false).await.unwrap();

    Some(Harness { admin_pool, api_pool, pg_schema, path })
}

/// Build the production router with auth middleware in front of it. The
/// auth state holds the schema registry, the auth registry, the JWKS
/// cache, and the (mock) revocation checker.
async fn build_authed_router(
    h: &Harness,
    iss: &str,
    jwks_url: &str,
    fail_open: bool,
    checker: MockChecker,
) -> Router {
    let (schemas, _ready) = SchemaRegistry::new();
    schemas.upsert(ResolvedSchema::from_spec(h.path.clone(), schema_spec()));

    let strategies = AuthRegistry::new();
    let strategy_ref = NamespacedRef { name: "default".into(), namespace: "acme-platform".into() };
    let resolved =
        ResolvedAuthStrategy::from_spec(&strategy_ref, strategy_spec(iss, jwks_url, fail_open));
    let jwks = JwksCache::new();
    resolved.prime_jwks(&jwks).await;
    strategies.upsert(resolved.clone());

    let auth_state =
        AuthState::new(Arc::clone(&schemas), strategies, jwks).with_revocation(Arc::new(checker));
    auth_state.prime_strategy(&resolved).unwrap();

    let app_state = DataState::new(Arc::clone(&schemas), h.api_pool.clone());
    router::build(app_state).layer(from_fn_with_state(auth_state, authenticate))
}

async fn read_body(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn post_create(token: &str, path: &SchemaPath, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!(
            "/api/{}/{}/{}/{}/{}",
            path.org, path.app, path.domain, path.object, path.version
        ))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Reads the most recent audit row for `actor` and returns the fail_modes
/// JSONB. Asserts exactly one row to guard against a regression where
/// audit_insert is called more than once per request.
async fn read_audit_fail_modes(admin: &PgPool, actor: &str) -> Value {
    let row: (Value, String) =
        sqlx::query_as("SELECT fail_modes, action FROM platform.audit_log WHERE actor = $1")
            .bind(actor)
            .fetch_one(admin)
            .await
            .expect("audit row");
    assert_eq!(row.1, "create", "this test only audits creates");
    row.0
}

async fn audit_row_count_for(admin: &PgPool, actor: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM platform.audit_log WHERE actor = $1")
        .bind(actor)
        .fetch_one(admin)
        .await
        .unwrap_or(0)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_writes_audit_row_with_allowed_fail_mode() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured (set VELOCITY_API_TEST_PG_URL)");
        return;
    };

    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    let app = build_authed_router(&h, iss, &jwks_url, /*fail_open=*/ false, checker).await;

    let actor = format!("alice-{suffix}");
    let token = mint(iss, &actor, "k1", &enc);
    let res =
        app.oneshot(post_create(&token, &h.path, json!({ "po_number": "PO-0001" }))).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "happy path must admit");
    let body = read_body(res.into_body()).await;
    assert_eq!(body["po_number"], "PO-0001");

    let fail_modes = read_audit_fail_modes(&h.admin_pool, &actor).await;
    // ADR-003 — the happy case still records the fail-mode field so audit
    // can prove the revocation backend was queried on every admit. If a
    // refactor drops this, change-data captures of audit chains will go
    // backwards-incompatible with Grafana dashboards.
    assert_eq!(fail_modes["auth"], "verified");
    assert_eq!(fail_modes["revocation"], "allowed");
    assert_eq!(fail_modes["revocation_fail_open"], false);
    assert_eq!(fail_modes["strategy"], "acme-platform/default");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn revoked_actor_is_rejected_and_writes_no_audit_row() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured");
        return;
    };

    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    let actor = format!("mallory-{suffix}");
    checker.revoke(&actor);
    let app = build_authed_router(&h, iss, &jwks_url, /*fail_open=*/ false, checker).await;

    let token = mint(iss, &actor, "k1", &enc);
    let res =
        app.oneshot(post_create(&token, &h.path, json!({ "po_number": "PO-0001" }))).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = read_body(res.into_body()).await;
    assert_eq!(body["error"], "ACTOR_REVOKED");

    // No audit row — the request never reached the handler. Important:
    // an audit row for a rejected request would imply we ran the data
    // write, which is exactly the failure mode revocation is meant to
    // prevent.
    let n = audit_row_count_for(&h.admin_pool, &actor).await;
    assert_eq!(n, 0, "revoked request must not write a data row or an audit row");

    cleanup(&h.admin_pool, &h.pg_schema).await;
}

#[tokio::test]
async fn backend_down_fail_open_admits_and_records_decision() {
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let org = format!("acme{suffix}");
    let Some(h) = setup_db(&org).await else {
        eprintln!("skipping: Postgres not configured");
        return;
    };

    let iss = "https://idp.test";
    let (jwk, enc) = make_keypair("k1");
    let (jwks_url, _srv) = spawn_jwks(jwk).await;
    let checker = MockChecker::new();
    checker.set_failing(true);
    let app = build_authed_router(&h, iss, &jwks_url, /*fail_open=*/ true, checker).await;

    let actor = format!("oscar-{suffix}");
    let token = mint(iss, &actor, "k1", &enc);
    let res =
        app.oneshot(post_create(&token, &h.path, json!({ "po_number": "PO-0001" }))).await.unwrap();
    // fail_open admits — but the audit row makes it discoverable later.
    assert_eq!(res.status(), StatusCode::CREATED);

    let fail_modes = read_audit_fail_modes(&h.admin_pool, &actor).await;
    assert_eq!(fail_modes["revocation"], "backend_down_admitted");
    assert_eq!(fail_modes["revocation_fail_open"], true);

    cleanup(&h.admin_pool, &h.pg_schema).await;
}
