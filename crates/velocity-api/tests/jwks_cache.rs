#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! JWKS cache behaviour against a local axum sidecar that serves a
//! controllable JWKS document. Exercises cold-start, kid lookup, key
//! rotation, transient JWKS outages, and the kid-miss rate-limit.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use velocity_api::auth::jwks::{
    IssuerConfig, IssuerStatus, JwksCache, JwksError, KID_MISS_REFRESH_INTERVAL,
};

/// State shared between the JWKS server and the tests so each test can
/// swap the served JWKS body or trigger a 500.
#[derive(Clone, Default)]
struct JwksFixture {
    body: Arc<Mutex<Value>>,
    fail: Arc<Mutex<bool>>,
}

impl JwksFixture {
    async fn set_body(&self, v: Value) {
        *self.body.lock().await = v;
    }
    async fn set_fail(&self, fail: bool) {
        *self.fail.lock().await = fail;
    }
}

async fn jwks_handler(
    State(state): State<JwksFixture>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    if *state.fail.lock().await {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
    let body = state.body.lock().await.clone();
    Ok(Json(body))
}

/// Bind a tiny axum server on a free port and return `(url, fixture, task)`.
async fn spawn_jwks_server() -> (String, JwksFixture, JoinHandle<()>) {
    let fixture = JwksFixture::default();
    let app =
        axum::Router::new().route("/jwks.json", get(jwks_handler)).with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/jwks.json"), fixture, task)
}

/// Build a JWKS document containing one RSA public key with the given kid.
fn jwks_with_kid(kid: &str) -> Value {
    let mut rng = rand::thread_rng();
    let private = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let public = private.to_public_key();
    let n = URL_SAFE_NO_PAD.encode(public.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(public.e().to_bytes_be());
    json!({
        "keys": [
            {
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": kid,
                "n": n,
                "e": e,
            }
        ]
    })
}

fn jwks_with_two(kid1: &str, kid2: &str) -> Value {
    // Merging two single-key sets into one document.
    let a = jwks_with_kid(kid1);
    let b = jwks_with_kid(kid2);
    let mut keys = a["keys"].as_array().unwrap().clone();
    keys.extend(b["keys"].as_array().unwrap().iter().cloned());
    json!({ "keys": keys })
}

#[tokio::test]
async fn cold_start_success_marks_issuer_ready() {
    let (url, fixture, _server) = spawn_jwks_server().await;
    fixture.set_body(jwks_with_kid("k1")).await;

    let cache = JwksCache::new();
    let status = cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url,
    }).await;

    assert_eq!(status, IssuerStatus::Ready);
    assert_eq!(cache.key_count("https://idp.test"), 1);
    let jwk = cache.lookup("https://idp.test", "k1").await.expect("k1 present");
    assert_eq!(jwk.common.key_id.as_deref(), Some("k1"));
}

#[tokio::test]
async fn cold_start_failure_marks_issuer_pending() {
    let (url, fixture, _server) = spawn_jwks_server().await;
    fixture.set_fail(true).await;

    let cache = JwksCache::new();
    let status = cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url,
    }).await;
    assert_eq!(status, IssuerStatus::Pending);

    let err = cache.lookup("https://idp.test", "k1").await.unwrap_err();
    assert!(matches!(err, JwksError::IssuerUnavailable(_)), "{err:?}");
}

#[tokio::test]
async fn unknown_issuer_returns_unknown_issuer_error() {
    let cache = JwksCache::new();
    let err = cache.lookup("https://nope.test", "k1").await.unwrap_err();
    assert!(matches!(err, JwksError::UnknownIssuer(_)));
}

#[tokio::test]
async fn kid_miss_forces_one_refresh_then_succeeds() {
    let (url, fixture, _server) = spawn_jwks_server().await;
    // Cold-start serves only "k1".
    fixture.set_body(jwks_with_kid("k1")).await;
    let cache = JwksCache::new();
    cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url,
    }).await;
    assert_eq!(cache.key_count("https://idp.test"), 1);

    // IdP rotates: now serves both "k1" and "k2".
    fixture.set_body(jwks_with_two("k1", "k2")).await;

    // Looking up the new kid forces a refresh; succeeds.
    let jwk = cache.lookup("https://idp.test", "k2").await.expect("k2 reachable via forced refresh");
    assert_eq!(jwk.common.key_id.as_deref(), Some("k2"));
    assert_eq!(cache.key_count("https://idp.test"), 2);
}

#[tokio::test]
async fn kid_miss_refresh_is_rate_limited() {
    let (url, fixture, _server) = spawn_jwks_server().await;
    fixture.set_body(jwks_with_kid("k1")).await;
    let cache = JwksCache::new();
    cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url,
    }).await;

    // First unknown-kid lookup consumes the refresh budget.
    let err = cache.lookup("https://idp.test", "kX").await.unwrap_err();
    assert!(matches!(err, JwksError::UnknownKid { .. }));

    // Server pivots to a body that *would* contain the kid, but the
    // second miss within KID_MISS_REFRESH_INTERVAL must not trigger another
    // fetch — so the cache stays empty of "kX".
    fixture.set_body(jwks_with_two("k1", "kX")).await;
    let err = cache.lookup("https://idp.test", "kX").await.unwrap_err();
    assert!(matches!(err, JwksError::UnknownKid { .. }));
    assert!(
        cache.key_count("https://idp.test") < 2,
        "second kid-miss within {:?} must not refresh",
        KID_MISS_REFRESH_INTERVAL
    );
}

#[tokio::test]
async fn transient_outage_leaves_cached_keys_intact() {
    let (url, fixture, _server) = spawn_jwks_server().await;
    fixture.set_body(jwks_with_kid("k1")).await;
    let cache = JwksCache::new();
    cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url,
    }).await;
    assert!(cache.lookup("https://idp.test", "k1").await.is_ok());

    // JWKS endpoint flips to 500. Background refresh runs; key set must not
    // be cleared — degrade-gracefully posture (ADR-003).
    fixture.set_fail(true).await;
    cache.refresh_all().await;
    assert_eq!(cache.status_of("https://idp.test"), Some(IssuerStatus::Ready));
    assert!(cache.lookup("https://idp.test", "k1").await.is_ok());
}

#[tokio::test]
async fn add_issuer_replaces_url_for_same_iss() {
    let (url_a, fix_a, _a) = spawn_jwks_server().await;
    let (url_b, fix_b, _b) = spawn_jwks_server().await;
    fix_a.set_body(jwks_with_kid("ka")).await;
    fix_b.set_body(jwks_with_kid("kb")).await;

    let cache = JwksCache::new();
    cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url_a,
    }).await;
    assert!(cache.lookup("https://idp.test", "ka").await.is_ok());

    cache.add_issuer(IssuerConfig {
        issuer: "https://idp.test".into(),
        jwks_url: url_b,
    }).await;
    // After re-registration, only the new URL's key set should be live.
    assert!(cache.lookup("https://idp.test", "kb").await.is_ok());
    assert!(matches!(
        cache.lookup("https://idp.test", "ka").await.unwrap_err(),
        JwksError::UnknownKid { .. }
    ));
}
