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

use crate::ddl_builder::{ColumnSpec, DdlPlan};
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

/// Result of [`PostgresProvisioner::sync_archive_schema`] — the cold-tier
/// destination for archived rows. Schema is a sibling of the hot domain
/// schema with an `_archive` suffix (matching the `archived_at` /
/// `archive_ref` system-column convention from `ddl_builder`); roles
/// mirror the hot tier so the eventual archive worker can run with the
/// same role-switching discipline.
#[derive(Debug, Clone)]
pub struct ProvisionedArchiveSchema {
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
        sqlx::query("SELECT pg_advisory_xact_lock(7610358901234567890)").execute(&mut *tx).await?;

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
            exec(&mut tx, &format!("GRANT SELECT ON platform.event_log TO {role}")).await?;
        }
        // Writer / admin can append events; reader cannot. Restore lives
        // on writer (it produces a new event), so the writer grant covers
        // both standard CRUD and restore paths.
        for role in [&writer, &admin] {
            exec(&mut tx, &format!("GRANT INSERT ON platform.event_log TO {role}")).await?;
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
                &format!("GRANT SELECT, INSERT, UPDATE ON platform.idempotency_keys TO {role}"),
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

    /// Provision the archive destination schema for a domain that has an
    /// `ArchivePolicy` with `destination.backend = postgres-cold`. Idempotent.
    ///
    /// Layout mirrors `sync_domain`:
    ///
    /// - schema name: `<org>_<app>_<domain>_archive`
    /// - roles:       `<schema>_reader`, `<schema>_writer`, `<schema>_admin`
    /// - `velocity_api` is granted membership in each role so reads from the
    ///   API (archive lookup, GET /{id}/archive) can `SET LOCAL ROLE` into
    ///   the archive reader the same way hot reads do.
    ///
    /// What's NOT here yet: default privileges for SEQUENCES (archive tables
    /// don't take new sequences — rows arrive with their original id), and
    /// `platform.*` grants (the archive tier is mutation-free from the
    /// user's perspective, so audit/event-log writes don't happen under
    /// the archive role).
    pub async fn sync_archive_schema(
        &self,
        org: &str,
        app: &str,
        domain: &str,
    ) -> Result<ProvisionedArchiveSchema, ProvisionError> {
        let org_id = validate_ident(&sanitize(org))?;
        let app_id = validate_ident(&sanitize(app))?;
        let dom_id = validate_ident(&sanitize(domain))?;

        let schema = format!("{org_id}_{app_id}_{dom_id}_archive");
        validate_ident(&schema)?;

        let reader = format!("{schema}_reader");
        let writer = format!("{schema}_writer");
        let admin = format!("{schema}_admin");
        for role in [&reader, &writer, &admin] {
            validate_ident(role)?;
        }

        let mut tx = self.pool.begin().await?;

        // Same advisory-lock pattern as sync_domain — concurrent reconciles
        // racing on pg_roles / pg_default_acl can return "tuple concurrently
        // updated". Reuse the same lock key so cold + hot provisioning of
        // different domains can't overlap on these system catalogs either.
        sqlx::query("SELECT pg_advisory_xact_lock(7610358901234567890)").execute(&mut *tx).await?;

        exec(&mut tx, &format!("CREATE SCHEMA IF NOT EXISTS {schema}")).await?;

        create_role_if_absent(&mut tx, &reader).await?;
        create_role_if_absent(&mut tx, &writer).await?;
        create_role_if_absent(&mut tx, &admin).await?;

        for role in [&reader, &writer, &admin] {
            exec(&mut tx, &format!("GRANT {role} TO velocity_api")).await?;
            exec(&mut tx, &format!("GRANT USAGE ON SCHEMA {schema} TO {role}")).await?;
        }
        exec(&mut tx, &format!("GRANT USAGE ON SCHEMA {schema} TO velocity_api")).await?;
        exec(&mut tx, &format!("GRANT USAGE, CREATE ON SCHEMA {schema} TO velocity_operator"))
            .await?;

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
                 GRANT SELECT, INSERT ON TABLES TO {writer}, velocity_api"
            ),
        )
        .await?;
        exec(
            &mut tx,
            &format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT SELECT, INSERT, DELETE ON TABLES TO {admin}"
            ),
        )
        .await?;

        tx.commit().await?;

        Ok(ProvisionedArchiveSchema { pg_schema: schema, pg_roles: vec![reader, writer, admin] })
    }

    /// Provision a mirror of a hot table inside the archive schema.
    /// Column-for-column copy (excluding generated columns like `__fts`),
    /// primary key on `id`, no foreign keys or indexes. Idempotent — uses
    /// `CREATE TABLE IF NOT EXISTS` so re-runs are no-ops.
    ///
    /// Called by the `ArchivePolicy` reconciler once the archive schema
    /// exists; gives the eventual archive worker a target table whose
    /// shape matches the hot table being drained. The mirror does NOT
    /// include a `_history` partner — archived rows are terminal; their
    /// history (if any) is left in the hot history table to be reaped
    /// by the existing retention machinery.
    pub async fn sync_archive_mirror_table(
        &self,
        archive_schema: &str,
        table: &str,
        columns: &[ColumnSpec],
    ) -> Result<(), ProvisionError> {
        let schema_id = validate_ident(archive_schema)?;
        let table_id = validate_ident(table)?;
        let ddl = build_archive_mirror_ddl(&schema_id, &table_id, columns);

        let mut tx = self.pool.begin().await?;
        // Same advisory lock as sync_archive_schema so a mirror table
        // creation can't race with the schema-level grants being applied.
        sqlx::query("SELECT pg_advisory_xact_lock(7610358901234567890)").execute(&mut *tx).await?;
        exec(&mut tx, &ddl).await?;
        tx.commit().await?;
        Ok(())
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
            let live_hash = fetch_existing_fts_hash(&self.pool, &pg_schema, &table).await?;
            let fts_ops =
                fts_migration_ops(&plan.qualified_table, plan.fts_expression.as_deref(), live_hash);
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

/// Render the `CREATE TABLE IF NOT EXISTS` for an archive-tier mirror.
///
/// Pure (no DB) so it can be unit-tested. Column order follows the input,
/// every column is `NOT NULL` if the source was, type follows the source
/// `base_type` (with optional `(length)`), and there are no defaults — the
/// archive worker is expected to copy `id`, `created_at`, etc. verbatim
/// from the hot row, so server-side defaults aren't useful and would just
/// surprise anyone running a manual `INSERT` against the archive.
///
/// The mirror has no `__fts` generated column (search is hot-only), no
/// indexes (added later if archive queries demand them), and no foreign
/// keys (cross-domain refs may not exist in the archive tier).
pub fn build_archive_mirror_ddl(schema: &str, table: &str, columns: &[ColumnSpec]) -> String {
    let cols: Vec<String> = columns
        .iter()
        .map(|c| {
            let ty = match c.length {
                Some(n) if c.base_type == "varchar" => format!("varchar({n})"),
                _ => c.base_type.clone(),
            };
            let null = if c.not_null { " NOT NULL" } else { "" };
            format!("    {} {}{}", c.name, ty, null)
        })
        .collect();

    let mut body = cols.join(",\n");
    if columns.iter().any(|c| c.name == "id") {
        body.push_str(",\n    PRIMARY KEY (id)");
    }

    format!("CREATE TABLE IF NOT EXISTS {schema}.{table} (\n{body}\n);")
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

    #[test]
    fn diff_error_into_provision_error_covers_every_arm() {
        // Each arm of `From<DiffError> for ProvisionError`. The reconciler
        // tests cover the BreakingChange branches end-to-end against a
        // live DB, but the `DiffError::Sql` arm only fires when sqlx
        // returns an error mid-diff — easier to construct directly.
        use crate::DiffError;

        let blocked: ProvisionError = DiffError::BreakingOpsBlocked(vec![]).into();
        assert!(matches!(blocked, ProvisionError::BreakingChange(_)));

        let deferred: ProvisionError = DiffError::BreakingOpsDeferred(vec![]).into();
        assert!(matches!(deferred, ProvisionError::BreakingChangeDeferred(_)));

        let sql_err = sqlx::Error::PoolTimedOut;
        let sql: ProvisionError = DiffError::Sql(sql_err).into();
        assert!(matches!(sql, ProvisionError::Sql(_)));
    }

    fn col(name: &str, ty: &str, not_null: bool) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            base_type: ty.into(),
            length: None,
            not_null,
            system: false,
        }
    }

    #[test]
    fn archive_mirror_ddl_basic_shape() {
        let cols = vec![
            col("id", "uuid", true),
            col("created_at", "timestamptz", true),
            col("po_number", "text", true),
            col("notes", "text", false),
        ];
        let ddl = build_archive_mirror_ddl("acme_sc_proc_archive", "purchase_order_v1", &cols);
        assert!(
            ddl.starts_with("CREATE TABLE IF NOT EXISTS acme_sc_proc_archive.purchase_order_v1 (")
        );
        assert!(ddl.contains("id uuid NOT NULL"));
        assert!(ddl.contains("created_at timestamptz NOT NULL"));
        assert!(ddl.contains("po_number text NOT NULL"));
        assert!(ddl.contains("notes text,"));
        assert!(ddl.contains("PRIMARY KEY (id)"));
        assert!(ddl.trim_end().ends_with(");"));
    }

    #[test]
    fn archive_mirror_ddl_renders_varchar_with_length() {
        let mut c = col("po_number", "varchar", true);
        c.length = Some(32);
        let ddl = build_archive_mirror_ddl("acme_sc_proc_archive", "po_v1", &[c]);
        assert!(ddl.contains("po_number varchar(32) NOT NULL"), "got: {ddl}");
    }

    #[test]
    fn archive_mirror_ddl_no_primary_key_when_no_id_column() {
        let cols = vec![col("name", "text", true)];
        let ddl = build_archive_mirror_ddl("acme_sc_proc_archive", "kv_v1", &cols);
        assert!(!ddl.contains("PRIMARY KEY"));
    }

    #[test]
    fn archive_mirror_ddl_emits_idempotent_create() {
        let cols = vec![col("id", "uuid", true)];
        let ddl = build_archive_mirror_ddl("s", "t", &cols);
        assert!(ddl.contains("IF NOT EXISTS"));
    }
}
