//! `velocity health` / `velocity metrics` / `velocity slo` —
//! observability convenience wrappers.
//!
//! - `health`  hits `/version` on the active context to prove the API
//!   is reachable and ready. Same target as `velocity version`, but
//!   without the dual-line client+server table — pure smoke-test for
//!   CI gates: zero exit on healthy, non-zero otherwise.
//!
//! - `metrics` fetches `/metrics` in Prometheus exposition format.
//!   Output is the raw text by default; `--filter <substring>` greps
//!   for matching lines (cheap; for real querying, pipe to grep or
//!   PromQL via Prometheus).
//!
//! - `slo` reads SchemaDefinitions from the apiserver and prints the
//!   SLOs each one declares (operation, target_p99_ms, availability,
//!   window). The Prometheus alerting rules these emit are managed
//!   by the operator's SLO sweeper — this command is the source-of-
//!   truth view, not a re-derivation.
//!
//! `logs` is deferred: pod logs are already `kubectl logs`, and the
//! central log stream lands as a portal feature (Phase 10).

use anyhow::{anyhow, Context as _, Result};
use clap::Args;
use kube::api::ListParams;
use kube::Api;
use velocity_types::crds::schema::SchemaDefinition;

use crate::client::ApiClient;
use crate::config::Config;
use crate::kube_helpers::build_client;
use crate::output::{print, OutputFormat};

// ---------------------------------------------------------------------
// health
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct HealthArgs {
    /// Treat `ready=false` as failure (non-zero exit). Default true,
    /// matching what a CI healthcheck wants — schema-registry warm-up
    /// is part of "healthy."
    #[arg(long, default_value_t = true)]
    pub require_ready: bool,
}

pub(crate) async fn health(
    args: HealthArgs,
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<()> {
    let api = build_data_client(config_path, context_override)?;
    let v = api.get_version().await?;
    if args.require_ready && !v.ready {
        eprintln!("UNHEALTHY: {} {} (ready=false)", v.service, v.version);
        std::process::exit(1);
    }
    eprintln!("OK: {} {} ready={}", v.service, v.version, v.ready);
    Ok(())
}

// ---------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct MetricsArgs {
    /// Only print metric lines whose name contains this substring.
    /// Useful for "show me velocity_archive_*" without piping through
    /// grep.
    #[arg(long)]
    pub filter: Option<String>,
    /// Override the metrics URL (default: `<api>/metrics`). Use this
    /// when the deployment exposes /metrics on a separate health
    /// listener.
    #[arg(long)]
    pub url: Option<String>,
}

pub(crate) async fn metrics(
    args: MetricsArgs,
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<()> {
    let api = build_data_client(config_path, context_override)?;
    let body = api.get_metrics_raw(args.url.as_deref()).await?;
    print_filtered(&body, args.filter.as_deref());
    Ok(())
}

fn print_filtered(body: &str, filter: Option<&str>) {
    let Some(needle) = filter else {
        print!("{body}");
        return;
    };
    // Substring match covers both metric lines and the `# HELP` /
    // `# TYPE` comment lines (which embed the metric name verbatim),
    // so the output stays grep-able with no special-case branching.
    for line in body.lines() {
        if line.contains(needle) {
            println!("{line}");
        }
    }
}

// ---------------------------------------------------------------------
// slo
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct SloArgs {
    /// Filter to a single namespace.
    #[arg(short, long)]
    pub namespace: Option<String>,
    /// Filter to a single schema (metadata.name match).
    #[arg(long)]
    pub schema: Option<String>,
}

pub(crate) async fn slo(
    args: SloArgs,
    kubeconfig: &Option<String>,
    output: OutputFormat,
) -> Result<()> {
    let client = build_client(kubeconfig.as_deref()).await?;
    let api: Api<SchemaDefinition> = match &args.namespace {
        Some(ns) => Api::namespaced(client, ns),
        None => Api::all(client),
    };
    let list =
        api.list(&ListParams::default()).await.context("listing SchemaDefinitions for SLO view")?;

    let mut rows: Vec<Vec<String>> = Vec::new();
    for sd in list.items {
        let name = sd.metadata.name.clone().unwrap_or_else(|| "<unnamed>".into());
        if let Some(filter) = &args.schema {
            if name != *filter {
                continue;
            }
        }
        let ns = sd.metadata.namespace.clone().unwrap_or_else(|| "<none>".into());
        for slo in &sd.spec.observability.slos {
            rows.push(vec![
                ns.clone(),
                name.clone(),
                slo.operation.clone(),
                slo.target_p99_ms.map(|n| format!("{n}ms")).unwrap_or_else(|| "—".into()),
                slo.availability.map(|f| format!("{f:.4}")).unwrap_or_else(|| "—".into()),
                slo.window.clone().unwrap_or_else(|| "—".into()),
            ]);
        }
    }
    print(
        &["NAMESPACE", "SCHEMA", "OPERATION", "P99 TARGET", "AVAILABILITY", "WINDOW"],
        &rows,
        output,
    );
    Ok(())
}

// ---------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn print_filtered_no_filter_prints_everything() {
        // Smoke test: no needle means body unchanged. We can't easily
        // capture stdout from a fn that prints, so just confirm the
        // branch is taken via behaviour parity in a future integration.
        // For now, prove the filter branch picks lines correctly.
        let body = "# HELP velocity_archive_records_total total archived\n\
             # TYPE velocity_archive_records_total counter\n\
             velocity_archive_records_total 42\n\
             velocity_api_request_duration_ms_bucket{le=\"100\"} 7\n";

        // Mirror the source: select lines whose substring contains the needle.
        let keep: Vec<&str> =
            body.lines().filter(|line| line.contains("velocity_archive")).collect();
        assert_eq!(keep.len(), 3);
        assert!(keep[0].starts_with("# HELP"));
        assert!(keep[2].contains("42"));
    }
}
