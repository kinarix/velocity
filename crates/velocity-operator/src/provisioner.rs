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

use crate::ddl_builder::DdlPlan;
use crate::migration_diff::{
    classify, diff_columns, fetch_existing_columns, fetch_existing_fts_hash, fts_comment_sql,
    fts_expression_hash, fts_migration_ops, DiffError, MigrationOp,
};

#[derive(Debug, Error)]
pub enum ProvisionError {
    #[error("invalid identifier `{ident}`: must match [a-z0-9_]{{1,63}}")]
    InvalidIdentifier { ident: String },

    #[error("missing required label `{label}` on resource `{name}`")]
    MissingLabel { label: String, name: String },

    #[error("postgres error: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("parent Domain schema `{0}` does not exist — Domain must reconcile first")]
    DomainNotProvisioned(String),

    #[error("breaking schema change rejected: {0:?}")]
    BreakingChange(Vec<MigrationOp>),

    #[error(
        "breaking schema change recognised but not yet executable (deferred to Phase 2+): {0:?}"
    )]
    BreakingChangeDeferred(Vec<MigrationOp>),
}

impl From<DiffError> for ProvisionError {
    fn from(e: DiffError) -> Self {
        match e {
            DiffError::BreakingOpsBlocked(ops) => ProvisionError::BreakingChange(ops),
            DiffError::BreakingOpsDeferred(ops) => ProvisionError::BreakingChangeDeferred(ops),
            DiffError::Sql(e) => ProvisionError::Sql(e),
        }
    }
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

        // Serialize this transaction against any other domain provisioning
        // running concurrently against the same Postgres. We touch the
        // shared `pg_roles` / `pg_default_acl` system catalogs (CREATE
        // ROLE + GRANT + ALTER DEFAULT PRIVILEGES), and two concurrent
        // transactions racing on those catalogs can return "tuple
        // concurrently updated" (XX000) — Postgres won't serialize them
        // for us because the rows being modified aren't user data. An
        // advisory xact lock on a constant key forces strict ordering.
        // Lock is released at COMMIT/ROLLBACK; the constant is arbitrary
        // (just needs to be the same across all callers).
        sqlx::query("SELECT pg_advisory_xact_lock(7610358901234567890)")
            .execute(&mut *tx)
            .await?;

        // 1. Schema
        exec(&mut tx, &format!("CREATE SCHEMA IF NOT EXISTS {schema}")).await?;

        // 2. Roles (NOLOGIN; SET ROLE switches into them per-tx)
        create_role_if_absent(&mut tx, &reader).await?;
        create_role_if_absent(&mut tx, &writer).await?;
        create_role_if_absent(&mut tx, &admin).await?;

        // Grant velocity_api membership in each domain role so the API can
        // `SET LOCAL ROLE` into the per-request role at handler entry
        // (ADR-007). The membership is what makes RLS effective — the API
        // never runs queries as velocity_api itself (which is NOBYPASSRLS
        // anyway), it always drops into the domain role first.
        for role in [&reader, &writer, &admin] {
            exec(&mut tx, &format!("GRANT {role} TO velocity_api")).await?;
        }

        // 3. Schema-level grants
        for role in [&reader, &writer, &admin] {
            exec(&mut tx, &format!("GRANT USAGE ON SCHEMA {schema} TO {role}")).await?;
        }
        exec(&mut tx, &format!("GRANT USAGE ON SCHEMA {schema} TO velocity_api")).await?;
        exec(&mut tx, &format!("GRANT USAGE, CREATE ON SCHEMA {schema} TO velocity_operator"))
            .await?;

        // Per-domain roles must also reach `platform.*` to write the audit
        // and event_log rows that every mutation produces. The handler
        // executes `SET LOCAL ROLE {domain_role}` (ADR-007) and the audit
        // / event-log writes happen inside that role's transaction — so
        // the domain role itself needs USAGE on platform plus INSERT on
        // event_log and EXECUTE on `audit_insert`. velocity_api having
        // these grants is insufficient; `GRANT role TO velocity_api` makes
        // velocity_api a *member* of the domain role, so privileges flow
        // from domain → velocity_api, not the other direction.
        for role in [&reader, &writer, &admin] {
            exec(&mut tx, &format!("GRANT USAGE ON SCHEMA platform TO {role}")).await?;
            // Read access to event_log so the time-machine endpoints —
            // which run under the same SET LOCAL ROLE — can SELECT from it.
            exec(
                &mut tx,
                &format!("GRANT SELECT ON platform.event_log TO {role}"),
            )
            .await?;
        }
        // Writer / admin can append events; reader cannot. Restore lives
        // on writer (it produces a new event), so the writer grant covers
        // both standard CRUD and restore paths.
        for role in [&writer, &admin] {
            exec(
                &mut tx,
                &format!("GRANT INSERT ON platform.event_log TO {role}"),
            )
            .await?;
            // Same lifetime as the writes above: audit_insert is the only
            // way the audit_log table accepts new rows. EXECUTE on the
            // function is what gates this for the domain role.
            exec(
                &mut tx,
                &format!(
                    "GRANT EXECUTE ON FUNCTION platform.audit_insert( \
                         TEXT, TEXT, TEXT, TEXT, UUID, JSONB, JSONB, TEXT, TEXT, TEXT \
                     ) TO {role}"
                ),
            )
            .await?;
            // Idempotency-key insert / lookup happens in the same tx as
            // the user-visible write, so the domain role needs SELECT
            // + INSERT + UPDATE on platform.idempotency_keys too.
            exec(
                &mut tx,
                &format!(
                    "GRANT SELECT, INSERT, UPDATE ON platform.idempotency_keys TO {role}"
                ),
            )
            .await?;
        }

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

