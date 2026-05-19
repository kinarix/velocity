//! `velocity archive` + `velocity approve` — Phase 8 lifecycle ops.
//!
//! `archive` is a parent command with three data-plane operations:
//!   - `get   <path> <id>`  — fetch a single archived record.
//!   - `query <path> -f -`  — paginated DSL against the archive store.
//!   - `restore <path> <id>` — unarchive (data flows back to hot).
//!
//! `approve <ns> <purge-request-name>` is the human-in-the-loop gate
//! for PurgeRequest CRDs (Phase 8 slice 7). The operator's controller
//! waits for `velocity.sh/approved-by: <human>` before executing the
//! hard DELETE; this command patches that annotation on with the
//! current user's identity (USER env, falling back to "unknown").

use anyhow::{anyhow, bail, Context as _, Result};
use clap::{Args, Subcommand};
use kube::api::{DynamicObject, Patch, PatchParams};
use kube::{Api, Discovery};
use serde_json::{json, Value};

use crate::client::{ApiClient, SchemaPath};
use crate::config::Config;
use crate::kube_helpers::{build_client, find_resource, is_namespaced};

/// The annotation the PurgeRequest controller (operator) watches for.
/// Kept in sync with `APPROVED_BY_ANNOTATION` in the operator —
/// duplicated rather than imported to avoid pulling the operator crate
/// into the CLI build.
const APPROVED_BY_ANNOTATION: &str = "velocity.sh/approved-by";

const FIELD_MANAGER: &str = "velocity-cli";

#[derive(Debug, Subcommand)]
pub(crate) enum ArchiveCmd {
    /// Fetch a single archived record.
    Get {
        path: String,
        id: String,
    },
    /// POST a query DSL body to `/{path}/archive/query`. Body shape:
    /// `{ limit, cursor, archivedAfter }`. CLI forwards bytes.
    Query {
        path: String,
        #[arg(short, long, default_value = "-")]
        file: String,
    },
    /// Move an archived row back to the hot table.
    /// 410 ARCHIVE_HOT_ROW_PURGED means the hot row was already purged
    /// (purgeAfter elapsed); the archive copy is read-only at that point.
    Restore {
        path: String,
        id: String,
    },
}

pub(crate) async fn run(
    cmd: ArchiveCmd,
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<()> {
    let api = build_data_client(config_path, context_override)?;
    match cmd {
        ArchiveCmd::Get { path, id } => {
            let p = SchemaPath::parse(&path)?;
            let v = api.get_archive(&p, &id).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&v).context("serialising archive response")?
            );
        }
        ArchiveCmd::Query { path, file } => {
            let p = SchemaPath::parse(&path)?;
            let body = read_json(&file)?;
            let env = api.query_archive(&p, &body).await?;
            let canon = json!({ "items": env.items, "next_cursor": env.next_cursor });
            println!(
                "{}",
                serde_json::to_string_pretty(&canon).context("serialising archive query")?
            );
        }
        ArchiveCmd::Restore { path, id } => {
            let p = SchemaPath::parse(&path)?;
            let v = api.post_unarchive(&p, &id).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&v).context("serialising unarchive response")?
            );
        }
    }
    Ok(())
}

fn build_data_client(
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<ApiClient> {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => Config::default_path()
            .ok_or_else(|| anyhow!("could not resolve config path (set $VELOCITY_CONFIG)"))?,
    };
    let cfg = Config::load(&path)?;
    let ctx = cfg.resolve(context_override)?;
    ApiClient::from_context(&ctx)
}

fn read_json(source: &str) -> Result<Value> {
    use std::io::Read as _;
    let raw = if source == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("reading body from stdin")?;
        buf
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading body from {source}"))?
    };
    serde_json::from_str(&raw).with_context(|| format!("parsing body from {source}"))
}

// ---------------------------------------------------------------------
// approve
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct ApproveArgs {
    pub namespace: String,
    pub name: String,
    /// Identifier to write into the annotation. Defaults to `$USER`,
    /// then `unknown`. Use a real human identifier — the operator
    /// records this verbatim in the PurgeRequest status and the
    /// downstream audit chain.
    #[arg(long)]
    pub approver: Option<String>,
    #[arg(long)]
    pub yes: bool,
}

pub(crate) async fn approve(args: ApproveArgs, kubeconfig: &Option<String>) -> Result<()> {
    let approver = args
        .approver
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_string());

    if !args.yes
        && !crate::confirm::confirm(&format!(
            "approve PurgeRequest {}/{} as `{approver}`? \
             this triggers a hard DELETE on the archive store.",
            args.namespace, args.name
        ))?
    {
        bail!("aborted");
    }

    let client = build_client(kubeconfig.as_deref()).await?;
    let discovery = Discovery::new(client.clone())
        .run()
        .await
        .context("discovering cluster APIs")?;
    let (ar, caps) = find_resource(&discovery, "PurgeRequest")?;
    if !is_namespaced(&caps) {
        bail!("PurgeRequest is unexpectedly cluster-scoped — check operator version");
    }
    let api: Api<DynamicObject> = Api::namespaced_with(client, &args.namespace, &ar);

    let patch = json!({
        "metadata": {
            "annotations": { APPROVED_BY_ANNOTATION: approver }
        }
    });
    api.patch(
        &args.name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Merge(&patch),
    )
    .await
    .with_context(|| format!("annotating PurgeRequest {}/{}", args.namespace, args.name))?;

    eprintln!(
        "approved: PurgeRequest {}/{} -> {APPROVED_BY_ANNOTATION}={approver}",
        args.namespace, args.name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn read_json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.json");
        std::fs::write(&path, r#"{"limit": 10, "archivedAfter": "2026-01-01T00:00:00Z"}"#).unwrap();
        let v = read_json(path.to_str().unwrap()).unwrap();
        assert_eq!(v["limit"], 10);
    }
}
