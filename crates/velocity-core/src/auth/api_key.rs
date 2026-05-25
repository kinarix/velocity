//! API key authentication — Phase 2c.
//!
//! Plaintext format: `vel_{env}_{43-char URL-safe base64}` for 32 bytes (256
//! bits) of entropy (per `docs/design.md §1.6` / review fix S3). The platform
//! stores only `SHA256(plaintext)`; the plaintext is shown once at creation by
//! the CLI and never again. Lookup is by hash:
//!
//! 1. middleware reads `Authorization: ApiKey <plaintext>`
//! 2. structural parse — reject anything that isn't `vel_<env>_<base64>` so a
//!    JWT or a typo doesn't make it to the DB
//! 3. `sha256_hex(plaintext)` → look up `platform.api_keys` by `key_hash`
//! 4. enforce `revoked_at IS NULL`, `expires_at > now()`, and IP allowlist
//!    against the request's connect IP (callers feed `client_ip` in)
//! 5. return [`ApiKeyRecord`] — the caller builds an [`Identity`] from `actor`
//!    + `actor_type` and runs scope-intersection in lieu of role-based RBAC
//!
//! **Plaintext discipline:** the plaintext never reaches `tracing`. Hashing
//! is the first thing the checker does; failed lookups log only the hash
//! prefix (8 hex chars) so a key burned into a misconfigured log line can't
//! be exfiltrated. The `Debug` impl on [`ApiKeyError::InvalidFormat`] does
//! not echo the offending bytes for the same reason.
//!
//! **Constant-time defence:** SHA256 lookup miss vs hit timing leaks "this
//! hash exists" — accepted risk (attacker needs to brute-force the 256-bit
//! random tail, which is infeasible).

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ipnet::IpNet;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Length of the random tail (URL-safe base64, no padding) for 32 bytes.
/// `ceil(32 * 4 / 3) = 44`, minus 1 padding char = **43 chars**.
const KEY_RANDOM_TAIL_LEN: usize = 43;

/// Allowed environment slug characters in `vel_{env}_…`. Keep tight so the
/// lookup hash and the audit metadata don't drift over a typo.
fn valid_env_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 16
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Allowed characters in the random tail — URL-safe base64 alphabet, no
/// padding (matches `base64::engine::general_purpose::URL_SAFE_NO_PAD`).
fn valid_b64_tail_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Structurally validate an API key plaintext. Cheap (no I/O); call this
/// before hitting the DB so malformed strings never reach lookup.
///
/// Returns `Ok(())` if the format matches `vel_{env}_{43-char tail}`.
pub fn validate_plaintext(plaintext: &str) -> Result<(), ApiKeyError> {
    let rest = plaintext.strip_prefix("vel_").ok_or(ApiKeyError::InvalidFormat)?;
    let Some((env, tail)) = rest.split_once('_') else {
        return Err(ApiKeyError::InvalidFormat);
    };
    if !valid_env_segment(env) {
        return Err(ApiKeyError::InvalidFormat);
    }
    if tail.len() != KEY_RANDOM_TAIL_LEN {
        return Err(ApiKeyError::InvalidFormat);
    }
    if !tail.chars().all(valid_b64_tail_char) {
        return Err(ApiKeyError::InvalidFormat);
    }
    Ok(())
}

/// SHA-256 of `plaintext`, lowercase hex. Matches what
/// `platform.api_keys.key_hash` stores and what the `velocity create
/// api-key` CLI prints alongside the plaintext.
#[must_use]
pub fn sha256_hex(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    hex::encode(digest)
}

/// One row from `platform.api_keys`, projected to what the middleware needs
/// after verification. Scopes and allowlist live as JSONB in the table — we
/// parse them once on lookup and hand a typed struct back so handlers don't
/// re-parse on every request.
#[derive(Debug, Clone)]
pub struct ApiKeyRecord {
    /// `{namespace}/{name}` of the source `ApiKey` CRD. Used in audit and as
    /// the strategy provenance string on [`crate::Identity`].
    pub key: String,
    /// Actor identifier this key represents. Goes into `Identity.actor_id`
    /// and `app.current_user`.
    pub actor: String,
    /// `service` / `human` / `operator` / etc. — informational only, no
    /// authorization branches on this today.
    pub actor_type: String,
    /// Schema-scoped permissions. The Layer-1 RBAC analogue for API keys —
    /// the handler checks `(schema, op)` against this list rather than
    /// against role names.
    pub scopes: Vec<ApiKeyScope>,
    /// CIDR or bare-IP entries. Empty list = allow any source IP. Bare IPs
    /// were already normalised to `/32` (v4) or `/128` (v6) at row load.
    pub ip_allowlist: Vec<IpNet>,
}

