//! HTTP handlers for the OIDC authorization-code flow.
//!
//! Three routes, all under `/auth`, deliberately *outside* the
//! `/api/{org}/{app}/{domain}/{object}/{version}` surface that the auth
//! middleware guards. The auth middleware skips them (its
//! `schema_path_from_uri` returns `None`) — the handlers carry their own
//! state and run as `velocity_api` directly, without a
//! [`crate::session::with_session_context`] prelude.
//!
//! ## Routes
//!
//! - `GET /auth/login/{namespace}/{name}` — kick off the redirect. Resolves
//!   the strategy from the path, generates PKCE + `state` + `nonce`,
//!   builds the IdP authorization URL, and sets the flow cookie before
//!   issuing a 302.
//! - `GET /auth/callback` — receives the authorization code from the IdP,
//!   verifies state via the flow cookie, exchanges the code for tokens
//!   (`client_secret_basic`, PKCE), verifies the ID token's signature
//!   against the strategy's JWKS, checks `nonce` in constant time, maps
//!   claims into an [`crate::Identity`], and persists a row in
//!   `platform.sessions`. Sets the session cookie and 302s to
//!   `return_to`. UserInfo merging is intentionally deferred — Phase 2c
//!   relies on the ID token alone for identity.
//! - `POST /auth/logout` — revokes the session row and clears the cookie.
//!
//! ## Scope of this phase
//!
//! Per task #34 — full OIDC redirect flow. Token exchange uses
//! `client_secret_basic` (RFC 6749 §2.3.1); the plaintext client secret
//! is resolved via [`ClientSecretResolver`] which production wires to an
//! env var (`VELOCITY_API_OIDC_CLIENT_SECRET_<NS>_<NAME>`) and tests
//! wire to a static map. ID-token verification reuses the strategy's
//! [`JwksCache`]; nonce is checked manually after decode (it's not part
//! of `jsonwebtoken`'s validation).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine;
use dashmap::DashMap;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use velocity_types::common::NamespacedRef;
use velocity_types::crds::auth::{AuthStrategyType, OidcConfig};

use crate::auth::claims::CompiledClaimMapping;
use crate::auth::jwks::JwksCache;
use crate::auth::oidc::{
    constant_time_eq, decode_flow_cookie, encode_flow_cookie, generate_nonce, generate_state,
    FlowState, PkcePair, TokenResponse,
};
use crate::auth::session::{SessionStore, DEFAULT_SESSION_TTL, SESSION_COOKIE_NAME};
use crate::auth::{AuthRegistry, ResolvedAuthStrategy};
use crate::ApiError;

/// State packaged for the `/auth/*` sub-router. Distinct from `AppState`
/// because these handlers don't touch the schema registry — only the
/// auth registry, the session store, the flow-cookie key, and the bits
/// needed to verify an ID token (JWKS + compiled claim mappings).
///
/// `jwks` and `claim_mappings` are the same `Arc`s that
/// [`crate::auth::AuthState`] holds — the callback writes a session, the
/// middleware reads it back on every subsequent request, and both need
/// the strategy's compiled mapping. Sharing the `Arc`s (rather than
/// rebuilding) keeps the two paths in lock-step with the informer's
/// `prime_strategy` cycle.
#[derive(Clone)]
pub struct AuthHandlersState {
    pub auth_registry: Arc<AuthRegistry>,
    pub sessions: Arc<dyn SessionStore>,
    /// HMAC key for signing the flow cookie. Loaded from
    /// `VELOCITY_API_FLOW_COOKIE_KEY` at startup; must be at least 32
    /// bytes. Kept opaque (`Vec<u8>`) here so the handlers never log it.
    pub flow_cookie_key: Arc<Vec<u8>>,
    /// JWKS cache shared with the auth middleware — used by the callback
    /// to verify the ID-token signature against the strategy's issuer.
    pub jwks: JwksCache,
    /// Per-strategy compiled claim mappings — same map the middleware
    /// reads on every request. The callback applies the mapping once to
    /// derive the actor id before persisting the session row, so the
    /// stored `actor_id` matches what later requests will compute.
    pub claim_mappings: Arc<DashMap<String, Arc<CompiledClaimMapping>>>,
    /// HTTP client for the token-endpoint POST. Cheap to clone (one
    /// `Arc` internally) — sharing across handlers reuses the connection
    /// pool.
    pub http: reqwest::Client,
    /// Resolves a strategy's `client_secret_ref` to the plaintext secret
    /// at request time. Production wires
    /// [`EnvClientSecretResolver`]; integration tests wire
    /// [`StaticClientSecretResolver`].
    pub client_secret_resolver: Arc<dyn ClientSecretResolver>,
}

