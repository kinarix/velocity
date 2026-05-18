//! OIDC authorization-code flow primitives.
//!
//! This module owns the *pure* bits of OIDC — generating PKCE pairs and
//! the CSRF `state`, signing the short-lived flow cookie that carries
//! `state` / `code_verifier` / `nonce` / `return_to` across the IdP
//! roundtrip, and the wire types for token / userinfo responses. The
//! HTTP plumbing lives in the auth handlers; the session-store
//! abstraction lives in [`crate::auth::session`]. Keeping these separated
//! means PKCE math and cookie integrity can be unit-tested without
//! standing up a session store or pool.
//!
//! ## PKCE
//!
//! RFC 7636 §4 — the verifier is `[A-Za-z0-9-._~]{43,128}`, generated
//! from at least 256 bits of entropy. The challenge is the URL-safe
//! base64 of `SHA256(verifier)` with no padding. We always send
//! `code_challenge_method=S256`; `plain` is forbidden.
//!
//! ## Flow cookie
//!
//! Between `/auth/login` and `/auth/callback` the server is stateless.
//! All cross-redirect state lives in a single signed cookie:
//!
//! ```text
//! base64url(payload_json) "." base64url(HMAC-SHA256(secret, payload_json))
//! ```
//!
//! The cookie is `HttpOnly; Secure; SameSite=Lax; Path=/auth; Max-Age=600`.
//! `SameSite=Lax` is the loosest setting compatible with the cross-site
//! POST that some IdPs use in `response_mode=form_post`; for the default
//! `query` mode it's strictly tighter than `None`. The 10-minute TTL is
//! a hard cap on the IdP roundtrip — sessions that take longer to
//! complete are expected to restart at `/auth/login`.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Hard ceiling on `state` length so an attacker can't blow up the cookie
/// by passing a giant value through `?return_to=...`.
const MAX_STATE_LEN: usize = 128;
/// Hard ceiling on the cookie payload after JSON encoding. Cookie headers
/// in most browsers tolerate ~4 KB; staying well under that.
const MAX_COOKIE_PAYLOAD_LEN: usize = 2048;
/// Flow cookie TTL — see module docs.
const FLOW_COOKIE_TTL_SECS: u64 = 600;

/// PKCE verifier/challenge pair.
///
/// The challenge is what we send to the IdP at authorization time. The
/// verifier stays server-side (in the flow cookie) until we exchange the
/// authorization code at the token endpoint.
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl PkcePair {
    /// Generate a fresh pair with 32 bytes of entropy.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = pkce_challenge(&verifier);
        Self { verifier, challenge }
    }
}

/// Compute the S256 PKCE challenge for a given verifier (RFC 7636 §4.2).
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Generate the CSRF `state` parameter — 24 bytes of entropy, URL-safe
/// base64 with no padding. The IdP echoes it back on the callback so we
/// can recognise the request as one we initiated.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate the OIDC `nonce` — bound into the ID token by the IdP so we
/// can detect a replayed token (RFC 6819 §4.6.6).
pub fn generate_nonce() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// What the flow cookie persists between `/auth/login` and `/auth/callback`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowState {
    pub state: String,
    pub code_verifier: String,
    pub nonce: String,
    /// Where to send the user after a successful callback. Always a
    /// same-origin relative path — the handler validates this before
    /// redirecting so a malicious `?return_to=https://evil` can't slip
    /// through.
    pub return_to: String,
    /// `{namespace}/{name}` of the strategy that owns this flow. Pinned
    /// at login time so an attacker can't swap strategies mid-flow by
    /// touching the cookie.
    pub strategy_key: String,
    /// Unix seconds when this flow was started — used to enforce the
    /// 10-minute TTL on the verify side.
    pub issued_at: u64,
}

impl FlowState {
    pub fn new(
        state: String,
        code_verifier: String,
        nonce: String,
        return_to: String,
        strategy_key: String,
    ) -> Self {
        let issued_at =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Self { state, code_verifier, nonce, return_to, strategy_key, issued_at }
    }
}

