//! Axum middleware: extract bearer → verify JWT → build [`Identity`] →
//! attach as request extension.
//!
//! ## Verification ordering (advisor decisions)
//!
//! 1. Parse the JWT header — read `kid` only; the `alg` is for hinting.
//! 2. Decode the payload **without verifying** to read `iss`. This is only
//!    used to *select* which [`IssuerConfig`] (and therefore which JWKS)
//!    to verify against; the cryptographic check then *proves* the iss.
//! 3. Look up the JWK by `(iss, kid)`. The JWK's `alg` is what we feed to
//!    `jsonwebtoken::Validation` — never the JWT header's claim. This
//!    closes the "alg confusion" / RS256↔HS256 downgrade family of attacks.
//! 4. Run full decode-and-verify with `iss`, `aud`, `exp`, `nbf`, leeway.
//! 5. Map claims into [`Identity`] via the strategy's [`CompiledClaimMapping`].
//!
//! ## Why the `AuthState` extra wrapper
//!
//! The middleware needs three things that `AppState` doesn't carry yet:
//! the [`AuthRegistry`], the [`JwksCache`], and the per-strategy
//! [`CompiledClaimMapping`]s. We keep them in their own struct so this
//! layer can be wired into existing routers without churning every
//! handler signature.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;
use velocity_types::common::SchemaPath;
use velocity_types::crds::auth::AuthStrategyType;

use crate::auth::api_key::{ip_is_allowed, ApiKeyChecker, ApiKeyRecord};
use crate::auth::claims::CompiledClaimMapping;
use crate::auth::jwks::{JwksCache, JwksError};
use crate::auth::registry::{AuthRegistry, ResolvedAuthStrategy};
use crate::auth::revocation::{RevocationChecker, RevocationDecision, RevocationError};
use crate::auth::session::{SessionError, SessionStore, SESSION_COOKIE_NAME};
use crate::registry::SchemaRegistry;
use crate::{ApiError, Identity};

/// Per-server auth state. Cheap to clone (one `Arc` per field).
#[derive(Clone)]
pub struct AuthState {
    pub schemas: Arc<SchemaRegistry>,
    pub strategies: Arc<AuthRegistry>,
    pub jwks: JwksCache,
    /// Per-strategy compiled claim mappings. Keyed by
    /// `ResolvedAuthStrategy.key` (`"{namespace}/{name}"`).
    pub claim_mappings: Arc<DashMap<String, Arc<CompiledClaimMapping>>>,
    /// Revocation backend. `None` disables the check entirely — only
    /// appropriate for tests where the strategy has no `revocation` block.
    /// In production the operator wires a `RedisRevocationChecker`.
    pub revocation: Option<Arc<dyn RevocationChecker>>,
    /// API-key lookup backend. `None` rejects every API-key request with
    /// 503 — production wires `PgApiKeyChecker`. JWT-only deployments may
    /// leave this `None`; the middleware never reaches the checker on a
    /// JWT-kind strategy.
    pub api_keys: Option<Arc<dyn ApiKeyChecker>>,
    /// Browser-session backend for the OIDC cookie flow. `None` rejects
    /// every OIDC request — production wires
    /// [`crate::auth::session::PgSessionStore`]. Strategies whose
    /// resolved leaf isn't `Oidc` never touch this.
    pub sessions: Option<Arc<dyn SessionStore>>,
    /// Optional PG pool for writing denial audit rows when the
    /// middleware itself denies a request (Revoked / RevocationUnavailable).
    /// When `None`, denial audit at the middleware layer is skipped —
    /// the request still returns the right HTTP status. Production
    /// wires the same `velocity_api` pool the handlers use.
    pub audit_pool: Option<Arc<sqlx::PgPool>>,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("schemas", &"<SchemaRegistry>")
            .field("strategies", &"<AuthRegistry>")
            .field("jwks", &self.jwks)
            .field("claim_mappings_len", &self.claim_mappings.len())
            .field("revocation_configured", &self.revocation.is_some())
            .field("api_keys_configured", &self.api_keys.is_some())
            .field("sessions_configured", &self.sessions.is_some())
            .field("audit_pool_configured", &self.audit_pool.is_some())
            .finish()
    }
}

/// Captures what the middleware decided about this request's identity and
/// revocation status. Attached to the request extension so the audit
/// pipeline (task #26) can render it into `platform.audit_insert`'s
/// `p_fail_modes` JSONB without re-running the check.
///
/// ADR-003: an admitted request whose revocation backend was unreachable
/// is *not* the same as one that hit a live backend — both succeed at the
/// HTTP layer, but only the first should trigger an alert. Recording the
/// decision separately keeps that signal intact downstream.
#[derive(Debug, Clone)]
pub struct AuthDecision {
    pub revocation: RevocationDecision,
    /// What the strategy was configured with at admission time — preserved
    /// even after audit so a later policy flip doesn't make old rows lie.
    pub revocation_fail_open: bool,
    /// `{namespace}/{name}` of the strategy that admitted the request.
    pub strategy: String,
}

impl AuthState {
    pub fn new(schemas: Arc<SchemaRegistry>, strategies: Arc<AuthRegistry>, jwks: JwksCache) -> Self {
        Self {
            schemas,
            strategies,
            jwks,
            claim_mappings: Arc::new(DashMap::new()),
            revocation: None,
            api_keys: None,
            sessions: None,
            audit_pool: None,
        }
    }

    /// Builder-style — install the PG pool used to write middleware-layer
    /// denial audit rows. Without this, Revoked / RevocationUnavailable
    /// denials still produce the correct HTTP response but are not
    /// recorded in `platform.audit_log`.
    pub fn with_audit_pool(mut self, pool: Arc<sqlx::PgPool>) -> Self {
        self.audit_pool = Some(pool);
        self
    }

    /// Builder-style — install the browser-session store. Required when
    /// any resolved strategy has `kind: Oidc`; non-OIDC deployments leave
    /// this `None` and the middleware never reaches it.
    pub fn with_sessions(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.sessions = Some(store);
        self
    }

