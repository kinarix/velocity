//! Route-level RBAC — Layer 1 of the auth stack.
//!
//! After the auth middleware has built an [`Identity`] and the handler has
//! resolved the [`ResolvedSchema`], we ask one question: does any role on
//! the identity grant the requested operation on this schema?
//!
//! This is intentionally a thin gate. Row-level filtering and attribute-
//! based policies (ABAC, RLS) live below this in subsequent layers — Layer
//! 1 just keeps the wrong actor type from ever reaching the SQL builder.
//!
//! ## Why here, not in middleware
//!
//! The operation a request maps to depends on HTTP method *and* route
//! shape — `GET /…/{id}` and `GET /…` both produce `read`, but `POST` on
//! the collection is `create` while `POST /…/{id}:restore` would be
//! `restore`. The router knows that mapping; the middleware does not.
//! We pay one lookup per request (precomputed [`AccessIndex`]) in the
//! handler entry, after `resolve_schema`, before idempotency.
//!
//! ## Operation strings
//!
//! Per CLAUDE.md › *Metric label cardinality*, the canonical set is
//! `create | read | update | delete | restore | export | query | search`.
//! Handlers pass one of these literals. The [`AccessIndex`] lowercases CRD
//! input on ingest, so a CRD typo doesn't accidentally widen access — it
//! just won't match anything.
//!
//! ## Anonymous fallthrough
//!
//! Phase 1 tests don't wire the auth middleware, so handlers see no
//! [`Identity`] in the request extension. We treat that as anonymous: it
//! sails through *open* schemas (no `access.roles` declared) but is
//! denied by any schema that declares RBAC. The latter case is logged
//! at `error!` because it almost always means the auth middleware
//! isn't wired — a config/deploy bug, not a real attacker.
//!
//! [`Identity`]: crate::Identity
//! [`AccessIndex`]: crate::registry::AccessIndex
//! [`ResolvedSchema`]: crate::registry::ResolvedSchema

use crate::auth::api_key::ApiKeyScope;
use crate::error::ApiError;
use crate::identity::Identity;
use crate::registry::ResolvedSchema;

/// Canonical operation labels — exposed as constants so handler call sites
/// can't drift from the strings the [`AccessIndex`] expects.
pub mod op {
    pub const CREATE: &str = "create";
    pub const READ: &str = "read";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    #[allow(dead_code)]
    pub const RESTORE: &str = "restore";
    #[allow(dead_code)]
    pub const EXPORT: &str = "export";
    #[allow(dead_code)]
    pub const QUERY: &str = "query";
    #[allow(dead_code)]
    pub const SEARCH: &str = "search";
}

/// Layer-1 access gate. Dispatches to either role-based RBAC (JWT and
/// OIDC callers) or scope-intersection (API-key callers) based on whether
/// the identity carries `api_key_scopes`. Handlers call this — never the
/// per-strategy variants directly — so they don't have to care how the
/// credential was verified.
///
/// The two paths are deliberately distinct: an API-key caller's empty
/// scope list denies even on an open schema (would otherwise be a
/// skeleton-key footgun), whereas a JWT caller on the same open schema
/// is admitted. See [`check_api_key_scope`] for the rationale.
pub fn check_access(
    schema: &ResolvedSchema,
    identity: &Identity,
    op: &str,
) -> Result<(), ApiError> {
    if let Some(scopes) = &identity.api_key_scopes {
        check_api_key_scope(schema, scopes, op)
    } else {
        check_route_access(schema, identity, op)
    }
}

/// Layer-1 RBAC gate. Returns `Ok(())` to admit, `Err(AccessDenied)` to
/// deny. Open schemas always admit; schemas with declared roles require
/// the identity to carry at least one matching role for `op`.
pub fn check_route_access(
    schema: &ResolvedSchema,
    identity: &Identity,
    op: &str,
) -> Result<(), ApiError> {
    if schema.access.is_open {
        return Ok(());
    }

    // Schema declares RBAC. An anonymous identity at this point is almost
    // certainly a wiring bug — the auth middleware should have rejected
    // the request before we got here. Log loudly so the operator notices,
    // but still return `AccessDenied` so we never accidentally fail open.
    if identity.is_anonymous() {
        tracing::error!(
            schema = %schema.path.to_string(),
            op = %op,
            "rbac denied: anonymous identity on a schema with declared access roles — \
             this almost certainly means the auth middleware isn't wired",
        );
        return Err(ApiError::AccessDenied);
    }

    if schema.access.allows(op, &identity.roles) {
        Ok(())
    } else {
        tracing::warn!(
            schema = %schema.path.to_string(),
            actor = %identity.actor_id,
            op = %op,
            roles = ?identity.roles,
            "rbac denied: no role on identity grants the requested operation",
        );
        Err(ApiError::AccessDenied)
    }
}