impl std::fmt::Debug for AuthHandlersState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthHandlersState")
            .field("auth_registry", &"<AuthRegistry>")
            .field("sessions", &"<SessionStore>")
            .field("flow_cookie_key_len", &self.flow_cookie_key.len())
            .field("jwks", &self.jwks)
            .field("claim_mappings_len", &self.claim_mappings.len())
            .field("client_secret_resolver", &"<ClientSecretResolver>")
            .finish()
    }
}

// ─── Client-secret resolution ──────────────────────────────────────────────

/// Resolves a `client_secret_ref` on an OIDC strategy to the plaintext
/// shared secret used at the token endpoint. Kept behind a trait so the
/// production path (env var) and the test path (in-memory map) share one
/// call site in the callback.
///
/// Errors return `None` so the handler can surface a uniform 401
/// "session not established" — never leaking *which* leg of the
/// negotiation failed (and never echoing a secret-shaped string back).
#[async_trait]
pub trait ClientSecretResolver: Send + Sync {
    /// Resolve the secret for `strategy_key` (`"{namespace}/{name}"`).
    /// The CRD's `SecretRef { name, key }` is passed verbatim so an
    /// alternative backend (Vault, file mount) can use either part of
    /// the address without re-deriving it from the strategy key.
    async fn resolve(
        &self,
        strategy_key: &str,
        secret_ref: &velocity_types::crds::auth::SecretRef,
    ) -> Option<String>;
}

/// Production resolver — reads `VELOCITY_API_OIDC_CLIENT_SECRET_<KEY>`
/// where `<KEY>` is `strategy_key` upper-cased with `/` and `-`
/// converted to `_`. Keeps the secret out of every CRD file and the
/// process's command line.
#[derive(Debug, Default)]
pub struct EnvClientSecretResolver;

