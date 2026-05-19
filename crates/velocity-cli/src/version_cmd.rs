//! `velocity version` — print client + server build info.
//!
//! Distinct from `velocity --version` (the clap-derived flag), which
//! prints the client build alone. This subcommand also reaches out to
//! the active context's `/version` to confirm the URL points at a real
//! Velocity API and to report the server version + git SHA + readiness.
//!
//! When `--client` is set, or no context is configured, only the
//! client side prints — so a bare `velocity version` on a fresh
//! machine doesn't hard-fail before `context add`.

use anyhow::Result;
use clap::Args;

use crate::client::ApiClient;
use crate::config::Config;
use crate::output::{print, OutputFormat};

#[derive(Debug, Args)]
pub(crate) struct VersionArgs {
    /// Skip the server fetch and print only client build info. Useful
    /// in CI / offline environments where the context can't reach the
    /// API.
    #[arg(long)]
    pub(crate) client: bool,
}

pub(crate) async fn run(
    args: VersionArgs,
    config_path_override: Option<&std::path::Path>,
    context_override: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    let client_version = env!("CARGO_PKG_VERSION");
    let client_sha = option_env!("VELOCITY_GIT_SHA").unwrap_or("unknown");

    let mut rows: Vec<Vec<String>> = vec![vec![
        "client".into(),
        "velocity-cli".into(),
        client_version.to_string(),
        client_sha.to_string(),
        "n/a".into(),
    ]];

    if !args.client {
        match try_server(config_path_override, context_override).await {
            Ok((name, v)) => rows.push(vec![
                format!("server ({name})"),
                v.service,
                v.version,
                v.git_sha,
                if v.ready { "ready" } else { "starting" }.into(),
            ]),
            Err(e) => {
                // Don't fail the command — the client line is still
                // useful, and we want this command to work offline.
                // Surface the reason on stderr so users see why the
                // server line is missing.
                eprintln!("server: unavailable ({e})");
            }
        }
    }

    print(&["ROLE", "SERVICE", "VERSION", "GIT SHA", "STATUS"], &rows, output);
    Ok(())
}

async fn try_server(
    config_path_override: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<(String, crate::client::VersionResponse)> {
    let path = match config_path_override {
        Some(p) => p.to_path_buf(),
        None => Config::default_path()
            .ok_or_else(|| anyhow::anyhow!("could not resolve config path"))?,
    };
    let cfg = Config::load(&path)?;
    let ctx = cfg.resolve(context_override)?;
    let api = ApiClient::from_context(&ctx)?;
    let v = api.get_version().await?;
    Ok((ctx.name, v))
}