    /// Builder-style — install the revocation backend. The middleware will
    /// run the check on every request once this is set.
    pub fn with_revocation(mut self, checker: Arc<dyn RevocationChecker>) -> Self {
        self.revocation = Some(checker);
        self
    }

    /// Builder-style — install the API-key checker. Required when any
    /// resolved strategy has `kind: ApiKey` (or `Composite` once that
    /// lands); JWT-only deployments may omit this.
    pub fn with_api_keys(mut self, checker: Arc<dyn ApiKeyChecker>) -> Self {
        self.api_keys = Some(checker);
        self
    }

    /// Compile claim mappings for every issuer on `strategy` and cache
    /// them under the strategy's key. Idempotent — second call replaces
    /// the previous entry (so an updated `AuthStrategy` reconciles cleanly).
    pub fn prime_strategy(
        &self,
        strategy: &ResolvedAuthStrategy,
    ) -> Result<(), crate::auth::claims::ClaimError> {
        // Phase 2a: one compiled mapping per strategy — claim mappings are
        // declared per-issuer in the CRD, but we currently use the first
        // issuer's mapping for the whole strategy. Multi-issuer-distinct
        // mappings land when we have a real motivating use case.
        let Some(first) = strategy.issuers.values().next() else {
            return Ok(());
        };
        let compiled = CompiledClaimMapping::from_crd(&first.claims)?;
        self.claim_mappings.insert(strategy.key.clone(), compiled);
        Ok(())
    }
}