#[async_trait]
impl ClientSecretResolver for EnvClientSecretResolver {
    async fn resolve(
        &self,
        strategy_key: &str,
        _secret_ref: &velocity_types::crds::auth::SecretRef,
    ) -> Option<String> {
        let suffix = strategy_key.replace(['/', '-'], "_").to_ascii_uppercase();
        let var = format!("VELOCITY_API_OIDC_CLIENT_SECRET_{suffix}");
        match std::env::var(&var) {
            Ok(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
}

/// Test resolver — backed by a fixed map keyed by `strategy_key`.
#[derive(Debug, Default)]
pub struct StaticClientSecretResolver {
    pub secrets: HashMap<String, String>,
}

impl StaticClientSecretResolver {
    pub fn with(mut self, strategy_key: &str, secret: &str) -> Self {
        self.secrets.insert(strategy_key.to_string(), secret.to_string());
        self
    }
}

#[async_trait]
impl ClientSecretResolver for StaticClientSecretResolver {
    async fn resolve(
        &self,
        strategy_key: &str,
        _secret_ref: &velocity_types::crds::auth::SecretRef,
    ) -> Option<String> {
        self.secrets.get(strategy_key).cloned()
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    /// Same-origin path to redirect to after a successful callback.
    /// Defaults to `/` if missing or invalid. The callback handler
    /// validates this again before it actually redirects.
    #[serde(default)]
    pub return_to: Option<String>,
}

const FLOW_COOKIE_NAME: &str = "velocity_oidc_flow";

/// `GET /auth/login/{namespace}/{name}` — kick off OIDC redirect.
pub async fn login(
    State(state): State<AuthHandlersState>,
    Path((namespace, name)): Path<(String, String)>,
    Query(q): Query<LoginQuery>,
) -> Result<Response, ApiError> {
    let strategy_ref = NamespacedRef { namespace: namespace.clone(), name: name.clone() };
    let strategy = state
        .auth_registry
        .resolve(&strategy_ref)
        .ok_or_else(|| ApiError::AuthStrategyMissing(format!("{namespace}/{name}")))?;
    if strategy.kind != AuthStrategyType::Oidc {
        return Err(ApiError::AuthStrategyMissing(format!(
            "strategy `{namespace}/{name}` is not kind: oidc"
        )));
    }
    let oidc = strategy.spec.config.oidc.as_ref().ok_or_else(|| {
        ApiError::AuthStrategyMissing(format!(
            "strategy `{namespace}/{name}` is kind: oidc but has no `oidc` config block"
        ))
    })?;

    let pkce = PkcePair::generate();
    let csrf = generate_state();
    let nonce = generate_nonce();
    let return_to = sanitize_return_to(q.return_to.as_deref());

    let flow = FlowState::new(
        csrf.clone(),
        pkce.verifier.clone(),
        nonce.clone(),
        return_to,
        strategy.key.clone(),
    );
    let cookie_value = encode_flow_cookie(&flow, &state.flow_cookie_key)
        .map_err(|e| ApiError::Internal(format!("flow cookie encode: {e}")))?;

    let auth_url = build_authorization_url(&strategy, oidc, &csrf, &nonce, &pkce.challenge);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&auth_url).map_err(|_| {
            ApiError::Internal("authorization URL is not a valid header value".into())
        })?,
    );
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{FLOW_COOKIE_NAME}={cookie_value}; HttpOnly; Secure; SameSite=Lax; Path=/auth; Max-Age=600"
        ))
        .map_err(|_| ApiError::Internal("flow cookie is not a valid header value".into()))?,
    );

    Ok((StatusCode::FOUND, headers).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// `GET /auth/callback` — exchange authorization code for tokens.
///
/// Steps, in order:
/// 1. Read the `velocity_oidc_flow` cookie set by `/auth/login`.
/// 2. Verify cookie signature, state match, and TTL — refuses to proceed
///    on any mismatch (CSRF / replay defence).
/// 3. Resolve the strategy named in the flow cookie. The strategy *key*
///    is pinned at login time so an attacker can't swap strategies
///    mid-flow by hand-crafting a new cookie value.
/// 4. POST the authorization code to the token endpoint using
///    `client_secret_basic` (RFC 6749 §2.3.1) and the PKCE
///    `code_verifier`.
/// 5. Verify the ID token: signature against the strategy's JWKS, `iss`
///    matches `oidc.issuer`, `aud` matches `oidc.client_id` (OIDC Core
///    §3.1.3.7 step 3 — **not** `IssuerConfig.audience`, which is for
///    API-call JWTs), `exp`/`nbf` with clock skew.
/// 6. Constant-time compare the `nonce` claim to the cookie's `nonce`
///    (replay defence — not part of `jsonwebtoken::Validation`).
/// 7. Apply the strategy's compiled claim mapping to derive the actor id
///    that gets stored on the session row.
/// 8. Persist a session row and set `velocity_session=<id>` cookie.
/// 9. 302 to `flow.return_to` (re-sanitised; defence in depth).
pub async fn callback(
    State(state): State<AuthHandlersState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    // 0. IdP-side error short-circuit — propagate so the user sees the
    //    *reason* in their browser address bar rather than a generic 401.
    if let Some(err) = q.error.as_deref() {
        return Err(ApiError::Unauthenticated(format!("idp returned error: {err}")));
    }
    let code =
        q.code.ok_or_else(|| ApiError::Unauthenticated("missing `code` query parameter".into()))?;
    let returned_state = q
        .state
        .ok_or_else(|| ApiError::Unauthenticated("missing `state` query parameter".into()))?;

    // 1-2. Flow cookie — find, verify, decode.
    let flow_cookie = flow_cookie_from_headers(&headers)
        .ok_or_else(|| ApiError::Unauthenticated("missing or malformed oidc flow cookie".into()))?;
    let flow =
        decode_flow_cookie(&flow_cookie, &returned_state, &state.flow_cookie_key, unix_now())
            .map_err(|e| ApiError::Unauthenticated(format!("flow cookie: {e}")))?;

    // 3. Resolve strategy from the flow's pinned key.
    let (ns, name) = flow.strategy_key.split_once('/').ok_or_else(|| {
        ApiError::Internal(format!(
            "flow cookie has malformed strategy_key `{}`",
            flow.strategy_key
        ))
    })?;
    let strategy_ref = NamespacedRef { namespace: ns.into(), name: name.into() };
    let strategy = state
        .auth_registry
        .resolve(&strategy_ref)
        .ok_or_else(|| ApiError::AuthStrategyMissing(flow.strategy_key.clone()))?;
    if strategy.kind != AuthStrategyType::Oidc {
        return Err(ApiError::AuthStrategyMissing(format!(
            "strategy `{}` is not kind: oidc",
            strategy.key
        )));
    }
    let oidc = strategy.spec.config.oidc.as_ref().ok_or_else(|| {
        ApiError::AuthStrategyMissing(format!(
            "strategy `{}` is kind: oidc but has no `oidc` config block",
            strategy.key
        ))
    })?;

    // 4. Resolve client secret. Missing secret → 401 (don't surface
    //    "config drift" to the IdP-driven flow; treat it as an auth
    //    failure so the operator notices via dashboards rather than
    //    leaking config state to the browser).
    let client_secret = state
        .client_secret_resolver
        .resolve(&strategy.key, &oidc.client_secret_ref)
        .await
        .ok_or_else(|| {
            tracing::warn!(strategy = %strategy.key, "client_secret unavailable for callback");
            ApiError::Unauthenticated("client_secret unavailable".into())
        })?;

    // 5. Token exchange.
    let tokens =
        exchange_code(&state.http, oidc, &code, &flow.code_verifier, &client_secret).await?;

    // 6. Verify ID token.
    let claims = verify_id_token(&state.jwks, &strategy, oidc, &tokens.id_token).await?;

    // 7. Nonce check — manual; jsonwebtoken doesn't know about OIDC nonce.
    let claim_nonce = claims
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Unauthenticated("id_token missing `nonce`".into()))?;
    if !constant_time_eq(claim_nonce.as_bytes(), flow.nonce.as_bytes()) {
        tracing::warn!(strategy = %strategy.key, "id_token nonce mismatch — possible replay");
        return Err(ApiError::Unauthenticated("id_token nonce mismatch".into()));
    }

    // 8. Map claims → actor id. We re-run the full mapping every request
    //    (cheap, see middleware), but we need the actor_id *now* to
    //    persist on the row.
    let mapping = state.claim_mappings.get(&strategy.key).map(|m| m.clone()).ok_or_else(|| {
        ApiError::Internal(format!("strategy `{}` has no compiled claim mapping", strategy.key))
    })?;
    let identity = mapping
        .apply(&claims, &strategy.key, &oidc.issuer)
        .map_err(|e| ApiError::Unauthenticated(format!("claim mapping: {e}")))?;

    // 9. Persist session row.
    let ttl = oidc
        .session_ttl
        .map(|s| std::time::Duration::from_secs(s as u64))
        .unwrap_or(DEFAULT_SESSION_TTL);
    let expires_at = chrono::Utc::now()
        + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::hours(8));
    let record = state
        .sessions
        .create(&identity.actor_id, &oidc.issuer, claims.clone(), expires_at)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "session create failed");
            ApiError::SessionUnavailable
        })?;

    // 10. Build response: 302 + Set-Cookie (session + clear flow cookie).
    let return_to = sanitize_return_to(Some(&flow.return_to));
    let session_max_age = ttl.as_secs();

    let mut out = HeaderMap::new();
    out.insert(
        header::LOCATION,
        HeaderValue::from_str(&return_to).map_err(|_| {
            ApiError::Internal("sanitized return_to is not a valid header value".into())
        })?,
    );
    out.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE_NAME}={}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={session_max_age}",
            record.id
        ))
        .map_err(|_| ApiError::Internal("session cookie is not a valid header value".into()))?,
    );
    out.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{FLOW_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/auth; Max-Age=0"
        ))
        .map_err(|_| ApiError::Internal("flow cookie clear is not a valid header value".into()))?,
    );

    Ok((StatusCode::FOUND, out).into_response())
}