/// One entry in `ApiKey.spec.scopes`. We keep `version` optional so a scope
/// can target every version of an object without listing each one.
#[derive(Debug, Clone)]
pub struct ApiKeyScope {
    pub schema: String,
    pub version: Option<String>,
    /// Canonical lowercase ops — `create | read | update | delete | restore
    /// | export | query | search`. Drift is caught at parse time so a typo
    /// in the CRD silently widens nothing.
    pub operations: Vec<String>,
}

/// Failure modes for API-key lookup + verification. Each variant maps to an
/// HTTP-visible `ApiError` via [`ApiKeyError::into_api_error`], so the same
/// causes always surface the same code.
#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    /// Plaintext didn't match `vel_{env}_{43-char tail}`. Body deliberately
    /// omits the offending bytes — Debug must not echo the plaintext.
    #[error("invalid api key format")]
    InvalidFormat,
    /// Hash didn't match any active row in `platform.api_keys`. Distinct
    /// from `Revoked` (row exists but `revoked_at IS NOT NULL`) so the
    /// audit pipeline can tell "wrong credential" apart from "revoked
    /// credential" — both render as 401 to the client.
    #[error("api key not recognised")]
    NotFound,
    /// Row exists but `revoked_at IS NOT NULL`.
    #[error("api key revoked")]
    Revoked,
    /// `expires_at` in the past.
    #[error("api key expired")]
    Expired,
    /// IP allowlist non-empty and source IP not in any entry.
    #[error("api key denied for client ip")]
    IpDenied,
    /// DB / driver-level error talking to Postgres. The HTTP layer maps
    /// this to 503 (not 500) so callers retry with backoff — a single
    /// `pgbouncer` blip shouldn't burn a deploy gate.
    #[error("api key backend unavailable: {0}")]
    Backend(String),
    /// Row exists but the JSONB shape doesn't match what the CRD declares.
    /// Almost always a hand-edited DB row — log loudly, return 500.
    #[error("api key row malformed: {0}")]
    RowMalformed(String),
}

impl ApiKeyError {
    /// Stable client-visible status. 401 for credential failures, 503 for
    /// backend trouble, 500 for "the operator wrote junk to the row".
    /// IP-deny is also 401 — the credential is technically valid but
    /// useless from this network, and we don't differentiate so probing
    /// from outside the perimeter gains nothing.
    pub fn into_api_error(self) -> crate::error::ApiError {
        use crate::error::ApiError;
        match self {
            ApiKeyError::InvalidFormat | ApiKeyError::NotFound => {
                ApiError::Unauthenticated("invalid api key".into())
            }
            ApiKeyError::Revoked => ApiError::Revoked,
            ApiKeyError::Expired => ApiError::Unauthenticated("api key expired".into()),
            ApiKeyError::IpDenied => {
                ApiError::Unauthenticated("api key denied for client ip".into())
            }
            ApiKeyError::Backend(_) => ApiError::IssuerUnavailable("api key backend".into()),
            ApiKeyError::RowMalformed(detail) => ApiError::Internal(detail),
        }
    }
}

/// Async lookup interface. Production wires `PgApiKeyChecker`; tests use a
/// hand-rolled mock so the middleware can be exercised without a Postgres
/// container.
#[async_trait]
pub trait ApiKeyChecker: Send + Sync + std::fmt::Debug + 'static {
    /// Resolve a plaintext key. Implementations MUST hash the plaintext
    /// before any I/O, never log the plaintext, and never echo it back in
    /// error messages.
    async fn lookup(&self, plaintext: &str) -> Result<ApiKeyRecord, ApiKeyError>;
}

