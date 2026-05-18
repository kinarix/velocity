//! `velocity drift ...` — detect Postgres state that has diverged from
//! the declared SchemaDefinition CRDs, and (optionally) move orphaned
//! tables aside so a future SchemaDefinition apply doesn't collide.
//!
//! v1 scope: orphan tables only. A table is an orphan if its
//! `<pg_schema>.<pg_table>` does NOT correspond to any declared
//! `SchemaDefinition.spec` (or its companion `_history` / `_outbox`
//! tables). Column drift and missing-index detection live in the
//! follow-up phase — they require reusing the operator's `DdlBuilder`
//! to recompute the expected shape, which we don't pull into the CLI
//! crate yet.
//!
//! Quarantine: move the table to schema `platform_quarantine` with a
//! timestamp suffix so multiple quarantines of the same name coexist.
//! Quarantined tables remain queryable for forensics until an operator
//! drops the `platform_quarantine` schema.

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use kube::api::ListParams;
use kube::{Api, Client, Config};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::HashSet;
use velocity_types::common::sanitize;
use velocity_types::crds::schema::SchemaDefinition;

use crate::output::{self, OutputFormat};

#[derive(Debug, Subcommand)]
pub(crate) enum DriftCmd {
    /// Detect orphan tables (Postgres tables that no SchemaDefinition
    /// claims). Read-only.
    Check {
        /// Filter to a single namespace's SchemaDefinitions. Without
        /// this, all namespaces are scanned.
        #[arg(short, long)]
        namespace: Option<String>,
    },
    /// Move an orphan table to the `platform_quarantine` schema with a
    /// timestamp suffix. Requires `UPDATE` on `pg_class` (typically the
    /// `velocity_operator` role).
    Quarantine {
        /// `<pg_schema>.<table>` to quarantine. Must exist; the CLI
        /// refuses to quarantine a table that IS claimed by a current
        /// SchemaDefinition (those must be removed via CRD delete).
        target: String,
    },
}

pub(crate) async fn run(
    cmd: DriftCmd,
    db_url: Option<&str>,
    kubeconfig: &Option<String>,
    output: OutputFormat,
) -> Result<()> {
    match cmd {
        DriftCmd::Check { namespace } => check(db_url, kubeconfig, namespace, output).await,
        DriftCmd::Quarantine { target } => quarantine(db_url, kubeconfig, &target).await,
    }
}

/// One row in the `drift check` output.
#[derive(Debug)]
struct OrphanTable {
    pg_schema: String,
    table: String,
    size_bytes: i64,
    row_estimate: i64,
}

async fn check(
    db_url: Option<&str>,
    kubeconfig: &Option<String>,
    namespace: Option<String>,
    output: OutputFormat,
) -> Result<()> {
    let pool = connect_pg(db_url).await?;
    let client = build_kube_client(kubeconfig.as_deref()).await?;
    let expected = expected_tables(&client, namespace.as_deref()).await?;
    let known_schemas: HashSet<String> =
        expected.iter().map(|(s, _)| s.clone()).collect();

    // Restrict the actual-side query to schemas we already know are
    // Velocity-managed (anchored on the SchemaDefinitions we observed).
    // This avoids ever reporting non-Velocity Postgres tables as
    // orphans — those belong to some other workload sharing the
    // database.
    if known_schemas.is_empty() {
        eprintln!("no SchemaDefinitions found — nothing to compare against");
        output::print(&["PG_SCHEMA", "TABLE", "SIZE_BYTES", "ROWS"], &[], output);
        return Ok(());
    }

    let schema_list: Vec<String> = known_schemas.iter().cloned().collect();
    let actual = actual_tables(&pool, &schema_list).await?;

    let mut orphans: Vec<OrphanTable> = Vec::new();
    for row in actual {
        if !expected.contains(&(row.pg_schema.clone(), row.table.clone())) {
            orphans.push(row);
        }
    }

    let rows: Vec<Vec<String>> = orphans
        .iter()
        .map(|o| {
            vec![
                o.pg_schema.clone(),
                o.table.clone(),
                o.size_bytes.to_string(),
                o.row_estimate.to_string(),
            ]
        })
        .collect();

    eprintln!(
        "drift check: {} Velocity schemas, {} orphan tables",
        known_schemas.len(),
        orphans.len()
    );
    output::print(
        &["PG_SCHEMA", "TABLE", "SIZE_BYTES", "ROWS"],
        &rows,
        output,
    );

    if orphans.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("{} orphan table(s) detected", orphans.len()))
    }
}