/// Find `velocity_oidc_flow` in the request's Cookie header.
fn flow_cookie_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for piece in raw.split(';') {
        let piece = piece.trim();
        if let Some(value) = piece.strip_prefix(&format!("{FLOW_COOKIE_NAME}=")) {
            return Some(value.to_string());
        }
    }
    None
}

/// POST the authorization code to the token endpoint with
/// `client_secret_basic` (RFC 6749 §2.3.1). The body carries the PKCE
/// `code_verifier`; the client secret is *not* in the body when Basic
/// auth is used.
async fn exchange_code(
    http: &reqwest::Client,
    oidc: &OidcConfig,
    code: &str,
    code_verifier: &str,
    client_secret: &str,
) -> Result<TokenResponse, ApiError> {
    let basic = B64_STANDARD.encode(format!("{}:{}", oidc.client_id, client_secret));
    let auth_value = format!("Basic {basic}");

    // Build `application/x-www-form-urlencoded` body by hand —
    // reqwest's `.form()` lives behind a feature we don't enable.
    let body = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={redirect}&code_verifier={verifier}",
        code = urlenc(code),
        redirect = urlenc(&oidc.redirect_uri),
        verifier = urlenc(code_verifier),
    );

    let resp = http
        .post(&oidc.token_endpoint)
        .header(header::AUTHORIZATION, auth_value)
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "token endpoint request failed");
            ApiError::Unauthenticated("token exchange failed".into())
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        // Body may carry RFC 6749 error details — log them but don't
        // surface to the caller (could echo attacker-influenced input).
        let body_preview = resp.text().await.unwrap_or_default();
        tracing::warn!(
            status = %status,
            body = %body_preview.chars().take(200).collect::<String>(),
            "token endpoint returned non-2xx",
        );
        return Err(ApiError::Unauthenticated("token exchange rejected".into()));
    }

    resp.json::<TokenResponse>().await.map_err(|e| {
        tracing::warn!(error = %e, "token endpoint body not valid JSON");
        ApiError::Unauthenticated("token exchange response malformed".into())
    })
}

