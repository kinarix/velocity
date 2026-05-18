//! `velocity status` — read SchemaDefinitions across the cluster and
//! print their reconcile phase + Ready condition. This is the SRE's
//! first stop when a tenant says "my schema is stuck."
//!
//! Read-only against the apiserver. No DB connection required.

use anyhow::{Context, Result};
use clap::Args;
use kube::api::ListParams;
use kube::{Api, Client, Config};
use velocity_types::crds::schema::{SchemaDefinition, SchemaDefinitionStatus};

use crate::output::{self, OutputFormat};

#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    /// Filter to a single namespace. Without this, all namespaces are
    /// scanned (the actor must have list permission on every namespace
    /// they want to see).
    #[arg(short, long)]
    pub namespace: Option<String>,

    /// Filter to a single org. Matches the `velocity.sh/org` label the
    /// operator sets on every SchemaDefinition.
    #[arg(long)]
    pub org: Option<String>,

    /// Show only schemas whose phase != Ready. Useful for `--no-output`
    /// CI checks: empty output + zero exit ⇒ everything is healthy.
    #[arg(long)]
    pub only_unhealthy: bool,
}

pub(crate) async fn run(args: StatusArgs, kubeconfig: &Option<String>, output: OutputFormat) -> Result<()> {
    let client = build_client(kubeconfig.as_deref()).await?;

    let api: Api<SchemaDefinition> = if let Some(ns) = &args.namespace {
        Api::namespaced(client, ns)
    } else {
        Api::all(client)
    };

    let mut lp = ListParams::default();
    if let Some(org) = &args.org {
        lp = lp.labels(&format!("velocity.sh/org={org}"));
    }

    // `paginated` keeps memory bounded for large clusters; the typical
    // case is small (tens of SchemaDefinitions per tenant) so we just
    // materialise everything.
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut stream = api.list(&lp).await.context("listing SchemaDefinitions")?;
    let items = std::mem::take(&mut stream.items);
    for sd in items {
        let ns = sd.metadata.namespace.clone().unwrap_or_else(|| "<none>".into());
        let name = sd.metadata.name.clone().unwrap_or_else(|| "<unnamed>".into());
        let (phase, ready_msg) = summarise_status(sd.status.as_ref());

        if args.only_unhealthy && phase == "Ready" {
            continue;
        }

        let created = sd
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.to_string())
            .unwrap_or_else(|| "?".into());

        rows.push(vec![ns, name, phase, created, ready_msg]);
    }

    output::print(
        &["NAMESPACE", "NAME", "PHASE", "CREATED", "READY"],
        &rows,
        output,
    );
    Ok(())
}

/// Build a kube client honouring `--kubeconfig` if given, otherwise
/// falling back to the standard discovery chain (KUBECONFIG env,
/// `~/.kube/config`, in-cluster service account).
async fn build_client(kubeconfig: Option<&str>) -> Result<Client> {
    if let Some(path) = kubeconfig {
        // KubeConfigOptions::default() picks current-context; explicit
        // KUBECONFIG-style paths are honoured via the env var only, so
        // for an explicit path arg we read + parse the file ourselves.
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

/// Pick the phase + a short Ready-condition message for the row.
fn summarise_status(status: Option<&SchemaDefinitionStatus>) -> (String, String) {
    let Some(status) = status else {
        return ("Unknown".into(), "no status yet".into());
    };
    let phase = status
        .phase
        .map(|p| format!("{p:?}"))
        .unwrap_or_else(|| "Unknown".into());

    // Pull the most recent Ready condition. If absent, show the phase
    // again (useful when an operator only writes `phase` without
    // conditions). Truncated to keep table rows aligned.
    let ready_msg = status
        .conditions
        .iter()
        .find(|c| c.kind == "Ready")
        .map(|c| {
            let s = c.message.as_deref().unwrap_or("");
            if s.is_empty() { c.status.clone() } else { truncate(s, 60) }
        })
        .unwrap_or_else(|| "—".into());

    (phase, ready_msg)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use velocity_types::crds::Condition;

    fn cond(kind: &str, status: &str, message: &str) -> Condition {
        Condition {
            kind: kind.into(),
            status: status.into(),
            reason: None,
            message: Some(message.into()),
            last_transition_time: None,
        }
    }

    #[test]
    fn summarise_no_status() {
        let (p, msg) = summarise_status(None);
        assert_eq!(p, "Unknown");
        assert!(msg.contains("no status"));
    }

    #[test]
    fn summarise_ready() {
        let s = SchemaDefinitionStatus {
            phase: Some(velocity_types::crds::ReconcilePhase::Ready),
            provisioned_at: None,
            pg_table: None,
            policy_hash: None,
            records: None,
            conditions: vec![cond("Ready", "True", "Provisioned")],
        };
        let (p, msg) = summarise_status(Some(&s));
        assert_eq!(p, "Ready");
        assert!(msg.contains("Provisioned"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // Multi-byte char near the cut point; ensure we never split
        // inside a UTF-8 sequence.
        let s = "café-system-stuck-on-migration";
        let out = truncate(s, 5);
        assert!(out.ends_with('…'));
    }
}