/// Axum middleware function. Wire as
/// `Router::layer(axum::middleware::from_fn_with_state(auth_state, authenticate))`.
pub async fn authenticate(
    State(state): State<AuthState>,
    mut req: Request,
    next: Next,
) -> Response {
    match try_authenticate(&state, &mut req).await {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

async fn try_authenticate(state: &AuthState, req: &mut Request<Body>) -> Result<(), ApiError> {
    // Resolve the schema path from the URI so we know which strategy to
    // apply. Routes that aren't under /api/{org}/{app}/{domain}/{object}/{ver}
    // are skipped — they're either /healthz, /readyz, or the index.
    let Some(path) = schema_path_from_uri(req.uri().path()) else {
        return Ok(());
    };
    let schema = state
        .schemas
        .resolve(&path)
        .ok_or_else(|| ApiError::SchemaNotFound)?;

    let strategy_ref = &schema.spec.auth.strategy_ref;
    let strategy = state
        .strategies
        .resolve(strategy_ref)
        .ok_or_else(|| ApiError::AuthStrategyMissing(format!(
            "{}/{}",
            strategy_ref.namespace, strategy_ref.name
        )))?;

    // Resolve composite indirection iteratively. Iterative rather than
    // recursive so the future stays `Send` (a `&Request<Body>` capture
    // breaks `Send` because `Body` isn't `Sync`). Real configs are depth
    // 0 (leaf) or 1 (composite -> leaf); we cap at MAX_COMPOSITE_DEPTH
    // and surface a 500 on any cycle.
    let leaf = resolve_leaf_strategy(state, strategy, req)?;

    // Dispatch on the leaf's `kind`. Schemes are exclusive:
    // a `Bearer` token on an `ApiKey` strategy is rejected without
    // reaching the api-key backend (and vice versa) so a leaked JWT
    // can't be re-interpreted as an API key by a mis-configured route.
    //
    // Credentials are copied out of the request *before* any `.await` so
    // the future we produce is `Send`. `Request<Body>` is `Send` but not
    // `Sync` (the inner `Body` is `dyn HttpBody + Send` only), so any
    // reborrow that survives an `.await` would taint the whole middleware
    // future with `!Send` and Axum's `Service<Request>` bound would fail.
    let identity = match leaf.kind {
        AuthStrategyType::Jwt => {
            let token = bearer_token(req)?.to_string();
            verify_and_map(state, &leaf, &token).await?
        }
        AuthStrategyType::ApiKey => {
            let plaintext = api_key_credential(req)?.to_string();
            let client_ip = client_ip_from_request(req);
            authenticate_api_key(state, &leaf, plaintext, client_ip).await?
        }
        AuthStrategyType::None => Identity::anonymous(),
        AuthStrategyType::Composite => {
            // `resolve_leaf_strategy` returned a `Composite` only if every
            // hop ran out without reaching a leaf — internal-error shape.
            return Err(ApiError::Internal(
                "composite strategy resolved to composite — likely cycle in children".into(),
            ));
        }
        AuthStrategyType::Oidc => {
            // Defensive — a `kind: Oidc` strategy without an `oidc:` block
            // is a misconfigured CRD. If a stale `CompiledClaimMapping`
            // were still primed from an earlier valid spec we'd otherwise
            // admit requests whose session was minted against a config
            // that's since been invalidated. Hard-fail before lookup.
            if leaf.spec.config.oidc.is_none() {
                return Err(ApiError::AuthStrategyMissing(format!(
                    "strategy `{}` is kind: oidc but has no `oidc` config block",
                    leaf.key
                )));
            }
            // OIDC requests bring a `velocity_session` cookie. The handler
            // at `/auth/callback` issued it after verifying the ID token
            // and persisting the claim set in `platform.sessions`. The
            // middleware re-runs claim mapping every request — keeps the
            // session row small (claims only, no compiled artefacts) and
            // means a strategy edit takes effect on the next request
            // without rewriting sessions in-place.
            let session_id = session_cookie_id(req)?;
            authenticate_oidc(state, &leaf, session_id).await?
        }
    };

    let decision = if identity.is_anonymous() {
        // No actor to look up in the revoked set.
        RevocationDecision::Allowed
    } else {
        match check_revocation(state, &leaf, &identity.actor_id).await {
            Ok(d) => d,
            Err(err) => {
                // ADR-005: revocation-class denials (Revoked, RevocationUnavailable
                // with fail-closed) are the most security-critical 403/503 paths
                // we serve — record them in the audit chain even though the
                // request never reaches a handler. Audit failure is logged but
                // does not block the rejection.
                if let Some(pool) = state.audit_pool.as_ref() {
                    let code = err.code();
                    let action = action_from_method(req.method());
                    let request_id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok());
                    let provisional = AuthDecision {
                        revocation: match &err {
                            ApiError::Revoked => RevocationDecision::RevokedActor,
                            _ => RevocationDecision::BackendDownDenied,
                        },
                        revocation_fail_open: leaf.revocation_fail_open,
                        strategy: leaf.key.clone(),
                    };
                    if let Err(e) = crate::audit::write_audit_denial(
                        pool.as_ref(),
                        &schema,
                        &identity,
                        action,
                        code,
                        Some(&provisional),
                        request_id,
                    )
                    .await
                    {
                        tracing::error!(
                            error = %e,
                            code = %code,
                            actor = %identity.actor_id,
                            "middleware-layer denial audit write failed"
                        );
                    }
                }
                return Err(err);
            }
        }
    };
    req.extensions_mut().insert(identity);
    req.extensions_mut().insert(AuthDecision {
        revocation: decision,
        revocation_fail_open: leaf.revocation_fail_open,
        // Audit records the *leaf* strategy that actually admitted the
        // request, not the composite wrapper — composite is only a
        // routing hop, not a credential source.
        strategy: leaf.key.clone(),
    });
    Ok(())
}

/// Maximum levels of `Composite -> Composite -> …` indirection we'll
/// follow before giving up. A real config has depth 0 (leaf) or 1
/// (composite -> leaf). Anything beyond 2 is almost certainly a cycle or
/// a mistake; we reject loudly rather than recurse to overflow.
const MAX_COMPOSITE_DEPTH: usize = 4;

/// Walk composite children iteratively until we land on a leaf strategy
/// (any kind other than `Composite`). The leaf is what `try_authenticate`
/// then dispatches against.
///
/// Composite picks the first child whose credential scheme is present on
/// the request. There is no fall-through after verification *failure* —
/// once we pick a child, that's the child; if it 401s, the request 401s.
/// Allowing fall-through after a verification failure would let an
/// attacker probe two strategies' error oracles in a single request.
fn resolve_leaf_strategy(
    state: &AuthState,
    initial: Arc<ResolvedAuthStrategy>,
    req: &Request<Body>,
) -> Result<Arc<ResolvedAuthStrategy>, ApiError> {
    let mut current = initial;
    let scheme = present_scheme(req);

    for _ in 0..=MAX_COMPOSITE_DEPTH {
        if current.kind != AuthStrategyType::Composite {
            return Ok(current);
        }
        if current.composite_children.is_empty() {
            return Err(ApiError::AuthStrategyMissing(format!(
                "composite strategy `{}` declares no children",
                current.key
            )));
        }

        let mut next: Option<Arc<ResolvedAuthStrategy>> = None;
        for child_ref in &current.composite_children {
            let child = state.strategies.resolve(child_ref).ok_or_else(|| {
                ApiError::AuthStrategyMissing(format!(
                    "{}/{}",
                    child_ref.namespace, child_ref.name
                ))
            })?;
            if scheme_matches(child.kind, scheme.as_deref()) {
                next = Some(child);
                break;
            }
        }

        match next {
            Some(child) => current = child,
            None => {
                // No child matches the request's scheme — list what
                // *would* be accepted so callers can fix their headers.
                let mut accepts: Vec<&'static str> = current
                    .composite_children
                    .iter()
                    .filter_map(|child_ref| state.strategies.resolve(child_ref))
                    .filter_map(|c| scheme_label(c.kind))
                    .collect();
                accepts.sort();
                accepts.dedup();
                return Err(ApiError::Unauthenticated(format!(
                    "missing or unrecognised Authorization scheme; composite accepts: {}",
                    if accepts.is_empty() {
                        "<none>".to_string()
                    } else {
                        accepts.join(", ")
                    }
                )));
            }
        }
    }

    Err(ApiError::Internal(format!(
        "composite strategy depth exceeded ({MAX_COMPOSITE_DEPTH}) — \
         likely a cycle in `children`"
    )))
}

/// The credential scheme present on the request, or `None` if there's no
/// `Authorization` header or we can't parse it. Lower-cased so `bearer`
/// and `Bearer` map the same way.
fn present_scheme(req: &Request<Body>) -> Option<String> {
    let header = req.headers().get(axum::http::header::AUTHORIZATION)?;
    let raw = header.to_str().ok()?;
    let (scheme, _rest) = raw.split_once(' ')?;
    Some(scheme.to_ascii_lowercase())
}

/// Does this child's `kind` accept the scheme present on the request?
/// `None` scheme matches `Kind::None` (allow-anon strategy) so a composite
/// that lists a `None` child as the last entry can serve as a public
/// fallback when no credentials are presented.
fn scheme_matches(kind: AuthStrategyType, scheme: Option<&str>) -> bool {
    match (kind, scheme) {
        (AuthStrategyType::Jwt | AuthStrategyType::Oidc, Some(s)) => s == "bearer",
        (AuthStrategyType::ApiKey, Some(s)) => s == "apikey",
        (AuthStrategyType::None, None) => true,
        // Composite-within-composite is allowed (depth-limited); accept
        // whatever scheme is present and let the inner dispatch decide.
        (AuthStrategyType::Composite, _) => true,
        _ => false,
    }
}

/// Human label for the credential scheme this strategy accepts. Used in
/// the 401 body when a composite has no child matching the request.
fn scheme_label(kind: AuthStrategyType) -> Option<&'static str> {
    match kind {
        AuthStrategyType::Jwt | AuthStrategyType::Oidc => Some("Bearer"),
        AuthStrategyType::ApiKey => Some("ApiKey"),
        AuthStrategyType::None => Some("<anonymous>"),
        AuthStrategyType::Composite => None,
    }
}