/// Verify the ID token's signature against the strategy's JWKS and
/// enforce `iss`/`aud`/`exp`/`nbf`. Returns the verified claims as a
/// JSON object so the caller can run claim mapping + nonce check.
async fn verify_id_token(
    jwks: &JwksCache,
    strategy: &ResolvedAuthStrategy,
    oidc: &OidcConfig,
    id_token: &str,
) -> Result<Value, ApiError> {
    let header = decode_header(id_token)
        .map_err(|e| ApiError::Unauthenticated(format!("id_token header: {e}")))?;
    let kid = header
        .kid
        .ok_or_else(|| ApiError::Unauthenticated("id_token header missing `kid`".into()))?;

    let jwk = jwks
        .lookup(&oidc.issuer, &kid)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, issuer = %oidc.issuer, kid = %kid, "id_token JWKS lookup failed");
            ApiError::Unauthenticated("id_token JWKS lookup failed".into())
        })?;
    let alg = jwk
        .common
        .key_algorithm
        .and_then(jwk_alg_to_jsonwebtoken_alg)
        .ok_or_else(|| ApiError::Unauthenticated("JWK has no usable `alg`".into()))?;
    let decoding_key = DecodingKey::from_jwk(&jwk)
        .map_err(|e| ApiError::Unauthenticated(format!("JWK -> DecodingKey: {e}")))?;

    let mut validation = Validation::new(alg);
    validation.set_issuer(std::slice::from_ref(&oidc.issuer));
    // OIDC Core §3.1.3.7 step 3 — id_token's `aud` is `client_id`, not
    // the API-call audience configured on `IssuerConfig.audience`.
    validation.set_audience(std::slice::from_ref(&oidc.client_id));
    validation.leeway = strategy.clock_skew_secs as u64;

    let data = decode::<Value>(id_token, &decoding_key, &validation)
        .map_err(|e| ApiError::Unauthenticated(format!("id_token verify: {e}")))?;
    Ok(data.claims)
}

