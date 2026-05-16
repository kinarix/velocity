//! Postgres provisioning for the HierarchyOperator.
//!
//! Phase 0 scope: when a `Domain` is applied, create its Postgres schema and
//! the three per-domain roles (`<schema>_reader|writer|admin`), grant
//! velocity_api the schema-level USAGE it needs, and record the result on
//! the Domain status.
//!
//! ## Idempotency
//!
//! Every DDL statement uses `CREATE ... IF NOT EXISTS` or a `DO $$` block
//! gated on `pg_roles`. Running a reconcile twice produces the same final
//! state and never errors.
//!
//! ## Safety
//!
//! The provisioner connects as `velocity_operator`. The org/app/domain names
//! pass through [`validate_ident`] before being inserted into DDL — anything
//! outside `[a-z0-9_]{1,63}` is rejected. Roles and schema names are then
//! built from those validated parts, never from raw spec strings.

use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use velocity_types::common::sanitize;

#[derive(Debug, Error)]
pub enum ProvisionError {
    #[error("invalid identifier `{ident}`: must match [a-z0-9_]{{1,63}}")]
    InvalidIdentifier { ident: String },

    #[error("missing required label `{label}` on resource `{name}`")]
    MissingLabel { label: String, name: String },

    #[error("postgres error: {0}")]
    Sql(#[from] sqlx::Error),
}

/// What was provisioned. Returned so the reconciler can write it to status.
#[derive(Debug, Clone)]
pub struct ProvisionedDomain {
    pub pg_schema: String,
    pub pg_roles: Vec<String>,
}

#[derive(Debug)]
pub struct PostgresProvisioner {
    pool: PgPool,
}

impl PostgresProvisioner {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Provision the Postgres state for a Domain. Idempotent.
    pub async fn sync_domain(
        &self,
        org: &str,
        app: &str,
        domain: &str,
    ) -> Result<ProvisionedDomain, ProvisionError> {
        let org_id = validate_ident(&sanitize(org))?;
        let app_id = validate_ident(&sanitize(app))?;
        let dom_id = validate_ident(&sanitize(domain))?;

        let schema = format!("{org_id}_{app_id}_{dom_id}");
        validate_ident(&schema)?;

        let reader = format!("{schema}_reader");
        let writer = format!("{schema}_writer");
        let admin = format!("{schema}_admin");
        for role in [&reader, &writer, &admin] {
            validate_ident(role)?;
        }

        let mut tx = self.pool.begin().await?;

        // 1. Schema
        exec(&mut tx, &format!("CREATE SCHEMA IF NOT EXISTS {schema}")).await?;

        // 2. Roles (NOLOGIN; SET ROLE switches into them per-tx)
        create_role_if_absent(&mut tx, &reader).await?;
        create_role_if_absent(&mut tx, &writer).await?;
        create_role_if_absent(&mut tx, &admin).await?;

        // 3. Schema-level grants
        for role in [&reader, &writer, &admin] {
            exec(&mut tx, &format!("GRANT USAGE ON SCHEMA {schema} TO {role}")).await?;
        }
        exec(&mut tx, &format!("GRANT USAGE ON SCHEMA {schema} TO velocity_api")).await?;
        exec(&mut tx, &format!("GRANT USAGE, CREATE ON SCHEMA {schema} TO velocity_operator"))
            .await?;

        // 4. Default privileges so future tables in this schema are usable
        //    without per-table GRANTs from the SchemaOperator. Issued AS the
        //    operator role — those are the privileges that flow.
        exec(
            &mut tx,
            &format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT SELECT ON TABLES TO {reader}, velocity_api"
            ),
        )
        .await?;
        exec(
            &mut tx,
            &format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT SELECT, INSERT, UPDATE ON TABLES TO {writer}, velocity_api"
            ),
        )
        .await?;
        exec(
            &mut tx,
            &format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {admin}"
            ),
        )
        .await?;
        exec(
            &mut tx,
            &format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT USAGE, SELECT ON SEQUENCES TO {writer}, {admin}, velocity_api"
            ),
        )
        .await?;

        tx.commit().await?;

        Ok(ProvisionedDomain { pg_schema: schema, pg_roles: vec![reader, writer, admin] })
    }
}

async fn exec(tx: &mut Transaction<'_, Postgres>, sql: &str) -> Result<(), sqlx::Error> {
    sqlx::query(sql).execute(&mut **tx).await?;
    Ok(())
}

async fn create_role_if_absent(
    tx: &mut Transaction<'_, Postgres>,
    role: &str,
) -> Result<(), sqlx::Error> {
    // `CREATE ROLE IF NOT EXISTS` doesn't exist in Postgres pre-16; even on 16
    // it does, but we keep the DO-block form for portability.
    let stmt = format!(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN \
             CREATE ROLE {role} NOLOGIN NOSUPERUSER NOBYPASSRLS; \
           END IF; \
         END $$;"
    );
    sqlx::query(&stmt).execute(&mut **tx).await?;
    Ok(())
}

/// Whitelist: only allow `[a-z0-9_]`, 1-63 chars (Postgres' identifier cap).
/// Names go straight into DDL strings, so this MUST be airtight.
pub fn validate_ident(s: &str) -> Result<String, ProvisionError> {
    let ok = !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        // Postgres identifiers can't start with a digit.
        && !s.as_bytes()[0].is_ascii_digit();
    if ok {
        Ok(s.to_string())
    } else {
        Err(ProvisionError::InvalidIdentifier { ident: s.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ident_rejects_bad_input() {
        assert!(validate_ident("").is_err());
        assert!(validate_ident("Foo").is_err()); // uppercase
        assert!(validate_ident("foo-bar").is_err()); // dash
        assert!(validate_ident("123foo").is_err()); // leading digit
        assert!(validate_ident("foo;DROP TABLE x;").is_err());
        assert!(validate_ident(&"a".repeat(64)).is_err());
    }

    #[test]
    fn ident_accepts_normal_names() {
        assert!(validate_ident("acme_supply_chain_procurement").is_ok());
        assert!(validate_ident("x").is_ok());
        assert!(validate_ident(&"a".repeat(63)).is_ok());
    }
}
