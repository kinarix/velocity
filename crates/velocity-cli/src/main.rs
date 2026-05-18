//! `velocity` — operator CLI for Velocity clusters.
//!
//! Phase 4.5 surface: subcommands an SRE needs to operate Velocity in
//! production. Read-only commands (`status`, `audit verify`, `drift
//! check`) can run against any healthy cluster + Postgres. Write
//! commands (`reconcile`, `drift quarantine`) mutate state and require
//! credentials with the right grants.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use anyhow::Result;
use clap::Parser;

mod audit;
mod drift;
mod output;
mod reconcile;
mod status;

use audit::AuditCmd;
use drift::DriftCmd;
use output::OutputFormat;
use reconcile::ReconcileArgs;
use status::StatusArgs;

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
        Cmd::Drift { cmd } => drift::run(cmd, cli.db_url.as_deref(), &cli.kubeconfig, cli.output).await,
    }
}