async fn quarantine(
    db_url: Option<&str>,
    kubeconfig: &Option<String>,
    target: &str,
) -> Result<()> {
    let (pg_schema, table) = parse_target(target)?;

    // Refuse if the table IS claimed — quarantining a live SchemaDef's
    // table would break the API. Force-removal must go through CRD
    // delete, which the operator handles correctly.
    let client = build_kube_client(kubeconfig.as_deref()).await?;
    let expected = expected_tables(&client, None).await?;
    if expected.contains(&(pg_schema.clone(), table.clone())) {
        return Err(anyhow!(
            "{pg_schema}.{table} IS claimed by a SchemaDefinition — delete the CRD instead"
        ));
    }

    let pool = connect_pg(db_url).await?;

    // Both DDL statements run in one tx so a failure in either leaves
    // the table where it started. `CREATE SCHEMA IF NOT EXISTS` is
    // idempotent — safe to run on every quarantine call.
    let mut tx = pool.begin().await.context("begin tx")?;
    sqlx::query("CREATE SCHEMA IF NOT EXISTS platform_quarantine")
        .execute(&mut *tx)
        .await
        .context("create platform_quarantine schema")?;

    // Sanitize identifiers defensively even though both came from
    // parsed `<schema>.<table>`. The CLI is a privileged tool but
    // operator habits should still surface SQL only after validation.
    if !is_safe_ident(&pg_schema) || !is_safe_ident(&table) {
        return Err(anyhow!(
            "refusing to quarantine `{pg_schema}.{table}` — invalid identifier"
        ));
    }
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let new_name = format!("{pg_schema}__{table}__{stamp}");
    if !is_safe_ident(&new_name) {
        return Err(anyhow!("internal: generated quarantine name not safe"));
    }

    // Two steps: rename then move. We can't do both in a single
    // ALTER TABLE in Postgres, but they share a tx so partial failure
    // is impossible.
    sqlx::query(&format!(
        "ALTER TABLE {pg_schema}.{table} RENAME TO {new_name}"
    ))
    .execute(&mut *tx)
    .await
    .with_context(|| format!("rename {pg_schema}.{table}"))?;

    sqlx::query(&format!(
        "ALTER TABLE {pg_schema}.{new_name} SET SCHEMA platform_quarantine"
    ))
    .execute(&mut *tx)
    .await
    .with_context(|| format!("move {pg_schema}.{new_name} to platform_quarantine"))?;

    tx.commit().await.context("commit quarantine")?;

    eprintln!(
        "quarantined {pg_schema}.{table} -> platform_quarantine.{new_name}"
    );
    Ok(())
}

/// All `<pg_schema>.<table>` pairs the declared SchemaDefinition set
/// expects to exist. Includes the main, `_history`, and `_outbox`
/// (Tier-3 search) tables.
async fn expected_tables(
    client: &Client,
    namespace: Option<&str>,
) -> Result<HashSet<(String, String)>> {
    let api: Api<SchemaDefinition> = if let Some(ns) = namespace {
        Api::namespaced(client.clone(), ns)
    } else {
        Api::all(client.clone())
    };
    let list = api.list(&ListParams::default()).await.context("listing SchemaDefinitions")?;

    let mut out: HashSet<(String, String)> = HashSet::new();
    for sd in list.items {
        // Org/app/domain come from labels the operator sets on every
        // SchemaDefinition; if a CRD landed without them the schema
        // never provisioned in the first place, so we skip it (no
        // expected tables) rather than guess.
        let labels = sd.metadata.labels.clone().unwrap_or_default();
        let (Some(org), Some(app), Some(domain)) = (
            labels.get("velocity.sh/org"),
            labels.get("velocity.sh/app"),
            labels.get("velocity.sh/domain"),
        ) else {
            continue;
        };
        let pg_schema = sanitize(&format!("{org}_{app}_{domain}"));

        // Object name comes from the CRD's `metadata.name`; version
        // from `spec.version`. Sanitisation mirrors the operator's
        // `ddl_builder` rules.
        let Some(object) = sd.metadata.name.as_ref() else {
            continue;
        };
        let table = format!("{}_{}", sanitize(object), sanitize(&sd.spec.version));

        out.insert((pg_schema.clone(), table.clone()));
        out.insert((pg_schema.clone(), format!("{table}_history")));
        // Outbox table only exists for Tier-3 search; for simplicity,
        // mark it as "expected if present, not orphaned otherwise" by
        // including it unconditionally — a real Tier-3 schema has it,
        // a Tier-1/2 schema simply won't have a row matching, so the
        // diff is neutral.
        out.insert((pg_schema, format!("{table}_outbox")));
    }
    Ok(out)
}