/// API-key verification path. Mirrors `verify_and_map` for JWT — looks up
/// the key, enforces IP allowlist + expiry + revoked, then builds the
/// [`Identity`]. The Layer-1 scope check still runs later in
/// [`crate::rbac::check_api_key_scope`]; this fn only proves the credential.
async fn authenticate_api_key(
    state: &AuthState,
    strategy: &Arc<ResolvedAuthStrategy>,
    plaintext: String,
    client_ip: Option<IpAddr>,
) -> Result<Identity, ApiError> {
    let checker = state
        .api_keys
        .as_ref()
        .ok_or_else(|| ApiError::AuthStrategyMissing("api-key backend not configured".into()))?;

    let record = checker
        .lookup(&plaintext)
        .await
        .map_err(|e| e.into_api_error())?;

    // IP allowlist — non-empty list means the request's source IP must be
    // in one entry. We only honour `ConnectInfo` (direct peer). XFF /
    // X-Real-IP would need an explicit trusted-proxy allowlist; deferred
    // to a later config knob so we don't ship a spoofable default.
    if !record.ip_allowlist.is_empty() {
        let ip = client_ip.ok_or_else(|| {
            tracing::warn!(
                api_key = %record.key,
                "api key denied: request has ip allowlist but no ConnectInfo peer"
            );
            ApiError::Unauthenticated("api key denied for client ip".into())
        })?;
        if !ip_is_allowed(&record, ip) {
            tracing::warn!(
                api_key = %record.key,
                client_ip = %ip,
                "api key denied: client ip not in allowlist",
            );
            return Err(ApiError::Unauthenticated("api key denied for client ip".into()));
        }
    }

    Ok(identity_from_api_key(strategy, &record))
}

fn identity_from_api_key(
    strategy: &ResolvedAuthStrategy,
    record: &ApiKeyRecord,
) -> Identity {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert("actor_type".to_string(), record.actor_type.clone());
    Identity {
        actor_id: record.actor.clone(),
        email: None,
        // API keys carry scopes, not roles — `Identity.roles` stays empty
        // and the handler routes scope-bearing requests through
        // `rbac::check_api_key_scope` instead of `check_route_access`.
        roles: Vec::new(),
        attributes,
        strategy: strategy.key.clone(),
        // Provenance: the `{namespace}/{name}` of the ApiKey CRD that
        // admitted this request. Distinct from `strategy` so audit can
        // tell which key was used even when multiple share a strategy.
        issuer: record.key.clone(),
        // `Some(_)` is the marker that flips Layer-1 from role-check
        // to scope-check. Even an empty Vec must be `Some(vec![])` so a
        // mis-configured key with no scopes is denied rather than
        // sliding through the JWT gate.
        api_key_scopes: Some(record.scopes.clone()),
    }
}

/// OIDC verification path. Maps a session cookie to an [`Identity`] by
/// looking up the persisted ID-token claims and re-running the strategy's
/// compiled claim mapping.
///
/// Errors translate to:
/// - missing/expired session → 401 (`Unauthenticated`)
/// - session backend unreachable → 503 (`SessionUnavailable`) — distinct
///   from a missing session so dashboards can spot a Postgres outage
///   independently of a stale-cookie spike.
/// - claim mapping fails → 401 (the persisted claims are no longer
///   compatible with the strategy's mapping; user must re-auth)
async fn authenticate_oidc(
    state: &AuthState,
    strategy: &Arc<ResolvedAuthStrategy>,
    session_id: uuid::Uuid,
) -> Result<Identity, ApiError> {
    let store = state.sessions.as_ref().ok_or_else(|| {
        ApiError::AuthStrategyMissing("session store not configured".into())
    })?;
    let record = store.lookup(session_id).await.map_err(|e| match e {
        SessionError::Expired => ApiError::Unauthenticated("session expired or missing".into()),
        SessionError::Backend(detail) => {
            tracing::warn!(error = %detail, "session backend unavailable");
            ApiError::SessionUnavailable
        }
    })?;
    let mapping = state.claim_mappings.get(&strategy.key).map(|m| m.clone()).ok_or_else(|| {
        ApiError::Internal(format!("strategy `{}` has no compiled claim mapping", strategy.key))
    })?;
    mapping
        .apply(&record.id_token_claims, &strategy.key, &record.issuer)
        .map_err(|e| ApiError::Unauthenticated(format!("claim mapping: {e}")))
}

/// Parse the `velocity_session` cookie out of `Cookie:` and decode it as
/// a UUID. We accept exactly one cookie value; if the header is missing
/// or the value isn't a UUID we 401 with a hint to log in.
fn session_cookie_id(req: &Request<Body>) -> Result<uuid::Uuid, ApiError> {
    let header = req
        .headers()
        .get(axum::http::header::COOKIE)
        .ok_or_else(|| ApiError::Unauthenticated("missing session cookie".into()))?;
    let raw = header
        .to_str()
        .map_err(|_| ApiError::Unauthenticated("Cookie header is not valid ASCII".into()))?;
    for piece in raw.split(';') {
        let piece = piece.trim();
        if let Some(value) = piece.strip_prefix(&format!("{SESSION_COOKIE_NAME}=")) {
            return uuid::Uuid::parse_str(value)
                .map_err(|_| ApiError::Unauthenticated("malformed session cookie".into()));
        }
    }
    Err(ApiError::Unauthenticated("missing session cookie".into()))
}

fn api_key_credential(req: &Request<Body>) -> Result<&str, ApiError> {
    let raw = authorization_header(req)?;
    let cred = raw
        .strip_prefix("ApiKey ")
        .or_else(|| raw.strip_prefix("apikey "))
        .ok_or_else(|| ApiError::Unauthenticated("expected `ApiKey <key>`".into()))?;
    if cred.is_empty() {
        return Err(ApiError::Unauthenticated("empty api key".into()));
    }
    Ok(cred)
}

fn authorization_header(req: &Request<Body>) -> Result<&str, ApiError> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| ApiError::Unauthenticated("missing Authorization header".into()))?;
    header
        .to_str()
        .map_err(|_| ApiError::Unauthenticated("Authorization is not valid ASCII".into()))
}

fn client_ip_from_request(req: &Request<Body>) -> Option<IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
}

