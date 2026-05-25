//! Per-request transaction prelude (ADR-007).
//!
//! Every read or write goes through [`with_session_context`]:
//!
//! 1. Acquire a connection, begin a transaction.
//! 2. `SET LOCAL ROLE <domain_role>` — drops privileges from `velocity_api`
//!    (NOBYPASSRLS, NOSUPERUSER) to the domain-specific role
//!    (`_reader`/`_writer`/`_admin`). RLS policies are enforced as if the
//!    request had connected directly as that role.
//! 3. `SET LOCAL app.current_user = '...'` — surface for RLS predicates and
//!    `platform.audit_insert()`.
//! 4. `SET LOCAL app.scoped_roles = '...'` — the encoded role-scope for
//!    Layer-7 RLS row filtering (see [`velocity_core::row_filter::scoped_roles_for_session`]).
//!    A `Broken` rowFilter on the schema surfaces here as a [`sqlx::Error`]
//!    so the handler turns it into a 500; the transaction never commits.
//! 5. Optional per-request attributes (`store_id`, `tenant_id`, etc.) via
//!    `set_config('app.<key>', '<value>', true)`.
//! 6. Run the caller's closure on the same transaction.
//! 7. Commit (or roll back via `?`).
//!
//! **SQL safety:** the `domain_role` and `app.<key>` identifiers must be
//! validated by the caller. In practice they come from `ResolvedSchema`
//! (built from the CRD, sanitized by the operator) and from a fixed
//! allow-list of attribute keys — never from the request body. Values
//! flow through `$N` binds, never interpolated.
//!
//! **Fails closed:** Layer-7 policies treat a missing `app.scoped_roles`
//! as NULL, which makes the `USING` clause evaluate NULL and excludes
//! the row. A handler path that forgot the prelude would see *zero rows*
//! rather than every row. Pinned in the operator-side integration tests.

use std::future::Future;
use std::pin::Pin;

use sqlx::{PgPool, Postgres, Transaction};

use velocity_core::identity::Identity;
use velocity_core::registry::ResolvedSchema;

/// Read transactions hit the schema's `_reader` role.
pub const ROLE_READER: RoleClass = RoleClass::Reader;
/// Create/update transactions hit `_writer`.
pub const ROLE_WRITER: RoleClass = RoleClass::Writer;
/// Delete transactions hit `_admin`.
pub const ROLE_ADMIN: RoleClass = RoleClass::Admin;

#[derive(Debug, Clone, Copy)]
pub enum RoleClass {
    Reader,
    Writer,
    Admin,
}

impl RoleClass {
    pub fn for_schema<'a>(&self, schema: &'a ResolvedSchema) -> &'a str {
        match self {
            RoleClass::Reader => &schema.pg_role_reader,
            RoleClass::Writer => &schema.pg_role_writer,
            RoleClass::Admin => &schema.pg_role_admin,
        }
    }
}

/// Identifier validator for SET LOCAL ROLE. Matches Postgres' unquoted
/// identifier syntax minus the special chars and length cap; the actual
/// role names come from operator-sanitised pg_schema strings.
fn validate_role_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Identifier validator for `app.<key>` config keys.
fn validate_attr_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Run `f` inside a transaction with the ADR-007 prelude applied.
///
/// The closure receives a `&mut Transaction` it can use for further SQL. On
/// `Ok` return, the transaction commits; on `Err`, it rolls back.
pub async fn with_session_context<'a, T, F>(
    pool: &'a PgPool,
    schema: &'a ResolvedSchema,
    role: RoleClass,
    identity: &'a Identity,
    f: F,
) -> Result<T, sqlx::Error>
where
    F: for<'t> FnOnce(
        &'t mut Transaction<'_, Postgres>,
    ) -> Pin<Box<dyn Future<Output = Result<T, sqlx::Error>> + Send + 't>>,
    T: Send,
{
    let role_name = role.for_schema(schema);
    // Defensive: role names come from operator-validated identifiers but we
    // re-check here so this helper is safe under future refactors.
    if !validate_role_ident(role_name) {
        return Err(sqlx::Error::Protocol(format!("invalid role identifier `{role_name}`")));
    }

    let mut tx = pool.begin().await?;

    // SET LOCAL ROLE — identifier interpolation is required (Postgres does
    // not accept $1 for role names). `role_name` is validated above.
    sqlx::query(&format!("SET LOCAL ROLE {role_name}")).execute(&mut *tx).await?;

    // SET LOCAL app.current_user via set_config() so the value can be bound.
    sqlx::query("SELECT set_config('app.current_user', $1, true)")
        .bind(&identity.actor_id)
        .execute(&mut *tx)
        .await?;

    // Layer-7 row-scope: encode the caller's role-scope into
    // `app.scoped_roles`. The per-table policies the operator emits
    // read this setting and grant access accordingly. A `Broken`
    // rowFilter on the schema surfaces here as a protocol error so the
    // transaction rolls back rather than committing a wide-open read.
    let scoped_roles =
        velocity_core::row_filter::scoped_roles_for_session(schema, identity).map_err(|e| {
            sqlx::Error::Protocol(format!("rowFilter broken on role `{}`: {}", e.role, e.reason))
        })?;
    sqlx::query("SELECT set_config('app.scoped_roles', $1, true)")
        .bind(&scoped_roles)
        .execute(&mut *tx)
        .await?;

    for (key, value) in &identity.attributes {
        if !validate_attr_key(key) {
            return Err(sqlx::Error::Protocol(format!("invalid identity attribute key `{key}`")));
        }
        let setting = format!("app.{key}");
        sqlx::query("SELECT set_config($1, $2, true)")
            .bind(&setting)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }

    let result = f(&mut tx).await?;
    tx.commit().await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_types::common::SchemaPath;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    fn schema() -> ResolvedSchema {
        let spec = SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "acme-platform".into(),
                },
                overrides: Vec::new(),
            },
            access: AccessSpec::default(),
            fields: Vec::new(),
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        };
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        ResolvedSchema::from_spec(path, spec)
    }

    #[test]
    fn role_class_picks_the_right_role() {
        let s = schema();
        assert_eq!(RoleClass::Reader.for_schema(&s), "acme_supply_chain_procurement_reader");
        assert_eq!(RoleClass::Writer.for_schema(&s), "acme_supply_chain_procurement_writer");
        assert_eq!(RoleClass::Admin.for_schema(&s), "acme_supply_chain_procurement_admin");
    }

    #[test]
    fn validators_reject_bad_idents() {
        assert!(validate_role_ident("acme_reader"));
        assert!(!validate_role_ident(""));
        assert!(!validate_role_ident("acme; DROP TABLE x"));
        assert!(!validate_role_ident("AcmeReader")); // uppercase
        assert!(validate_attr_key("store_id"));
        assert!(!validate_attr_key("store-id"));
    }
}