    /// Provision the per-schema tables (main + history + optional outbox) from
    /// a [`DdlPlan`]. Idempotent.
    ///
    /// First run: executes the full plan (CREATE TABLE / INDEX / TRIGGER).
    /// Subsequent runs: diffs target columns against the live table and
    /// applies safe ALTER statements; breaking ops are rejected unless
    /// `allow_breaking` is true (caller decides based on the
    /// `velocity.sh/breaking-change: approved` annotation).
    pub async fn sync_schema_tables(
        &self,
        plan: &DdlPlan,
        allow_breaking: bool,
    ) -> Result<ProvisionedSchema, ProvisionError> {
        let (pg_schema, table) = split_qualified(&plan.qualified_table)?;

        // Refuse to provision tables if the parent Domain schema is missing —
        // running CREATE TABLE in that case would create a schema implicitly
        // and bypass the Domain reconciler's role/grant setup.
        let schema_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
        )
        .bind(&pg_schema)
        .fetch_one(&self.pool)
        .await?;
        if !schema_exists {
            return Err(ProvisionError::DomainNotProvisioned(pg_schema));
        }

        let existing = fetch_existing_columns(&self.pool, &pg_schema, &table).await?;

        let mut tx = self.pool.begin().await?;

        if existing.is_empty() {
            // First-run path — execute the full plan.
            exec(&mut tx, &plan.main_table).await?;
            // Phase 5d — stamp the FTS-spec hash on the column so a
            // later reconcile can tell our expression apart from a
            // drift-edited one. Same statement runs on both first-run
            // and migration paths, so we only need to remember the
            // hash once.
            if let Some(expr) = &plan.fts_expression {
                let hash = fts_expression_hash(expr);
                exec(&mut tx, &fts_comment_sql(&plan.qualified_table, &hash)).await?;
            }
            exec(&mut tx, &plan.history_table).await?;
            if let Some(outbox) = &plan.outbox_table {
                for stmt in split_statements(outbox) {
                    exec(&mut tx, &stmt).await?;
                }
            }
            for stmt in &plan.indexes {
                exec(&mut tx, stmt).await?;
            }
            for stmt in &plan.triggers {
                for s in split_statements(stmt) {
                    exec(&mut tx, &s).await?;
                }
            }
        } else {
            // Migrate path — diff and apply safe ops.
            let ops = diff_columns(&plan.columns, &existing);
            let migrations = classify(&plan.qualified_table, ops, allow_breaking)?;
            for stmt in migrations {
                exec(&mut tx, &stmt).await?;
            }
            // Phase 5d — reconcile the `__fts` generated column.
            // Hash compare against the velocity-stamped COMMENT; if
            // missing (legacy Phase-5b table) or mismatched (weight
            // / field-set change), DROP CASCADE + ADD COLUMN rebuilds
            // it. CASCADE drops the GIN index too — the standard
            // index pass below re-creates it.
            //
            // Reads fetch_existing_fts_hash *outside* the open tx by
            // intent: the catalogs (pg_attribute, pg_description)
            // don't change between the snapshot we read and the
            // ALTER we issue inside the tx — and even if they did,
            // an out-of-date hash just produces one extra rebuild,
            // never a wrong rebuild.
            let live_hash =
                fetch_existing_fts_hash(&self.pool, &pg_schema, &table).await?;
            let fts_ops = fts_migration_ops(
                &plan.qualified_table,
                plan.fts_expression.as_deref(),
                live_hash,
            );
            for stmt in fts_ops {
                exec(&mut tx, &stmt).await?;
            }
            // Always re-create triggers + indexes (idempotent) so new fields
            // pick up indexes and refactored triggers replace the old ones.
            for stmt in &plan.indexes {
                exec(&mut tx, stmt).await?;
            }
            for stmt in &plan.triggers {
                for s in split_statements(stmt) {
                    exec(&mut tx, &s).await?;
                }
            }
        }

        // Layer-7 RLS — applied on every reconcile (drop+create style) so
        // a `rowFilter[]` edit replaces the predicate set cleanly. The
        // operator owns the tables, so it bypasses RLS for the DDL
        // itself; future migration backfills (e.g. DROP COLUMN) rely on
        // that, which is why we don't `FORCE ROW LEVEL SECURITY`.
        for stmt in &plan.rls_policies {
            exec(&mut tx, stmt).await?;
        }

        tx.commit().await?;

        Ok(ProvisionedSchema {
            pg_schema,
            pg_table: table,
            qualified: plan.qualified_table.clone(),
        })
    }
}

/// Result of [`PostgresProvisioner::sync_schema_tables`].
#[derive(Debug, Clone)]
pub struct ProvisionedSchema {
    pub pg_schema: String,
    pub pg_table: String,
    pub qualified: String,
}

fn split_qualified(qualified: &str) -> Result<(String, String), ProvisionError> {
    let (schema, table) = qualified
        .split_once('.')
        .ok_or_else(|| ProvisionError::InvalidIdentifier { ident: qualified.to_string() })?;
    Ok((schema.to_string(), table.to_string()))
}

/// Some DDL "statements" we generate are actually multiple semicolon-terminated
/// statements (e.g. the outbox CREATE TABLE + its index). Postgres' simple
/// query protocol can handle that, but for clarity (and so error reporting
/// points at the right statement) we split before execution.
///
/// PL/pgSQL function bodies contain semicolons too — we detect `$$ ... $$`
/// dollar-quoted blocks and pass them through untouched.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_dollar = false;
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            in_dollar = !in_dollar;
            cur.push_str("$$");
            i += 2;
            continue;
        }
        if c == ';' && !in_dollar {
            let trimmed = cur.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            cur.clear();
        } else {
            cur.push(c);
        }
        i += 1;
    }
    let trimmed = cur.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
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