/// Run the revocation check and translate it into a `RevocationDecision`
/// shaped by the strategy's `revocation_fail_open` flag.
///
/// Returns `Err` when the request must not proceed:
/// - actor in the revoked set → `ApiError::Revoked` (403)
/// - backend unreachable AND fail-closed → `ApiError::RevocationUnavailable` (503)
///
/// Returns `Ok(decision)` otherwise; the caller stores the decision on the
/// request extension so the audit row can record the fail mode.
async fn check_revocation(
    state: &AuthState,
    strategy: &Arc<ResolvedAuthStrategy>,
    actor_id: &str,
) -> Result<RevocationDecision, ApiError> {
    let Some(checker) = state.revocation.as_ref() else {
        // No backend wired — treat as allowed. Audit will see "allowed"
        // either way; the absence of a configured backend is itself a
        // platform-level concern flagged on startup, not per-request.
        return Ok(RevocationDecision::Allowed);
    };
    match checker.is_revoked(actor_id).await {
        Ok(false) => Ok(RevocationDecision::Allowed),
        Ok(true) => {
            tracing::warn!(
                actor = %actor_id,
                strategy = %strategy.key,
                "request rejected: actor in revoked set"
            );
            Err(ApiError::Revoked)
        }
        Err(RevocationError::Backend(detail)) => {
            tracing::warn!(
                actor = %actor_id,
                strategy = %strategy.key,
                fail_open = strategy.revocation_fail_open,
                error = %detail,
                "revocation backend unavailable"
            );
            if strategy.revocation_fail_open {
                // ADR-003: explicit opt-in. Admit, but the audit row will
                // carry `fail_mode = "open"` so an operator can spot bursts
                // of admitted-but-unverified traffic in Grafana.
                Ok(RevocationDecision::BackendDownAdmitted)
            } else {
                Err(ApiError::RevocationUnavailable)
            }
        }
    }
}

fn bearer_token(req: &Request<Body>) -> Result<&str, ApiError> {
    let raw = authorization_header(req)?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or_else(|| ApiError::Unauthenticated("expected `Bearer <token>`".into()))?;
    if token.is_empty() {
        return Err(ApiError::Unauthenticated("empty bearer token".into()));
    }
    Ok(token)
}

async fn verify_and_map(
    state: &AuthState,
    strategy: &Arc<ResolvedAuthStrategy>,
    token: &str,
) -> Result<Identity, ApiError> {
    let header = decode_header(token)
        .map_err(|e| ApiError::InvalidToken(format!("header parse: {e}")))?;
    let kid = header
        .kid
        .ok_or_else(|| ApiError::InvalidToken("JWT header missing `kid`".into()))?;

    // Unverified peek at `iss`, used only for picking the IssuerConfig and
    // the JWKS lookup key. The signature check below proves the value.
    let unverified_iss = unverified_issuer(token)
        .ok_or_else(|| ApiError::InvalidToken("token has no `iss` claim".into()))?;
    let issuer_cfg = strategy
        .issuers
        .get(&unverified_iss)
        .ok_or_else(|| {
            tracing::warn!(iss = %unverified_iss, strategy = %strategy.key, "rejected token: unknown issuer");
            ApiError::InvalidToken("issuer not configured on strategy".into())
        })?;

    let jwk = state
        .jwks
        .lookup(&unverified_iss, &kid)
        .await
        .map_err(map_jwks_err)?;
    let alg = jwk
        .common
        .key_algorithm
        .and_then(algorithm_from_key_algorithm)
        .ok_or_else(|| ApiError::InvalidToken("JWK has no usable `alg`".into()))?;
    let decoding_key = DecodingKey::from_jwk(&jwk)
        .map_err(|e| ApiError::InvalidToken(format!("JWK -> DecodingKey: {e}")))?;

    let mut validation = Validation::new(alg);
    validation.set_issuer(std::slice::from_ref(&unverified_iss));
    if let Some(aud) = issuer_cfg.audience.clone() {
        validation.set_audience(&[aud]);
    } else {
        validation.validate_aud = false;
    }
    validation.leeway = strategy.clock_skew_secs as u64;

    let data = decode::<Value>(token, &decoding_key, &validation)
        .map_err(|e| ApiError::InvalidToken(format!("verify: {e}")))?;

    let mapping = state
        .claim_mappings
        .get(&strategy.key)
        .map(|m| m.clone())
        .ok_or_else(|| ApiError::Internal(format!(
            "strategy `{}` has no compiled claim mapping",
            strategy.key
        )))?;
    let identity = mapping
        .apply(&data.claims, &strategy.key, &unverified_iss)
        .map_err(|e| ApiError::InvalidToken(format!("claim mapping: {e}")))?;
    Ok(identity)
}

fn map_jwks_err(e: JwksError) -> ApiError {
    match e {
        JwksError::IssuerUnavailable(iss) => ApiError::IssuerUnavailable(iss),
        JwksError::UnknownIssuer(_) | JwksError::UnknownKid { .. } => {
            ApiError::InvalidToken(e.to_string())
        }
        JwksError::Fetch { .. } | JwksError::Parse { .. } => ApiError::Internal(e.to_string()),
    }
}

fn algorithm_from_key_algorithm(a: jsonwebtoken::jwk::KeyAlgorithm) -> Option<Algorithm> {
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

/// Decode the payload segment without signature verification and read
/// `iss`. The returned value is *unverified* — callers must treat it as a
/// hint for choosing keys, never as proof of identity.
fn unverified_issuer(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload_b64,
    )
    .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("iss").and_then(Value::as_str).map(str::to_string)
}

/// Render the audit `action` for a denial happening in the middleware
/// layer. The handler-layer `audit_if_denied` is method-agnostic
/// (each handler passes its own constant), but the middleware sees the
/// raw HTTP method so we map it back to the same vocabulary. POST to
/// `/.../query` is folded into `"query"` (close enough for SOC dashboards
/// — the URL is captured via `request_id` -> trace anyway).
fn action_from_method(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    use crate::audit::action;
    match *method {
        Method::GET => action::READ,
        Method::POST => action::CREATE,
        Method::PUT | Method::PATCH => action::UPDATE,
        Method::DELETE => action::DELETE,
        _ => "unknown",
    }
}