/// Scope-intersection gate for API-key callers.
///
/// API keys don't carry roles — `ApiKey.spec.scopes` carries
/// `(schema, version?, operations[])` directly (`docs/design.md §1.6`). We
/// admit the request when at least one scope matches the resolved schema's
/// `object` + (optional) `version` and lists `op` in its `operations`.
///
/// Schemas that declare RBAC (`schema.access.roles[…]`) still flow through
/// `check_route_access` for JWT callers; this fn replaces that gate when
/// the strategy.kind is `ApiKey`. An *open* schema (no `access.roles`)
/// still requires an API key to declare a scope for it — otherwise a
/// stolen key would have unbounded reach across every open schema in the
/// cluster.
pub fn check_api_key_scope(
    schema: &ResolvedSchema,
    scopes: &[ApiKeyScope],
    op: &str,
) -> Result<(), ApiError> {
    let object = &schema.path.object;
    let version = &schema.path.version;
    let op_lc = op.to_ascii_lowercase();

    let admitted = scopes.iter().any(|s| {
        s.schema == *object
            && s.version.as_ref().is_none_or(|v| v == version)
            && s.operations.iter().any(|o| o == &op_lc)
    });

    if admitted {
        Ok(())
    } else {
        tracing::warn!(
            schema = %schema.path.to_string(),
            op = %op,
            "api key denied: no scope on the key grants this (object, version, op)",
        );
        Err(ApiError::AccessDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_types::common::SchemaPath;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, ObservabilitySpec, RoleAccess, SchemaDefinitionSpec, SearchSpec,
        SearchTier,
    };

    fn make_spec(roles: Vec<RoleAccess>) -> SchemaDefinitionSpec {
        SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "acme-platform".into(),
                },
                overrides: Vec::new(),
            },
            access: AccessSpec { roles, ..AccessSpec::default() },
            fields: Vec::new(),
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        }
    }

    fn make_schema(roles: Vec<RoleAccess>) -> ResolvedSchema {
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        ResolvedSchema::from_spec(path, make_spec(roles))
    }

    fn role(name: &str, ops: &[&str]) -> RoleAccess {
        RoleAccess { role: name.into(), operations: ops.iter().map(|s| (*s).into()).collect() }
    }

    fn ident(actor: &str, roles: &[&str]) -> Identity {
        Identity {
            actor_id: actor.into(),
            roles: roles.iter().map(|s| (*s).into()).collect(),
            strategy: "acme-platform/default".into(),
            ..Identity::default()
        }
    }

    #[test]
    fn open_schema_admits_anonymous() {
        let schema = make_schema(vec![]);
        let id = Identity::anonymous();
        assert!(check_route_access(&schema, &id, op::READ).is_ok());
        assert!(check_route_access(&schema, &id, op::CREATE).is_ok());
    }

    #[test]
    fn closed_schema_denies_anonymous() {
        let schema = make_schema(vec![role("reader", &["read"])]);
        let id = Identity::anonymous();
        let err = check_route_access(&schema, &id, op::READ).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    #[test]
    fn role_match_admits() {
        let schema = make_schema(vec![role("reader", &["read", "query"])]);
        let id = ident("alice", &["reader"]);
        assert!(check_route_access(&schema, &id, op::READ).is_ok());
        assert!(check_route_access(&schema, &id, op::QUERY).is_ok());
    }

    #[test]
    fn role_lacks_op_denies() {
        let schema = make_schema(vec![role("reader", &["read"])]);
        let id = ident("alice", &["reader"]);
        let err = check_route_access(&schema, &id, op::CREATE).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    #[test]
    fn unrelated_role_denies() {
        let schema = make_schema(vec![role("writer", &["create"])]);
        let id = ident("alice", &["reader"]);
        let err = check_route_access(&schema, &id, op::CREATE).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    #[test]
    fn any_matching_role_admits() {
        let schema = make_schema(vec![
            role("reader", &["read"]),
            role("admin", &["read", "create", "update", "delete"]),
        ]);
        let id = ident("alice", &["unrelated", "admin"]);
        assert!(check_route_access(&schema, &id, op::DELETE).is_ok());
    }

    fn scope(schema: &str, version: Option<&str>, ops: &[&str]) -> ApiKeyScope {
        ApiKeyScope {
            schema: schema.into(),
            version: version.map(str::to_string),
            operations: ops.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn api_key_scope_admits_on_object_version_op_match() {
        let schema = make_schema(vec![]);
        let scopes = vec![scope("purchase-order", Some("v1"), &["read", "create"])];
        assert!(check_api_key_scope(&schema, &scopes, op::READ).is_ok());
        assert!(check_api_key_scope(&schema, &scopes, op::CREATE).is_ok());
    }

    #[test]
    fn api_key_scope_version_none_acts_as_wildcard() {
        // A scope without `version` matches every version of the object —
        // matches the optional-version semantic in design.md §1.6.
        let schema = make_schema(vec![]);
        let scopes = vec![scope("purchase-order", None, &["read"])];
        assert!(check_api_key_scope(&schema, &scopes, op::READ).is_ok());
    }

    #[test]
    fn api_key_scope_wrong_object_denies() {
        let schema = make_schema(vec![]);
        let scopes = vec![scope("supplier", Some("v1"), &["read"])];
        let err = check_api_key_scope(&schema, &scopes, op::READ).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    #[test]
    fn api_key_scope_wrong_version_denies() {
        let schema = make_schema(vec![]);
        let scopes = vec![scope("purchase-order", Some("v2"), &["read"])];
        let err = check_api_key_scope(&schema, &scopes, op::READ).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    #[test]
    fn api_key_scope_missing_op_denies() {
        // Scope covers the right schema/version but only the wrong op.
        // Critical pin: an api-key holder must not be able to upgrade
        // `read` access into `delete` access by being on the same row.
        let schema = make_schema(vec![]);
        let scopes = vec![scope("purchase-order", Some("v1"), &["read"])];
        let err = check_api_key_scope(&schema, &scopes, op::DELETE).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    /// Forces every denial path to fire while a tracing subscriber is
    /// installed so the `tracing::warn!`/`tracing::error!` argument
    /// expressions are evaluated and observed by llvm-cov. Without a
    /// subscriber these lines stay uncovered even though the function
    /// is exercised by the other tests in this module.
    #[test]
    fn denial_paths_evaluate_tracing_args() {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        let _guard = tracing::subscriber::set_default(subscriber);

        // line 103: anonymous identity on closed schema
        let schema = make_schema(vec![role("reader", &["read"])]);
        let id = Identity::anonymous();
        assert!(check_route_access(&schema, &id, op::READ).is_err());

        // line 115: identity has roles but none grant the op
        let id = ident("alice", &["reader"]);
        assert!(check_route_access(&schema, &id, op::CREATE).is_err());

        // line 157: api-key scope doesn't grant the op
        let schema = make_schema(vec![]);
        let scopes = vec![scope("supplier", Some("v1"), &["read"])];
        assert!(check_api_key_scope(&schema, &scopes, op::READ).is_err());
    }

    #[test]
    fn api_key_scope_empty_list_denies_even_on_open_schema() {
        // Open schemas (no `access.roles`) let JWT callers in unauthenticated,
        // but an API key with zero scopes must still be denied — otherwise a
        // leaked key with empty scopes would be a cluster-wide skeleton key.
        let schema = make_schema(vec![]);
        let err = check_api_key_scope(&schema, &[], op::READ).unwrap_err();
        assert!(matches!(err, ApiError::AccessDenied));
    }

    fn jwt_ident(actor: &str, roles: &[&str]) -> Identity {
        ident(actor, roles)
    }

    fn api_key_ident(actor: &str, scopes: Vec<ApiKeyScope>) -> Identity {
        Identity {
            actor_id: actor.into(),
            strategy: "acme-platform/default".into(),
            api_key_scopes: Some(scopes),
            ..Identity::default()
        }
    }

    #[test]
    fn check_access_dispatches_jwt_to_role_check() {
        let schema = make_schema(vec![role("reader", &["read"])]);
        let id = jwt_ident("alice", &["reader"]);
        assert!(check_access(&schema, &id, op::READ).is_ok());
        let denied = jwt_ident("alice", &["writer"]);
        assert!(check_access(&schema, &denied, op::READ).is_err());
    }

    #[test]
    fn check_access_dispatches_api_key_to_scope_check() {
        // Identical schema-role config that would admit a JWT caller —
        // but an API-key identity must run through the scope path. Without
        // a scope entry it's denied, with a scope entry it's admitted.
        let schema = make_schema(vec![role("reader", &["read"])]);
        let no_scope = api_key_ident("svc", vec![]);
        assert!(check_access(&schema, &no_scope, op::READ).is_err());
        let with_scope = api_key_ident("svc", vec![scope("purchase-order", Some("v1"), &["read"])]);
        assert!(check_access(&schema, &with_scope, op::READ).is_ok());
    }

    #[test]
    fn check_access_api_key_identity_with_empty_scopes_denied_on_open_schema() {
        // The Some(vec![]) carrier means "this is an api-key request" —
        // even on an open schema we must deny. Pinned because the
        // alternative ("`None` and empty `Some` are equivalent") would
        // turn a leaked key with zero scopes into a skeleton key.
        let open = make_schema(vec![]);
        let id = api_key_ident("svc", vec![]);
        assert!(check_access(&open, &id, op::READ).is_err());
    }
}
