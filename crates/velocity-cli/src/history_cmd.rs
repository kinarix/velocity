//! `velocity history` + `velocity restore` — time-machine ops.
//!
//! Two top-level commands rather than a parent: history is a read,
//! restore is a write, and operators reach for them in different
//! contexts (debugging vs incident response). Keeping them separate
//! also matches what `phases.md` listed as Phase 9 deliverables.
//!
//! `history` has two modes, chosen by the server based on whether
//! `--at` is supplied:
//! - Without `--at`: paginated event listing newest-first
//!   (`--limit`, `--before <iso>`).
//! - With `--at <iso>`: reconstruct the entity state as of T.

use anyhow::{anyhow, Context as _, Result};
use clap::Args;

use crate::client::{ApiClient, SchemaPath};
use crate::config::Config;

#[derive(Debug, Args)]
pub(crate) struct HistoryArgs {
    /// Schema path: `org/app/domain/object/version`.
    pub path: String,
    /// Record id.
    pub id: String,
    /// Max events per page (default 50, server cap 1000 per ADR-009).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Exclusive ISO-8601 cursor: only events strictly older than this.
    #[arg(long)]
    pub before: Option<String>,
    /// Point-in-time mode: reconstruct entity state as of this instant
    /// instead of listing events. ISO-8601.
    #[arg(long)]
    pub at: Option<String>,
}

pub(crate) async fn history(
    args: HistoryArgs,
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<()> {
    let api = build_client(config_path, context_override)?;
    let path = SchemaPath::parse(&args.path)?;
    let v = api
        .get_history(
            &path,
            &args.id,
            args.limit,
            args.before.as_deref(),
            args.at.as_deref(),
        )
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&v).context("serialising history response")?
    );
    Ok(())
}

#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    /// Schema path: `org/app/domain/object/version`.
    pub path: String,
    /// Record id.
    pub id: String,
    /// Restore to this instant (ISO-8601). Must be in the past.
    #[arg(long)]
    pub at: String,
    /// Operator note recorded with the rollback. Defaults to none;
    /// `X-Reason` header equivalent. Provide one for any incident
    /// response — it shows up in the audit trail.
    #[arg(long)]
    pub reason: Option<String>,
    /// Skip the confirmation prompt. Required in non-TTY pipelines.
    #[arg(long)]
    pub yes: bool,
}

pub(crate) async fn restore(
    args: RestoreArgs,
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<()> {
    if !args.yes
        && !crate::confirm::confirm(&format!(
            "restore {} {} to {}? this writes a new event to the audit chain.",
            args.path, args.id, args.at
        ))?
    {
        anyhow::bail!("aborted");
    }

    let api = build_client(config_path, context_override)?;
    let path = SchemaPath::parse(&args.path)?;
    let v = api
        .post_restore(&path, &args.id, &args.at, args.reason.as_deref())
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&v).context("serialising restore response")?
    );
    Ok(())
}

fn build_client(
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