/// Parse `/api/{org}/{app}/{domain}/{object}/{version}[/...]`.
/// Returns `None` for any non-`/api/...` path or one with too few segments.
fn schema_path_from_uri(uri_path: &str) -> Option<SchemaPath> {
    let segments: Vec<&str> = uri_path.trim_start_matches('/').split('/').collect();
    if segments.len() < 6 || segments[0] != "api" {
        return None;
    }
    Some(SchemaPath::new(segments[1], segments[2], segments[3], segments[4], segments[5]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_path_from_uri_extracts_six_segments() {
        let p = schema_path_from_uri("/api/acme/supply-chain/procurement/purchase-order/v1")
            .unwrap();
        assert_eq!(p.org, "acme");
        assert_eq!(p.app, "supply-chain");
        assert_eq!(p.object, "purchase-order");
        assert_eq!(p.version, "v1");
    }

    #[test]
    fn schema_path_from_uri_includes_id_suffix_routes() {
        let p =
            schema_path_from_uri("/api/acme/supply-chain/procurement/purchase-order/v1/abc-id")
                .unwrap();
        assert_eq!(p.object, "purchase-order");
    }

    #[test]
    fn schema_path_from_uri_rejects_unauthenticated_paths() {
        assert!(schema_path_from_uri("/healthz").is_none());
        assert!(schema_path_from_uri("/api").is_none());
        assert!(schema_path_from_uri("/api/acme/supply-chain").is_none());
    }

    #[test]
    fn unverified_issuer_reads_iss_without_signature() {
        // Hand-crafted JWT segments — payload only matters here.
        use base64::Engine;
        let payload = serde_json::json!({ "iss": "https://idp.test", "sub": "alice" });
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("aGVhZGVy.{}.c2ln", enc);
        assert_eq!(unverified_issuer(&token).as_deref(), Some("https://idp.test"));
    }

    #[test]
    fn unverified_issuer_returns_none_on_garbage() {
        assert!(unverified_issuer("not.a.jwt").is_none());
        assert!(unverified_issuer("only-one-segment").is_none());
    }

    // ── API-key credential / dispatch tests ─────────────────────────────
    //
    // These exercise the small, pure helpers — `api_key_credential` and
    // `identity_from_api_key` — that the dispatch path delegates to.
    // Full request-flow tests live in the integration suite (#32 acceptance)
    // since they need an axum router + ConnectInfo. The helpers here pin
    // the scheme-exclusivity guarantee and the Identity shape.

    use crate::auth::api_key::ApiKeyRecord;
    use velocity_types::common::NamespacedRef;
    use velocity_types::crds::auth::{
        AuthStrategyConfig, AuthStrategySpec, AuthStrategyType,
    };

    fn make_strategy(kind: AuthStrategyType) -> ResolvedAuthStrategy {
        let spec = AuthStrategySpec {
            kind,
            config: AuthStrategyConfig::default(),
        };
        let r = NamespacedRef { namespace: "acme-platform".into(), name: "default".into() };
        ResolvedAuthStrategy::from_spec(&r, spec)
    }

    fn req_with_header(value: &str) -> Request<Body> {
        let mut req = Request::new(Body::empty());
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            value.parse().unwrap(),
        );
        req
    }

    #[test]
    fn api_key_credential_extracts_after_scheme() {
        let req = req_with_header("ApiKey vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1");
        assert_eq!(
            api_key_credential(&req).unwrap(),
            "vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1"
        );
    }

    #[test]
    fn api_key_credential_rejects_bearer_scheme() {
        // Scheme-exclusivity pin: a Bearer-format JWT must NOT be accepted
        // by the api-key path. If we silently fell back to "any opaque
        // string after a space," a JWT-issuing IdP that drifted into a
        // 256-bit token format could quietly bypass the api-key checker.
        let req = req_with_header("Bearer some-jwt.payload.sig");
        let err = api_key_credential(&req).unwrap_err();
        assert!(matches!(err, ApiError::Unauthenticated(_)));
    }

    #[test]
    fn api_key_credential_rejects_empty_value() {
        let req = req_with_header("ApiKey ");
        let err = api_key_credential(&req).unwrap_err();
        assert!(matches!(err, ApiError::Unauthenticated(_)));
    }

    #[test]
    fn api_key_credential_missing_header() {
        let req = Request::new(Body::empty());
        let err = api_key_credential(&req).unwrap_err();
        assert!(matches!(err, ApiError::Unauthenticated(_)));
    }

    #[test]
    fn identity_from_api_key_carries_actor_and_strategy_provenance() {
        let strategy = make_strategy(AuthStrategyType::ApiKey);
        let record = ApiKeyRecord {
            key: "acme-supply-chain-procurement/erp-sync-key".into(),
            actor: "erp-sync-service".into(),
            actor_type: "service".into(),
            scopes: vec![],
            ip_allowlist: vec![],
        };
        let id = identity_from_api_key(&strategy, &record);
        assert_eq!(id.actor_id, "erp-sync-service");
        // Roles MUST be empty on API-key callers — design.md §1.6 routes
        // them through scope-intersection, not role-based RBAC. Drift
        // here would silently let role-named scopes leak in via the
        // wrong gate.
        assert!(id.roles.is_empty());
        assert_eq!(id.strategy, "acme-platform/default");
        // `issuer` carries the key's CRD identity for audit so an admin
        // can ask "which key was used?" not just "which strategy?".
        assert_eq!(id.issuer, "acme-supply-chain-procurement/erp-sync-key");
        assert_eq!(id.attributes.get("actor_type").map(String::as_str), Some("service"));
        assert!(!id.is_anonymous());
    }

    #[test]
    fn action_from_method_maps_each_verb() {
        use axum::http::Method;
        assert_eq!(action_from_method(&Method::GET), "read");
        assert_eq!(action_from_method(&Method::POST), "create");
        assert_eq!(action_from_method(&Method::PUT), "update");
        assert_eq!(action_from_method(&Method::PATCH), "update");
        assert_eq!(action_from_method(&Method::DELETE), "delete");
        // Any other method (HEAD, OPTIONS) gets the sentinel — the
        // route layer wouldn't accept it on a CRUD path anyway, but
        // we don't want denials on unusual methods to silently mis-tag
        // as create/read.
        assert_eq!(action_from_method(&Method::OPTIONS), "unknown");
    }

    #[test]
    fn bearer_token_rejects_apikey_scheme() {
        // Opposite-direction pin: an `ApiKey …` header must NOT be parsed
        // as a JWT by the Bearer path. The two schemes are exclusive.
        let req = req_with_header("ApiKey vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1");
        let err = bearer_token(&req).unwrap_err();
        assert!(matches!(err, ApiError::Unauthenticated(_)));
    }

    // ── Composite dispatch tests ─────────────────────────────────────────

    fn make_composite(children: Vec<NamespacedRef>) -> ResolvedAuthStrategy {
        let spec = AuthStrategySpec {
            kind: AuthStrategyType::Composite,
            config: AuthStrategyConfig {
                children,
                ..AuthStrategyConfig::default()
            },
        };
        let r = NamespacedRef { namespace: "acme-platform".into(), name: "both".into() };
        ResolvedAuthStrategy::from_spec(&r, spec)
    }

    fn child_ref(name: &str) -> NamespacedRef {
        NamespacedRef { namespace: "acme-platform".into(), name: name.into() }
    }

    fn child_strategy(name: &str, kind: AuthStrategyType) -> ResolvedAuthStrategy {
        let spec = AuthStrategySpec {
            kind,
            config: AuthStrategyConfig::default(),
        };
        let r = NamespacedRef { namespace: "acme-platform".into(), name: name.into() };
        ResolvedAuthStrategy::from_spec(&r, spec)
    }

    fn state_with_strategies(
        children: Vec<ResolvedAuthStrategy>,
    ) -> AuthState {
        use crate::auth::jwks::JwksCache;
        use crate::auth::registry::AuthRegistry;
        use crate::registry::SchemaRegistry;
        let strategies = AuthRegistry::new();
        for s in children {
            strategies.upsert(s);
        }
        let (schemas, _ready) = SchemaRegistry::new();
        AuthState::new(schemas, strategies, JwksCache::new())
    }

    #[test]
    fn composite_picks_bearer_child_when_bearer_present() {
        let composite = Arc::new(make_composite(vec![
            child_ref("jwt-primary"),
            child_ref("api-key-fallback"),
        ]));
        let state = state_with_strategies(vec![
            child_strategy("jwt-primary", AuthStrategyType::Jwt),
            child_strategy("api-key-fallback", AuthStrategyType::ApiKey),
        ]);
        let req = req_with_header("Bearer some.jwt.token");
        let leaf = resolve_leaf_strategy(&state, composite, &req).unwrap();
        assert!(matches!(leaf.kind, AuthStrategyType::Jwt));
        assert_eq!(leaf.key, "acme-platform/jwt-primary");
    }

    #[test]
    fn composite_picks_apikey_child_when_apikey_present_even_if_jwt_listed_first() {
        // Order in `children` is a tiebreaker for which scheme to *try
        // first* when a credential is present — it must NOT override
        // which scheme is actually presented. ApiKey header → ApiKey
        // child, regardless of position.
        let composite = Arc::new(make_composite(vec![
            child_ref("jwt-primary"),
            child_ref("api-key-fallback"),
        ]));
        let state = state_with_strategies(vec![
            child_strategy("jwt-primary", AuthStrategyType::Jwt),
            child_strategy("api-key-fallback", AuthStrategyType::ApiKey),
        ]);
        let req = req_with_header("ApiKey vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1");
        let leaf = resolve_leaf_strategy(&state, composite, &req).unwrap();
        assert!(matches!(leaf.kind, AuthStrategyType::ApiKey));
    }

    #[test]
    fn composite_rejects_when_no_child_accepts_scheme() {
        let composite = Arc::new(make_composite(vec![child_ref("jwt-primary")]));
        let state = state_with_strategies(vec![child_strategy(
            "jwt-primary",
            AuthStrategyType::Jwt,
        )]);
        // ApiKey scheme present but the composite only declares JWT — the
        // 401 message must list "Bearer" as the accepted scheme.
        let req = req_with_header("ApiKey vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1");
        let err = resolve_leaf_strategy(&state, composite, &req).unwrap_err();
        match err {
            ApiError::Unauthenticated(msg) => {
                assert!(msg.contains("Bearer"), "expected accepts-list in message: {msg}");
            }
            other => panic!("expected Unauthenticated, got {other:?}"),
        }
    }

    #[test]
    fn composite_rejects_with_no_children() {
        let composite = Arc::new(make_composite(vec![]));
        let state = state_with_strategies(vec![]);
        let req = req_with_header("Bearer x.y.z");
        let err = resolve_leaf_strategy(&state, composite, &req).unwrap_err();
        assert!(matches!(err, ApiError::AuthStrategyMissing(_)));
    }

    #[test]
    fn composite_unknown_child_ref_surfaces_strategy_missing() {
        // A child referenced by the composite but absent from the
        // AuthRegistry is a config drift between operator and API — fail
        // closed, don't silently skip.
        let composite = Arc::new(make_composite(vec![child_ref("ghost")]));
        let state = state_with_strategies(vec![]);
        let req = req_with_header("Bearer x.y.z");
        let err = resolve_leaf_strategy(&state, composite, &req).unwrap_err();
        assert!(matches!(err, ApiError::AuthStrategyMissing(_)));
    }

    #[test]
    fn composite_with_no_authorization_header_lists_accepted_schemes() {
        let composite = Arc::new(make_composite(vec![
            child_ref("jwt-primary"),
            child_ref("api-key-fallback"),
        ]));
        let state = state_with_strategies(vec![
            child_strategy("jwt-primary", AuthStrategyType::Jwt),
            child_strategy("api-key-fallback", AuthStrategyType::ApiKey),
        ]);
        let req = Request::new(Body::empty());
        let err = resolve_leaf_strategy(&state, composite, &req).unwrap_err();
        let ApiError::Unauthenticated(msg) = err else {
            panic!("expected Unauthenticated");
        };
        assert!(msg.contains("Bearer"));
        assert!(msg.contains("ApiKey"));
    }

    #[test]
    fn composite_passes_through_a_leaf_in_one_hop() {
        // Sanity: a non-composite "initial" strategy passes through the
        // resolver untouched. This is the JWT-only / ApiKey-only deploy
        // pattern — should not error or alter the strategy identity.
        let leaf = Arc::new(child_strategy("jwt-only", AuthStrategyType::Jwt));
        let state = state_with_strategies(vec![]);
        let req = req_with_header("Bearer x.y.z");
        let out = resolve_leaf_strategy(&state, leaf.clone(), &req).unwrap();
        assert_eq!(out.key, leaf.key);
    }

    #[test]
    fn composite_nested_composite_resolves_to_leaf() {
        // composite(outer) -> composite(inner) -> Jwt. The iterative
        // resolver follows both hops. Pinned because the previous
        // recursive implementation had a Send-bound problem with
        // borrowed `Request`s.
        let outer = Arc::new(make_composite(vec![child_ref("inner")]));
        let mut inner = make_composite(vec![child_ref("jwt-primary")]);
        // Override the inner's key to match what `child_ref("inner")` resolves.
        inner.key = "acme-platform/inner".into();
        let state = state_with_strategies(vec![
            inner,
            child_strategy("jwt-primary", AuthStrategyType::Jwt),
        ]);
        let req = req_with_header("Bearer x.y.z");
        let leaf = resolve_leaf_strategy(&state, outer, &req).unwrap();
        assert_eq!(leaf.key, "acme-platform/jwt-primary");
    }

    #[test]
    fn composite_cycle_is_rejected() {
        // `outer -> inner -> outer -> inner -> ...` — must be caught by
        // the depth limit. Confirms the iterative resolver doesn't loop.
        let mut outer = make_composite(vec![child_ref("inner")]);
        outer.key = "acme-platform/outer".into();
        let mut inner = make_composite(vec![child_ref("outer")]);
        inner.key = "acme-platform/inner".into();
        let outer_arc = Arc::new(outer.clone());
        let state = state_with_strategies(vec![outer, inner]);
        let req = req_with_header("Bearer x.y.z");
        let err = resolve_leaf_strategy(&state, outer_arc, &req).unwrap_err();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn present_scheme_lowercases_input() {
        assert_eq!(present_scheme(&req_with_header("Bearer x")).as_deref(), Some("bearer"));
        assert_eq!(present_scheme(&req_with_header("ApiKey x")).as_deref(), Some("apikey"));
        assert_eq!(present_scheme(&Request::new(Body::empty())), None);
    }

    #[test]
    fn auth_state_debug_redacts_handles_and_reports_configured_flags() {
        // Covers the manual Debug impl (lines 77-90). The output must be
        // free of <Arc<RevocationChecker>>-style raw pointers and must
        // surface configuration-presence as booleans so a deploy log
        // shows whether revocation/api-keys/sessions/audit-pool are
        // wired without leaking implementation types.
        let state = state_with_strategies(vec![]);
        let dbg = format!("{state:?}");
        assert!(dbg.contains("AuthState"));
        assert!(dbg.contains("<SchemaRegistry>"));
        assert!(dbg.contains("<AuthRegistry>"));
        assert!(dbg.contains("revocation_configured: false"));
        assert!(dbg.contains("api_keys_configured: false"));
        assert!(dbg.contains("sessions_configured: false"));
        assert!(dbg.contains("audit_pool_configured: false"));
        assert!(dbg.contains("claim_mappings_len: 0"));
    }

    #[tokio::test]
    async fn auth_state_debug_reports_configured_flags_when_set() {
        // Drive each "configured" flag true and verify the Debug
        // output flips. Catches a regression where a future builder
        // wires a backend but forgets to mark it visible in Debug.
        use std::time::Duration;
        let mut state = state_with_strategies(vec![]);
        // Each `with_*` builder mutates `self` and returns Self; we
        // need each on a separate state to keep the test independent.
        state = state.with_audit_pool(Arc::new(
            sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                .acquire_timeout(Duration::from_millis(100))
                .connect_lazy("postgres://stub:stub@127.0.0.1:1/stub")
                .unwrap(),
        ));
        let dbg = format!("{state:?}");
        assert!(
            dbg.contains("audit_pool_configured: true"),
            "audit pool flag should flip: {dbg}"
        );
    }

    #[test]
    fn auth_decision_clone_preserves_fields() {
        // The middleware attaches an AuthDecision to the request
        // extension; the audit pipeline clones it out. Catch field
        // drift between manual clone implementations.
        let d = AuthDecision {
            revocation: RevocationDecision::BackendDownAdmitted,
            revocation_fail_open: true,
            strategy: "acme-platform/default".into(),
        };
        let c = d.clone();
        assert!(c.revocation_fail_open);
        assert_eq!(c.strategy, "acme-platform/default");
        // Pattern matches keep this honest if a variant is renamed.
        assert!(matches!(c.revocation, RevocationDecision::BackendDownAdmitted));
    }

    #[test]
    fn scheme_matches_pairs_schemes_to_kinds() {
        // Pin the kind→scheme mapping so a future kind addition has to
        // either pick a scheme or explicitly opt-out. JWT and OIDC share
        // Bearer; ApiKey has its own scheme; None matches absence.
        assert!(scheme_matches(AuthStrategyType::Jwt, Some("bearer")));
        assert!(scheme_matches(AuthStrategyType::Oidc, Some("bearer")));
        assert!(scheme_matches(AuthStrategyType::ApiKey, Some("apikey")));
        assert!(scheme_matches(AuthStrategyType::None, None));
        // Negative cases — wrong scheme for the kind, or scheme present
        // when the strategy expects absence.
        assert!(!scheme_matches(AuthStrategyType::Jwt, Some("apikey")));
        assert!(!scheme_matches(AuthStrategyType::ApiKey, Some("bearer")));
        assert!(!scheme_matches(AuthStrategyType::Jwt, None));
        assert!(!scheme_matches(AuthStrategyType::None, Some("bearer")));
    }
}
