//! HMAC-SHA256 token signer shared across subsystems.
//!
//! [`CursorSigner`] is the lowest-level primitive behind every opaque,
//! tamper-evident token the API mints: the `/query` keyset cursor
//! ([`crate::dsl`]) and the `/audit` keyset cursor
//! ([`crate::audit_query`]) both sign their payloads with the *same*
//! HMAC key so there is one key to rotate and one set of failure modes.
//!
//! It lives in its own module (rather than inside `dsl`) so a caller that
//! only needs to sign/verify bytes — the audit read path, the search tier
//! — doesn't pull in the `/query` DSL builder (and its `query`/`rbac`/
//! `registry` dependencies) just to reach the signer.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::ApiError;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 cursor signer. Configured from
/// `VELOCITY_API_CURSOR_SIGNING_KEY` (≥32 bytes). When unset, the API
/// still serves pages but `next_cursor` is always `null` and a
/// cursor-bearing request returns 400.
#[derive(Debug, Clone)]
pub struct CursorSigner {
    key: Arc<Vec<u8>>,
}

impl CursorSigner {
    /// Construct from a raw key. Returns `Err` if the key is shorter
    /// than 32 bytes — anything smaller is trivially brute-forceable
    /// and the failure must be loud so a misconfigured env doesn't
    /// silently weaken pagination integrity.
    pub fn new(key: Vec<u8>) -> Result<Self, &'static str> {
        if key.len() < 32 {
            return Err("cursor signing key must be at least 32 bytes");
        }
        Ok(Self { key: Arc::new(key) })
    }

    /// Sign an arbitrary byte payload, producing a URL-safe
    /// `<payload_b64>.<sig_b64>` token. Exposed so adjacent subsystems
    /// (e.g. the audit `/audit` keyset cursor in [`crate::audit_query`])
    /// can share the same HMAC key without each maintaining its own
    /// signer — one key to rotate, one set of failure modes.
    pub fn sign_bytes(&self, payload: &[u8]) -> Result<String, ApiError> {
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload);
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| ApiError::Internal("cursor hmac init".into()))?;
        mac.update(payload_b64.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(format!("{payload_b64}.{sig}"))
    }

    /// Inverse of [`Self::sign_bytes`]. Returns the decoded payload on
    /// signature match, or [`ApiError::BadRequest`] on tampered /
    /// malformed input — never silently accepts.
    pub fn verify_bytes(&self, token: &str) -> Result<Vec<u8>, ApiError> {
        let (payload_b64, sig_b64) = token
            .split_once('.')
            .ok_or_else(|| ApiError::BadRequest("cursor: malformed".into()))?;
        if payload_b64.is_empty() || sig_b64.is_empty() {
            return Err(ApiError::BadRequest("cursor: empty segment".into()));
        }
        let provided = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| ApiError::BadRequest("cursor: bad sig b64".into()))?;
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| ApiError::Internal("cursor hmac init".into()))?;
        mac.update(payload_b64.as_bytes());
        mac.verify_slice(&provided)
            .map_err(|_| ApiError::BadRequest("cursor: bad signature".into()))?;
        URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| ApiError::BadRequest("cursor: bad payload b64".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Vec<u8> {
        b"velocity-test-cursor-signing-key-32b!".to_vec()
    }

    #[test]
    fn rejects_short_key() {
        let err = CursorSigner::new(b"too-short".to_vec()).unwrap_err();
        assert!(err.contains("at least 32 bytes"));
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let s = CursorSigner::new(key()).unwrap();
        let token = s.sign_bytes(b"hello world").unwrap();
        assert_eq!(s.verify_bytes(&token).unwrap(), b"hello world");
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let s = CursorSigner::new(key()).unwrap();
        let token = s.sign_bytes(b"original").unwrap();
        let (_, sig) = token.split_once('.').unwrap();
        let forged = format!("{}.{}", URL_SAFE_NO_PAD.encode(b"tampered"), sig);
        let err = s.verify_bytes(&forged).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn verify_rejects_malformed() {
        let s = CursorSigner::new(key()).unwrap();
        assert!(matches!(s.verify_bytes("no-separator").unwrap_err(), ApiError::BadRequest(_)));
        assert!(matches!(s.verify_bytes(".only-sig").unwrap_err(), ApiError::BadRequest(_)));
    }
}