#[derive(Debug, Error)]
pub enum FlowCookieError {
    #[error("flow cookie malformed: {0}")]
    Malformed(&'static str),
    #[error("flow cookie signature mismatch")]
    BadSignature,
    #[error("flow cookie state mismatch")]
    StateMismatch,
    #[error("flow cookie expired")]
    Expired,
    #[error("flow cookie payload too large")]
    TooLarge,
}

/// Encode + sign a [`FlowState`] into the wire form `payload.sig`.
///
/// `secret` is the HMAC key — must be at least 32 bytes. Caller is
/// responsible for sourcing it (`VELOCITY_API_FLOW_COOKIE_KEY` env in
/// `main.rs`).
pub fn encode_flow_cookie(state: &FlowState, secret: &[u8]) -> Result<String, FlowCookieError> {
    if state.state.len() > MAX_STATE_LEN {
        return Err(FlowCookieError::TooLarge);
    }
    let payload = serde_json::to_vec(state).map_err(|_| FlowCookieError::Malformed("encode"))?;
    if payload.len() > MAX_COOKIE_PAYLOAD_LEN {
        return Err(FlowCookieError::TooLarge);
    }
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| FlowCookieError::Malformed("hmac key"))?;
    mac.update(payload_b64.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    Ok(format!("{payload_b64}.{sig}"))
}

/// Verify a flow cookie and return its payload.
///
/// Checks performed in order:
/// 1. Two `.`-separated segments
/// 2. HMAC-SHA256 over the payload segment (constant-time compare)
/// 3. JSON decode
/// 4. `state` echoed by the IdP matches the payload's `state`
/// 5. TTL — payload's `issued_at` is within the last 10 minutes
pub fn decode_flow_cookie(
    cookie: &str,
    expected_state: &str,
    secret: &[u8],
    now_secs: u64,
) -> Result<FlowState, FlowCookieError> {
    let (payload_b64, sig_b64) =
        cookie.split_once('.').ok_or(FlowCookieError::Malformed("missing separator"))?;

    if payload_b64.is_empty() || sig_b64.is_empty() {
        return Err(FlowCookieError::Malformed("empty segment"));
    }

    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| FlowCookieError::Malformed("hmac key"))?;
    mac.update(payload_b64.as_bytes());
    let provided_sig =
        URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| FlowCookieError::Malformed("sig b64"))?;
    mac.verify_slice(&provided_sig).map_err(|_| FlowCookieError::BadSignature)?;

    let payload =
        URL_SAFE_NO_PAD.decode(payload_b64).map_err(|_| FlowCookieError::Malformed("payload b64"))?;
    let state: FlowState =
        serde_json::from_slice(&payload).map_err(|_| FlowCookieError::Malformed("json"))?;

    if !constant_time_eq(state.state.as_bytes(), expected_state.as_bytes()) {
        return Err(FlowCookieError::StateMismatch);
    }
    if now_secs.saturating_sub(state.issued_at) > FLOW_COOKIE_TTL_SECS {
        return Err(FlowCookieError::Expired);
    }

    Ok(state)
}

/// Constant-time byte compare — avoids leaking the position of the first
/// differing byte via timing. Used on `state` and `nonce` so an attacker
/// can't grind them out one byte at a time. `pub(crate)` so the callback
/// handler can reuse the same primitive for its post-decode nonce check.
///
/// Backed by `subtle::ConstantTimeEq` (audited primitive) — the same
/// crate velocity-warm-reader uses for bearer-token comparison.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

// ─── OIDC wire types ────────────────────────────────────────────────────────

