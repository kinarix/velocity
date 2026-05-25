//! Phase 2a — JWT auth, JWKS caching, AuthStrategy registry.
//!
//! The hot path looks like:
//!
//! 1. handler resolves `ResolvedSchema` from [`crate::SchemaRegistry`]
//! 2. middleware reads `spec.auth.strategy_ref` and looks up the
//!    [`ResolvedAuthStrategy`] in [`AuthRegistry`]
//! 3. middleware decodes the bearer, uses unverified `iss` to pick an
//!    `IssuerConfig`, then verifies the signature against the JWK pinned
//!    by `kid` in [`jwks::JwksCache`]
//! 4. on success, claims are mapped into [`crate::Identity`] and stored
//!    as a request extension
//!
//! Each step is in its own module so it can be tested in isolation.

pub mod api_key;
pub mod claims;
pub mod discovery;
pub mod jwks;
pub mod middleware;
pub mod oidc;
pub mod registry;
pub mod revocation;
pub mod session;

pub use api_key::{
    sha256_hex, validate_plaintext, ApiKeyChecker, ApiKeyError, ApiKeyRecord, ApiKeyScope,
    PgApiKeyChecker,
};
pub use claims::{ClaimError, CompiledClaimMapping};
pub use discovery::{DiscoveryCache, DiscoveryError, OidcDiscovery};
pub use jwks::{Jwk, JwksCache, JwksError};
pub use middleware::{authenticate, AuthDecision, AuthState};
pub use registry::{AuthRegistry, ResolvedAuthStrategy};
pub use revocation::{
    MockChecker, RedisRevocationChecker, RevocationChecker, RevocationDecision, RevocationError,
    DEFAULT_REVOKED_SET_KEY,
};
pub use session::{
    MockSessionStore, PgSessionStore, SessionError, SessionRecord, SessionStore,
    DEFAULT_SESSION_TTL, SESSION_COOKIE_NAME,
};
