//! Small request-handling helpers shared across the API tiers.
//!
//! These are deliberately free of any tier-specific application state — each
//! takes exactly the dependency it needs (`&SchemaRegistry`, `&PgPool`) so the
//! data-plane CRUD handlers and the search handlers can share them even after
//! each tier defines its own `AppState`-equivalent struct. Keeping them here
//! (rather than in `handlers`) means the search tier can reach them without
//! linking the CRUD module.

use axum::http::{HeaderMap, StatusCode};
use axum::Extension;
use sqlx::PgPool;
use velocity_types::common::SchemaPath;

use crate::audit;
use crate::auth::AuthDecision;
use crate::error::ApiError;
use crate::identity::Identity;
use crate::registry::{ResolvedSchema, SchemaRegistry};

const REQUEST_ID_HEADER: &str = "x-request-id";

/// URL path: `/api/{org}/{app}/{domain}/{object}/{version}`.
pub type SchemaPathParts = (String, String, String, String, String);

pub fn path_from_parts(parts: SchemaPathParts) -> SchemaPath {
    SchemaPath::new(parts.0, parts.1, parts.2, parts.3, parts.4)
}

/// Resolve a schema path against the registry. Takes `&SchemaRegistry`
/// directly (not an `AppState`) so every tier can call it regardless of
/// the shape of its own application state.
pub fn resolve_schema(
    registry: &SchemaRegistry,
    parts: SchemaPathParts,
) -> Result<std::sync::Arc<ResolvedSchema>, ApiError> {
    let path = path_from_parts(parts);
    registry.resolve(&path).ok_or(ApiError::SchemaNotFound)
}

/// Take the `Identity` the auth middleware attached to the request, falling
/// back to `Identity::anonymous()` when the middleware isn't wired (Phase 1
/// integration tests, healthcheck-only deployments). The RBAC gate decides
/// what an anonymous identity may actually do — see [`crate::rbac`].
pub fn identity_from_ext(ext: Option<Extension<Identity>>) -> Identity {
    ext.map(|Extension(id)| id).unwrap_or_else(Identity::anonymous)
}

/// Read the request id the `SetRequestIdLayer` attached. Returns `None`
/// when the header is absent or non-ASCII (it shouldn't be either, but
/// audit-write paths must never blow up on header weirdness).
pub fn request_id_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers.get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok())
}

/// Wrap a result and, if it is a 401/403-class `ApiError`, write a
/// denial audit row in a short side-tx before returning the error.
///
/// Audit-write failure does NOT block the response — we log + continue.
/// The intent is observability, not a security gate; the 403 is
/// already happening upstream. Takes `&PgPool` directly for the same
/// reason as [`resolve_schema`].
pub async fn audit_if_denied<T>(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    action: &str,
    decision: Option<&AuthDecision>,
    request_id: Option<&str>,
    result: Result<T, ApiError>,
) -> Result<T, ApiError> {
    match result {
        Ok(v) => Ok(v),
        Err(err) => {
            let status = err.status();
            if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
                let code = err.code();
                if let Err(e) =
                    audit::write_audit_denial(pool, schema, identity, action, code, decision, request_id)
                        .await
                {
                    tracing::error!(
                        error = %e,
                        code = %code,
                        action = %action,
                        actor = %identity.actor_id,
                        "denial audit write failed"
                    );
                }
            }
            Err(err)
        }
    }
}