fn jwk_alg_to_jsonwebtoken_alg(a: jsonwebtoken::jwk::KeyAlgorithm) -> Option<Algorithm> {
    use jsonwebtoken::jwk::KeyAlgorithm as K;
    Some(match a {
        K::RS256 => Algorithm::RS256,
        K::RS384 => Algorithm::RS384,
        K::RS512 => Algorithm::RS512,
        K::PS256 => Algorithm::PS256,
        K::PS384 => Algorithm::PS384,
        K::PS512 => Algorithm::PS512,
        K::ES256 => Algorithm::ES256,
        K::ES384 => Algorithm::ES384,
        K::EdDSA => Algorithm::EdDSA,
        K::HS256 => Algorithm::HS256,
        K::HS384 => Algorithm::HS384,
        K::HS512 => Algorithm::HS512,
        _ => return None,
    })
}

/// `POST /auth/logout` — revoke the current session row and clear the
/// `velocity_session` cookie. Always succeeds (idempotent) — a missing
/// or malformed cookie just clears whatever the browser holds.
pub async fn logout(
    State(state): State<AuthHandlersState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(session_id) = session_id_from_cookie(&headers) {
        if let Err(e) = state.sessions.revoke(session_id).await {
            tracing::warn!(error = %e, "session revoke failed during logout");
        }
    }
    let mut out = HeaderMap::new();
    out.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0"
        ))
        .map_err(|_| ApiError::Internal("logout cookie is not a valid header value".into()))?,
    );
    out.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{FLOW_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/auth; Max-Age=0"
        ))
        .map_err(|_| ApiError::Internal("flow cookie is not a valid header value".into()))?,
    );
    Ok((StatusCode::NO_CONTENT, out).into_response())
}

/// Build the IdP authorization endpoint URL with the standard OIDC query
/// parameters. PKCE is mandatory (`code_challenge_method=S256`).
fn build_authorization_url(
    strategy: &ResolvedAuthStrategy,
    oidc: &velocity_types::crds::auth::OidcConfig,
    state: &str,
    nonce: &str,
    code_challenge: &str,
) -> String {
    let _ = strategy; // strategy.key not needed in the URL — only in the cookie
    let mut scopes: Vec<&str> = oidc.scopes.iter().map(String::as_str).collect();
    if !scopes.contains(&"openid") {
        scopes.push("openid");
    }
    let scope = scopes.join(" ");

    let mut url = oidc.authorization_endpoint.clone();
    let sep = if url.contains('?') { '&' } else { '?' };
    url.push(sep);
    url.push_str(&format!(
        "response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        urlenc(&oidc.client_id),
        urlenc(&oidc.redirect_uri),
        urlenc(&scope),
        urlenc(state),
        urlenc(nonce),
        urlenc(code_challenge),
    ));
    url
}