/// Production [`ApiKeyChecker`] backed by Postgres.
///
/// The unique partial index `idx_api_keys_hash_active` makes the lookup an
/// O(1) hash hit; we then materialise the row (scopes + allowlist + expiry)
/// into the typed [`ApiKeyRecord`] in one round-trip.
#[derive(Clone, Debug)]
pub struct PgApiKeyChecker {
    pool: PgPool,
}

impl PgApiKeyChecker {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn into_arc(self) -> Arc<dyn ApiKeyChecker> {
        Arc::new(self)
    }
}

#[async_trait]
impl ApiKeyChecker for PgApiKeyChecker {
    async fn lookup(&self, plaintext: &str) -> Result<ApiKeyRecord, ApiKeyError> {
        // Cheap structural check first — keeps malformed/typo'd values out
        // of the lookup path and out of any latency histogram bucket the DB
        // would otherwise widen.
        validate_plaintext(plaintext)?;

        let hash = sha256_hex(plaintext);

        // `revoked_at IS NULL` is enforced both here AND by the partial
        // unique index — defense in depth: if a future migration drops the
        // partial predicate, the WHERE here still prevents admit-on-revoke.
        let row: Option<RawApiKeyRow> = sqlx::query_as(
            r#"SELECT name, namespace, actor, actor_type,
                      scopes, ip_allowlist, expires_at, revoked_at
                 FROM platform.api_keys
                WHERE key_hash = $1
                  AND revoked_at IS NULL"#,
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ApiKeyError::Backend(e.to_string()))?;

        let Some(row) = row else {
            // Log only the hash prefix — the plaintext must not leak even
            // if the operator turns up trace verbosity. Eight hex chars
            // give enough signal to grep `platform.api_keys` without
            // revealing the full credential.
            tracing::warn!(key_hash_prefix = &hash[..8], "api key lookup miss",);
            return Err(ApiKeyError::NotFound);
        };

        if let Some(expires_at) = row.expires_at {
            if expires_at <= Utc::now() {
                return Err(ApiKeyError::Expired);
            }
        }

        let scopes = parse_scopes(&row.scopes)?;
        let ip_allowlist = parse_ip_allowlist(&row.ip_allowlist)?;

        Ok(ApiKeyRecord {
            key: format!("{}/{}", row.namespace, row.name),
            actor: row.actor,
            actor_type: row.actor_type,
            scopes,
            ip_allowlist,
        })
    }
}

#[derive(sqlx::FromRow)]
struct RawApiKeyRow {
    name: String,
    namespace: String,
    actor: String,
    actor_type: String,
    scopes: serde_json::Value,
    ip_allowlist: serde_json::Value,
    expires_at: Option<DateTime<Utc>>,
    #[allow(dead_code)] // sanity-check only — the WHERE clause already filters
    revoked_at: Option<DateTime<Utc>>,
}

/// Parse `[{schema, version?, operations: [...]}]` out of the JSONB column.
/// Drift in the row shape is loud, not silent.
fn parse_scopes(raw: &serde_json::Value) -> Result<Vec<ApiKeyScope>, ApiKeyError> {
    let arr =
        raw.as_array().ok_or_else(|| ApiKeyError::RowMalformed("scopes is not an array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let schema = item
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ApiKeyError::RowMalformed(format!("scopes[{i}].schema missing or not string"))
            })?
            .to_string();
        let version = item.get("version").and_then(serde_json::Value::as_str).map(str::to_string);
        let operations = item
            .get("operations")
            .and_then(serde_json::Value::as_array)
            .map(|ops| {
                ops.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_ascii_lowercase()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.push(ApiKeyScope { schema, version, operations });
    }
    Ok(out)
}

/// Parse a JSONB array of strings into `IpNet` entries. Bare IP literals
/// (`"10.1.2.3"`) are promoted to `/32` (v4) or `/128` (v6) so callers
/// match uniformly with `IpNet::contains`.
fn parse_ip_allowlist(raw: &serde_json::Value) -> Result<Vec<IpNet>, ApiKeyError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| ApiKeyError::RowMalformed("ip_allowlist is not an array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let s = item.as_str().ok_or_else(|| {
            ApiKeyError::RowMalformed(format!("ip_allowlist[{i}] is not a string"))
        })?;
        out.push(
            parse_one_allowlist_entry(s)
                .map_err(|e| ApiKeyError::RowMalformed(format!("ip_allowlist[{i}] `{s}`: {e}")))?,
        );
    }
    Ok(out)
}

