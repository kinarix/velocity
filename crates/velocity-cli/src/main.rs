//! `velocity` — operator CLI for Velocity clusters.
//!
//! Read-only commands (`status`, `audit verify`, `drift check`) target
//! kube + Postgres directly. Data-plane commands (Phase 9 slice 3+)
//! target the API server via `ApiClient`, resolved from the active
//! context in `~/.velocity/config`.
//!
//! Phase 9 slice 1 adds:
//!   - `context list|show|use|add|delete` — manage config entries.
//!   - `version`                            — client + server build info.
//!   - `--context` / `--config` global flags.
//!
//! OIDC device flow is **deferred** from Phase 9; today, `context add`
//! takes a bearer token verbatim. The on-disk file is `0600` on Unix.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod audit;
mod client;
mod config;
mod confirm;
mod context_cmd;
mod drift;
mod history_cmd;
mod kube_cmd;
mod kube_helpers;
mod output;
mod reconcile;
mod record_cmd;
mod status;
mod version_cmd;

use audit::AuditCmd;
use context_cmd::ContextCmd;
use drift::DriftCmd;
use history_cmd::{HistoryArgs, RestoreArgs};
use kube_cmd::{ApplyArgs, DeleteArgs, DescribeArgs, DiffArgs, GetArgs};
use output::OutputFormat;
use reconcile::ReconcileArgs;
use record_cmd::RecordCmd;
use status::StatusArgs;
use version_cmd::VersionArgs;

/// velocity — CLI for managing a Velocity deployment.
#[derive(Debug, Parser)]
#[command(name = "velocity", version, about, long_about = None)]
struct Cli {
    /// Output format. `table` is human-readable; `json` is stable for
    /// scripting. Honoured by every subcommand that emits rows.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Table)]
    output: OutputFormat,

    /// Kubeconfig path. Defaults to `$KUBECONFIG` then `~/.kube/config`,
    /// matching kubectl. Subcommands that don't touch the apiserver
    /// (e.g. `audit verify`) ignore this.
    #[arg(long, global = true, env = "KUBECONFIG")]
    kubeconfig: Option<String>,

    /// Postgres connection URL. Required for `audit verify` and `drift
    /// check`. Use a role with `SELECT` on `platform.*` (and `UPDATE` if
    /// you intend to run quarantine).
    #[arg(long, global = true, env = "VELOCITY_PG_URL")]
    db_url: Option<String>,

    /// Velocity CLI config path. Defaults to `$VELOCITY_CONFIG` then
    /// `~/.velocity/config`.
    #[arg(long, global = true, env = "VELOCITY_CONFIG")]
    config: Option<PathBuf>,

    /// Override the active context for this invocation. Precedence:
    /// this flag > `$VELOCITY_CONTEXT` > `current-context` in config.
    #[arg(long, global = true)]
    context: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, clap::Subcommand)]
enum Cmd {
    /// Show overall cluster + schema health.
    Status(StatusArgs),
    /// Force a CRD reconcile (kubectl-style annotate kick).
    Reconcile(ReconcileArgs),
    /// Audit-chain operations.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Drift detection + quarantine.
    Drift {
        #[command(subcommand)]
        cmd: DriftCmd,
    },
    /// Manage `~/.velocity/config` contexts.
    Context {
        #[command(subcommand)]
        cmd: ContextCmd,
    },
    /// Print client + server build info.
    Version(VersionArgs),
    /// Server-side apply a Velocity manifest (file or stdin, multi-doc).
    Apply(ApplyArgs),
    /// List or fetch CRD objects (case-insensitive kind lookup).
    Get(GetArgs),
    /// Pretty-print spec + conditions for one CRD object.
    Describe(DescribeArgs),
    /// Delete a CRD object (prompts unless --yes).
    Delete(DeleteArgs),
    /// Show what `apply` would change, against the current cluster state.
    Diff(DiffArgs),
    /// Data-plane reads against the API (get / list / query / export).
    Record {
        #[command(subcommand)]
        cmd: RecordCmd,
    },
    /// Show event history for a record, or reconstruct state at a point in time.
    History(HistoryArgs),
    /// Restore a record to its state at a past instant (writes a new event).
    Restore(RestoreArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    // CLI logs go to stderr so stdout stays clean for table/JSON output
    // that callers may pipe. Default level INFO; raise via RUST_LOG.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,velocity_cli=info")),
        )
        .init();

    // kube-rs's TLS path defaults to rustls; install the aws-lc-rs
    // provider once at startup so clients can build without panicking.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Status(args) => status::run(args, &cli.kubeconfig, cli.output).await,
        Cmd::Reconcile(args) => reconcile::run(args, &cli.kubeconfig).await,
        Cmd::Audit { cmd } => audit::run(cmd, cli.db_url.as_deref(), cli.output).await,
        Cmd::Drift { cmd } => {
            drift::run(cmd, cli.db_url.as_deref(), &cli.kubeconfig, cli.output).await
        }
        Cmd::Context { cmd } => context_cmd::run(cmd, cli.config.as_deref(), cli.output).await,
        Cmd::Version(args) => {
            version_cmd::run(args, cli.config.as_deref(), cli.context.as_deref(), cli.output).await
        }
        Cmd::Apply(args) => kube_cmd::apply(args, &cli.kubeconfig).await,
        Cmd::Get(args) => kube_cmd::get(args, &cli.kubeconfig, cli.output).await,
        Cmd::Describe(args) => kube_cmd::describe(args, &cli.kubeconfig).await,
        Cmd::Delete(args) => kube_cmd::delete(args, &cli.kubeconfig).await,
        Cmd::Diff(args) => kube_cmd::diff(args, &cli.kubeconfig).await,
        Cmd::Record { cmd } => {
            record_cmd::run(cmd, cli.config.as_deref(), cli.context.as_deref()).await
        }
        Cmd::History(args) => {
            history_cmd::history(args, cli.config.as_deref(), cli.context.as_deref()).await
        }
        Cmd::Restore(args) => {
            history_cmd::restore(args, cli.config.as_deref(), cli.context.as_deref()).await
        }
    }
}