fn urlenc(s: &str) -> String {
    // Minimal RFC 3986 percent-encoder — same shape as `config::percent_encode`
    // but operating on a `&str` directly.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if ok {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Same-origin relative path validator for `return_to`. Anything that
/// starts with `//` or a scheme is rejected — those could redirect the
/// user to an attacker-controlled host after login.
fn sanitize_return_to(raw: Option<&str>) -> String {
    let candidate = raw.unwrap_or("/");
    if candidate.is_empty()
        || !candidate.starts_with('/')
        || candidate.starts_with("//")
        || candidate.contains("://")
    {
        return "/".to_string();
    }
    candidate.to_string()
}

fn session_id_from_cookie(headers: &HeaderMap) -> Option<uuid::Uuid> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for piece in raw.split(';') {
        let piece = piece.trim();
        if let Some(value) = piece.strip_prefix(&format!("{SESSION_COOKIE_NAME}=")) {
            return uuid::Uuid::parse_str(value).ok();
        }
    }
    None
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_return_to_accepts_same_origin_paths() {
        assert_eq!(sanitize_return_to(Some("/portal")), "/portal");
        assert_eq!(sanitize_return_to(Some("/portal/orders?x=1")), "/portal/orders?x=1");
    }

    #[test]
    fn sanitize_return_to_rejects_off_origin() {
        assert_eq!(sanitize_return_to(Some("//evil.example")), "/");
        assert_eq!(sanitize_return_to(Some("https://evil.example/")), "/");
        assert_eq!(sanitize_return_to(Some("javascript:alert(1)")), "/");
        assert_eq!(sanitize_return_to(Some("")), "/");
        assert_eq!(sanitize_return_to(None), "/");
    }

    #[test]
    fn urlenc_encodes_reserved() {
        assert_eq!(urlenc("plain"), "plain");
        assert_eq!(urlenc("a b"), "a%20b");
        assert_eq!(urlenc("https://idp/x?y=1"), "https%3A%2F%2Fidp%2Fx%3Fy%3D1");
    }

    #[test]
    fn build_authorization_url_includes_openid_scope() {
        let oidc = velocity_types::crds::auth::OidcConfig {
            authorization_endpoint: "https://idp.test/authorize".into(),
            token_endpoint: "https://idp.test/token".into(),
            userinfo_endpoint: None,
            client_id: "vel-client".into(),
            client_secret_ref: velocity_types::crds::auth::SecretRef {
                name: "x".into(),
                key: "y".into(),
            },
            redirect_uri: "https://api.acme/auth/callback".into(),
            scopes: vec!["profile".into(), "email".into()],
            issuer: "https://idp.test".into(),
            session_ttl: None,
        };
        let spec = velocity_types::crds::auth::AuthStrategySpec {
            kind: AuthStrategyType::Oidc,
            config: velocity_types::crds::auth::AuthStrategyConfig {
                oidc: Some(oidc.clone()),
                ..Default::default()
            },
        };
        let reference = NamespacedRef { namespace: "acme".into(), name: "default".into() };
        let strategy = ResolvedAuthStrategy::from_spec(&reference, spec);
        let url = build_authorization_url(&strategy, &oidc, "state-x", "nonce-y", "challenge-z");
        assert!(url.starts_with("https://idp.test/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=vel-client"));
        assert!(url.contains("scope=profile%20email%20openid"));
        assert!(url.contains("state=state-x"));
        assert!(url.contains("nonce=nonce-y"));
        assert!(url.contains("code_challenge=challenge-z"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn session_id_from_cookie_picks_session_cookie() {
        let mut h = HeaderMap::new();
        let id = uuid::Uuid::new_v4();
        h.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("other=foo; {SESSION_COOKIE_NAME}={id}; trailing=bar"))
                .unwrap(),
        );
        assert_eq!(session_id_from_cookie(&h), Some(id));
    }

    #[test]
    fn session_id_from_cookie_returns_none_when_missing() {
        let h = HeaderMap::new();
        assert!(session_id_from_cookie(&h).is_none());
    }
}