/// Authorization-code → token exchange response (RFC 6749 §5.1 + OIDC
/// §3.1.3.3). Fields the API actually consumes; everything else is
/// ignored so an IdP can extend without breaking us.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub id_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Vec<u8> {
        b"velocity-test-flow-cookie-hmac-key-32b".to_vec()
    }

    #[test]
    fn pkce_pair_generate_then_verify() {
        let p = PkcePair::generate();
        assert_eq!(p.challenge, pkce_challenge(&p.verifier));
        // RFC 7636 verifier alphabet check
        for c in p.verifier.chars() {
            assert!(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'));
        }
        // 32 bytes base64-no-pad → 43 chars
        assert_eq!(p.verifier.len(), 43);
    }

    #[test]
    fn pkce_challenge_matches_rfc_example() {
        // RFC 7636 §4.2 — fixed input → fixed expected challenge
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let c = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_challenge(v), c);
    }

    #[test]
    fn state_and_nonce_have_entropy() {
        let a = generate_state();
        let b = generate_state();
        assert_ne!(a, b);
        let c = generate_nonce();
        let d = generate_nonce();
        assert_ne!(c, d);
    }

    fn fresh_flow() -> FlowState {
        FlowState::new(
            "state-xyz".into(),
            "verifier-abc".into(),
            "nonce-pqr".into(),
            "/portal".into(),
            "acme/default".into(),
        )
    }

    #[test]
    fn cookie_round_trip() {
        let state = fresh_flow();
        let cookie = encode_flow_cookie(&state, &key()).unwrap();
        let now = state.issued_at + 10;
        let decoded = decode_flow_cookie(&cookie, "state-xyz", &key(), now).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn cookie_rejects_tampered_payload() {
        let state = fresh_flow();
        let cookie = encode_flow_cookie(&state, &key()).unwrap();
        let (_, sig) = cookie.split_once('.').unwrap();
        // Swap the payload while keeping the signature.
        let other = fresh_flow();
        let other_payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&FlowState { state: "elsewhere".into(), ..other }).unwrap());
        let bad = format!("{other_payload}.{sig}");
        let err = decode_flow_cookie(&bad, "state-xyz", &key(), state.issued_at + 1).unwrap_err();
        assert!(matches!(err, FlowCookieError::BadSignature));
    }

    #[test]
    fn cookie_rejects_state_mismatch() {
        let state = fresh_flow();
        let cookie = encode_flow_cookie(&state, &key()).unwrap();
        let err = decode_flow_cookie(&cookie, "different", &key(), state.issued_at + 1).unwrap_err();
        assert!(matches!(err, FlowCookieError::StateMismatch));
    }

    #[test]
    fn cookie_rejects_expired() {
        let state = fresh_flow();
        let cookie = encode_flow_cookie(&state, &key()).unwrap();
        let future = state.issued_at + FLOW_COOKIE_TTL_SECS + 1;
        let err = decode_flow_cookie(&cookie, "state-xyz", &key(), future).unwrap_err();
        assert!(matches!(err, FlowCookieError::Expired));
    }

    #[test]
    fn cookie_rejects_wrong_secret() {
        let state = fresh_flow();
        let cookie = encode_flow_cookie(&state, &key()).unwrap();
        let bad_key = b"different-key-of-the-right-length-32";
        let err = decode_flow_cookie(&cookie, "state-xyz", bad_key, state.issued_at + 1).unwrap_err();
        assert!(matches!(err, FlowCookieError::BadSignature));
    }

    #[test]
    fn cookie_rejects_malformed() {
        let err = decode_flow_cookie("no-dot-here", "x", &key(), 0).unwrap_err();
        assert!(matches!(err, FlowCookieError::Malformed(_)));
        let err = decode_flow_cookie(".only-sig", "x", &key(), 0).unwrap_err();
        assert!(matches!(err, FlowCookieError::Malformed(_)));
    }

    #[test]
    fn oversized_state_rejected() {
        let big = "x".repeat(MAX_STATE_LEN + 1);
        let s = FlowState::new(big, "v".into(), "n".into(), "/".into(), "k/v".into());
        let err = encode_flow_cookie(&s, &key()).unwrap_err();
        assert!(matches!(err, FlowCookieError::TooLarge));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