async fn actual_tables(pool: &PgPool, schemas: &[String]) -> Result<Vec<OrphanTable>> {
    // `pg_class.reltuples` is the stat-collector estimate, not a live
    // count. Cheap and accurate enough for "is this orphan worth
    // keeping?" — exact COUNT(*) would be impractical on large tables.
    let rows = sqlx::query(
        "SELECT n.nspname::text AS pg_schema, \
                c.relname::text AS table_name, \
                pg_total_relation_size(c.oid)::bigint AS size_bytes, \
                c.reltuples::bigint AS row_estimate \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname = ANY($1) \
         ORDER BY n.nspname, c.relname",
    )
    .bind(schemas)
    .fetch_all(pool)
    .await
    .context("listing actual tables")?;

    Ok(rows
        .into_iter()
        .map(|r| OrphanTable {
            pg_schema: r.get("pg_schema"),
            table: r.get("table_name"),
            size_bytes: r.get("size_bytes"),
            row_estimate: r.get("row_estimate"),
        })
        .collect())
}

fn parse_target(s: &str) -> Result<(String, String)> {
    match s.split_once('.') {
        Some((schema, table)) if !schema.is_empty() && !table.is_empty() => {
            Ok((schema.to_string(), table.to_string()))
        }
        _ => Err(anyhow!(
            "invalid target `{s}` (expected `<pg_schema>.<table>`)"
        )),
    }
}

/// Defensive check used before identifiers are interpolated into DDL.
/// Same character set the operator's `sanitize` produces — we only
/// trust ascii lowercase + digits + underscore.
fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

async fn connect_pg(db_url: Option<&str>) -> Result<PgPool> {
    let url = db_url
        .ok_or_else(|| anyhow!("--db-url or VELOCITY_PG_URL is required for `drift`"))?;
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await
        .context("connecting to Postgres")
}

async fn build_kube_client(kubeconfig: Option<&str>) -> Result<Client> {
    if let Some(path) = kubeconfig {
        let cfg_file = std::fs::read_to_string(path)
            .with_context(|| format!("reading kubeconfig at {path}"))?;
        let kubeconfig: kube::config::Kubeconfig =
            serde_yaml::from_str(&cfg_file).context("parsing kubeconfig YAML")?;
        let config = Config::from_custom_kubeconfig(kubeconfig, &Default::default())
            .await
            .context("building kube config from --kubeconfig")?;
        Client::try_from(config).context("building kube client")
    } else {
        Client::try_default()
            .await
            .context("building kube client (no kubeconfig — using default discovery)")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_target_ok() {
        let (s, t) = parse_target("acme_supply_procurement.purchase_order_v1").unwrap();
        assert_eq!(s, "acme_supply_procurement");
        assert_eq!(t, "purchase_order_v1");
    }

    #[test]
    fn parse_target_rejects_garbage() {
        assert!(parse_target("nodot").is_err());
        assert!(parse_target(".table").is_err());
        assert!(parse_target("schema.").is_err());
    }

    #[test]
    fn is_safe_ident_examples() {
        assert!(is_safe_ident("acme_supply_procurement"));
        assert!(is_safe_ident("purchase_order_v1"));
        assert!(!is_safe_ident("Drop Table"));
        assert!(!is_safe_ident(""));
        assert!(!is_safe_ident("--; DROP TABLE foo --"));
        // 64-char limit (Postgres NAMEDATALEN is 63 + null terminator)
        let long: String = "a".repeat(64);
        assert!(!is_safe_ident(&long));
    }
}