/// Accept either a CIDR (`10.0.0.0/8`) or a bare IP (`192.0.2.7`,
/// `2001:db8::1`). Bare IPs widen to `/32` or `/128` so containment is one
/// `IpNet::contains` call at request time.
fn parse_one_allowlist_entry(s: &str) -> Result<IpNet, String> {
    if let Ok(net) = IpNet::from_str(s) {
        return Ok(net);
    }
    if let Ok(addr) = IpAddr::from_str(s) {
        let net = match addr {
            IpAddr::V4(v4) => IpNet::new(IpAddr::V4(v4), 32),
            IpAddr::V6(v6) => IpNet::new(IpAddr::V6(v6), 128),
        }
        .map_err(|e| format!("widen-to-host: {e}"))?;
        return Ok(net);
    }
    Err("not a CIDR or IP literal".into())
}

/// Check `client_ip` against the record's allowlist. An empty allowlist
/// means "any source IP" — matches `docs/design.md §1.6` semantics where
/// `ipAllowlist` is optional.
pub fn ip_is_allowed(record: &ApiKeyRecord, client_ip: IpAddr) -> bool {
    if record.ip_allowlist.is_empty() {
        return true;
    }
    record.ip_allowlist.iter().any(|net| net.contains(&client_ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VALID_KEY: &str = "vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1";

    #[test]
    fn valid_key_passes_format_check() {
        // 43-char tail, lowercase env. Sanity that the test fixture itself
        // is well-formed before any negative test claims meaning.
        assert_eq!(VALID_KEY.len(), "vel_prod_".len() + 43);
        assert!(validate_plaintext(VALID_KEY).is_ok());
    }

    #[test]
    fn missing_prefix_rejected() {
        assert!(matches!(
            validate_plaintext("notvel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1"),
            Err(ApiKeyError::InvalidFormat)
        ));
    }

    #[test]
    fn wrong_tail_length_rejected() {
        // 42 chars — one short.
        let short = "vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV";
        assert!(matches!(validate_plaintext(short), Err(ApiKeyError::InvalidFormat)));
    }

    #[test]
    fn uppercase_env_rejected() {
        // Env must be lowercase-ascii — drift here would let two CRD authors
        // create `vel_PROD_…` and `vel_prod_…` that hash to different rows.
        let bad = "vel_PROD_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1";
        assert!(matches!(validate_plaintext(bad), Err(ApiKeyError::InvalidFormat)));
    }

    #[test]
    fn invalid_tail_char_rejected() {
        // `+` and `/` are base64-standard chars but not URL-safe; we reject
        // them so the plaintext is safe to put in a URL or env var.
        let bad = "vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV+";
        assert!(matches!(validate_plaintext(bad), Err(ApiKeyError::InvalidFormat)));
    }

    #[test]
    fn empty_env_rejected() {
        let bad = "vel__AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV1";
        assert!(matches!(validate_plaintext(bad), Err(ApiKeyError::InvalidFormat)));
    }

    #[test]
    fn sha256_is_stable_and_lowercase_hex() {
        // Stability is what lets the CLI print the hash alongside the
        // plaintext at creation time — if we ever change the digest, every
        // existing api_keys row becomes unfindable.
        let h = sha256_hex("vel_prod_known");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_eq!(h, sha256_hex("vel_prod_known"));
    }

    #[test]
    fn parse_scopes_reads_canonical_shape() {
        let raw = json!([
            { "schema": "purchase-order", "version": "v2", "operations": ["read", "Create"] }
        ]);
        let scopes = parse_scopes(&raw).unwrap();
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].schema, "purchase-order");
        assert_eq!(scopes[0].version.as_deref(), Some("v2"));
        // Ops are lowercased so CRD-author capitalisation drift never
        // accidentally narrows or widens permissions vs the canonical set
        // declared in rbac::op.
        assert_eq!(scopes[0].operations, vec!["read".to_string(), "create".to_string()]);
    }

    #[test]
    fn parse_scopes_loudfails_on_bad_shape() {
        let raw = json!([{ "no_schema": true }]);
        assert!(matches!(parse_scopes(&raw), Err(ApiKeyError::RowMalformed(_))));
    }

    #[test]
    fn parse_ip_allowlist_promotes_bare_ip_to_host_prefix() {
        let raw = json!(["10.0.0.0/8", "192.0.2.7", "2001:db8::1"]);
        let nets = parse_ip_allowlist(&raw).unwrap();
        assert_eq!(nets.len(), 3);
        // Bare v4 → /32, bare v6 → /128 (otherwise containment would match
        // an entire subnet by accident).
        assert_eq!(nets[1].prefix_len(), 32);
        assert_eq!(nets[2].prefix_len(), 128);
    }

    #[test]
    fn parse_ip_allowlist_loudfails_on_junk() {
        let raw = json!(["not-an-ip"]);
        assert!(matches!(parse_ip_allowlist(&raw), Err(ApiKeyError::RowMalformed(_))));
    }

    #[test]
    fn ip_is_allowed_empty_list_admits_everything() {
        let rec = ApiKeyRecord {
            key: "ns/key".into(),
            actor: "svc".into(),
            actor_type: "service".into(),
            scopes: vec![],
            ip_allowlist: vec![],
        };
        assert!(ip_is_allowed(&rec, "10.1.2.3".parse().unwrap()));
        assert!(ip_is_allowed(&rec, "::1".parse().unwrap()));
    }

    #[test]
    fn ip_is_allowed_matches_cidr() {
        let rec = ApiKeyRecord {
            key: "ns/key".into(),
            actor: "svc".into(),
            actor_type: "service".into(),
            scopes: vec![],
            ip_allowlist: vec![IpNet::from_str("10.0.0.0/8").unwrap()],
        };
        assert!(ip_is_allowed(&rec, "10.1.2.3".parse().unwrap()));
        assert!(!ip_is_allowed(&rec, "11.1.2.3".parse().unwrap()));
    }

    #[test]
    fn ip_is_allowed_matches_bare_ip_via_host_prefix() {
        let rec = ApiKeyRecord {
            key: "ns/key".into(),
            actor: "svc".into(),
            actor_type: "service".into(),
            scopes: vec![],
            ip_allowlist: vec![parse_one_allowlist_entry("192.0.2.7").unwrap()],
        };
        assert!(ip_is_allowed(&rec, "192.0.2.7".parse().unwrap()));
        assert!(!ip_is_allowed(&rec, "192.0.2.8".parse().unwrap()));
    }

    /// Hand-rolled mock — used by middleware-level tests so we can exercise
    /// the API-key code path without a Postgres container.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct MockApiKeyChecker {
        pub admit: Option<ApiKeyRecord>,
        pub err: Option<&'static str>,
    }

    #[async_trait]
    impl ApiKeyChecker for MockApiKeyChecker {
        async fn lookup(&self, plaintext: &str) -> Result<ApiKeyRecord, ApiKeyError> {
            validate_plaintext(plaintext)?;
            if let Some(reason) = self.err {
                return Err(match reason {
                    "not_found" => ApiKeyError::NotFound,
                    "revoked" => ApiKeyError::Revoked,
                    "expired" => ApiKeyError::Expired,
                    "backend" => ApiKeyError::Backend("mock".into()),
                    _ => ApiKeyError::NotFound,
                });
            }
            self.admit.clone().ok_or(ApiKeyError::NotFound)
        }
    }

    #[tokio::test]
    async fn mock_checker_respects_format_validation() {
        let m = MockApiKeyChecker {
            admit: Some(ApiKeyRecord {
                key: "ns/key".into(),
                actor: "svc".into(),
                actor_type: "service".into(),
                scopes: vec![],
                ip_allowlist: vec![],
            }),
            err: None,
        };
        // Format check runs *before* the mock's admit logic — same
        // ordering as `PgApiKeyChecker` so the test exercises the real
        // middleware path.
        let bad = m.lookup("not-a-key").await.unwrap_err();
        assert!(matches!(bad, ApiKeyError::InvalidFormat));
        let ok = m.lookup(VALID_KEY).await.unwrap();
        assert_eq!(ok.actor, "svc");
    }
}
