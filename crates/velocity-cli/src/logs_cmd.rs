//! `velocity logs <component>` — kubectl-style log streaming for the
//! Velocity in-cluster components (operator, webhook, log-processor,
//! log-collector, etc.). Resolves `<component>` via the canonical
//! `app.kubernetes.io/component=<value>` label the Helm chart sets,
//! lists matching pods in the target namespace, and streams their
//! logs concurrently to stdout. When multiple pods match (DaemonSets,
//! multi-replica deployments) each line is prefixed `[pod-name]` so
//! you can tell streams apart.
//!
//! No Velocity API server involved — this talks straight to the kube
//! apiserver, same way `kubectl logs` does. The CLI already depends
//! on kube-rs for apply/get/delete/diff, so the surface area cost is
//! one new module + one new subcommand variant.

use std::collections::HashSet;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use futures::io::AsyncBufReadExt as _;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, LogParams};
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::kube_helpers::build_client;

/// Helm chart label key. The chart sets exactly this on every workload
/// (operator/webhook/log-processor/log-collector), so the CLI resolves
/// the user-friendly `<component>` argument through it. Centralised
/// here so a chart-side rename only needs one CLI patch.
const COMPONENT_LABEL: &str = "app.kubernetes.io/component";

#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    /// Component name. Matched against the
    /// `app.kubernetes.io/component=<value>` label set by the Helm
    /// chart. Typical values: `operator`, `webhook`, `log-processor`,
    /// `log-collector`. Free-form so chart additions don't need a
    /// CLI rebuild.
    pub component: String,

    /// Namespace to look in. When unset, falls back to the namespace
    /// from the current kubeconfig context (kubectl-style: that's
    /// usually `default` unless you've run `kubectl config set-context
    /// --current --namespace=…`). Pass `-n` to override per-invocation.
    #[arg(long, short = 'n')]
    pub namespace: Option<String>,

    /// Stream logs continuously, the way `kubectl logs -f` does. Ctrl-C
    /// to stop. Without --follow we emit whatever's currently buffered
    /// and exit when each pod's stream EOFs.
    #[arg(long, short = 'f', default_value_t = false)]
    pub follow: bool,

    /// Show only the last N lines per pod. Equivalent to
    /// `kubectl logs --tail N`.
    #[arg(long, value_name = "N")]
    pub tail: Option<i64>,

    /// Show logs from a previous (terminated) container instance.
    /// Helpful when a pod is in CrashLoopBackOff and you want the
    /// stack trace from the last crash.
    #[arg(long, default_value_t = false)]
    pub previous: bool,

    /// Container name within the pod. Defaults to the first container
    /// in the spec (kube-rs's default).
    #[arg(long)]
    pub container: Option<String>,

    /// Show logs since this many seconds ago (e.g. 300 = last 5 min).
    /// Use a number; no unit suffix, to keep parsing trivial and the
    /// arg unambiguous next to `--tail`.
    #[arg(long, value_name = "SECONDS")]
    pub since_seconds: Option<i64>,

    /// Cap the number of pods streamed concurrently. Matters mainly
    /// for `log-collector`, which is a DaemonSet with one pod per
    /// node — a 100-node cluster would otherwise open 100 streams.
    #[arg(long, default_value_t = 8)]
    pub max_pods: usize,
}

pub(crate) async fn run(args: LogsArgs, kubeconfig: Option<&str>) -> Result<()> {
    let client = build_client(kubeconfig).await?;
    // kube-rs threads the kubeconfig's current-context namespace through
    // `Client::default_namespace()`; honour that when the user didn't
    // pass `-n`. Matches `kubectl logs` UX.
    let namespace =
        args.namespace.clone().unwrap_or_else(|| client.default_namespace().to_string());
    let pods: Api<Pod> = Api::namespaced(client, &namespace);

    let selector = format!("{COMPONENT_LABEL}={}", args.component);
    let pod_list = pods.list(&ListParams::default().labels(&selector)).await.with_context(
        || format!("listing pods in namespace {namespace} (selector: {selector})"),
    )?;

    let matched: Vec<String> =
        pod_list.items.iter().filter_map(|p| p.metadata.name.clone()).collect();

    if matched.is_empty() {
        return Err(anyhow!(
            "no pods found with {COMPONENT_LABEL}={} in namespace {namespace} — \
             check that the chart is installed and the component name \
             is one of: operator, webhook, log-processor, log-collector",
            args.component,
        ));
    }

    let lp = LogParams {
        follow: args.follow,
        tail_lines: args.tail,
        previous: args.previous,
        container: args.container.clone(),
        since_seconds: args.since_seconds,
        ..Default::default()
    };

    // Cap concurrency. Beyond `max_pods` we simply skip the rest with
    // a stderr note — better than silently truncating.
    let (to_stream, skipped) = if matched.len() > args.max_pods {
        let (head, tail) = matched.split_at(args.max_pods);
        (head.to_vec(), tail.to_vec())
    } else {
        (matched.clone(), Vec::new())
    };
    if !skipped.is_empty() {
        eprintln!(
            "velocity logs: limiting to {} of {} pods (raise --max-pods to widen). \
             Skipped: {}",
            args.max_pods,
            matched.len(),
            skipped.join(", ")
        );
    }

    let multi = to_stream.len() > 1;
    let mut tasks = Vec::with_capacity(to_stream.len());
    for name in to_stream {
        let api = pods.clone();
        let lp = lp.clone();
        tasks.push(tokio::spawn(async move { stream_one(api, name, lp, multi).await }));
    }

    // Wait for all to complete; collect first error to surface a
    // non-zero exit while still letting other streams keep flowing.
    let mut first_err: Option<anyhow::Error> = None;
    let mut completed: HashSet<String> = HashSet::new();
    for t in tasks {
        match t.await {
            Ok(Ok(name)) => {
                completed.insert(name);
            }
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(join_err) => {
                if first_err.is_none() {
                    first_err = Some(anyhow!(join_err).context("log streaming task panicked"));
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

/// Stream one pod's log feed to stdout. Returns the pod name on
/// success so the caller can report which ones drained cleanly.
async fn stream_one(pods: Api<Pod>, name: String, lp: LogParams, prefix: bool) -> Result<String> {
    // kube-rs hands back an `impl futures::AsyncBufRead` — note the
    // `futures::` crate, NOT tokio's. The two traits are not
    // interchangeable; importing tokio::io::AsyncBufReadExt instead
    // gives a "doesn't satisfy AsyncBufRead" type error. We pull in
    // the futures-side extension trait for `read_until`, then write
    // out via tokio's BufWriter (stdout writes need tokio because
    // the runtime is tokio).
    let mut reader = pods
        .log_stream(&name, &lp)
        .await
        .with_context(|| format!("opening log stream for pod {name}"))?;
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .await
            .with_context(|| format!("reading log line from pod {name}"))?;
        if n == 0 {
            break; // EOF
        }
        if prefix {
            writer.write_all(b"[").await?;
            writer.write_all(name.as_bytes()).await?;
            writer.write_all(b"] ").await?;
        }
        writer.write_all(&buf[..n]).await?;
        // If the chunk ended mid-line (last byte isn't '\n'), pad it so
        // the next iteration's prefix doesn't run into the message.
        if buf[n - 1] != b'\n' {
            writer.write_all(b"\n").await?;
        }
        writer.flush().await?;
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use clap::Parser;

    /// Wrapper used by argument-parsing tests below — clap's
    /// `try_parse_from` works on full Parsers, not bare Args.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    }

    #[derive(Debug, clap::Subcommand)]
    enum HarnessCmd {
        Logs(LogsArgs),
    }

    fn parse(args: &[&str]) -> LogsArgs {
        let mut full = vec!["test", "logs"];
        full.extend_from_slice(args);
        let Harness { cmd: HarnessCmd::Logs(a) } = Harness::try_parse_from(full).unwrap();
        a
    }

    #[test]
    fn defaults_match_doc() {
        let a = parse(&["operator"]);
        assert_eq!(a.component, "operator");
        assert!(a.namespace.is_none(), "no --namespace → resolved at runtime from kubeconfig");
        assert!(!a.follow);
        assert!(a.tail.is_none());
        assert!(!a.previous);
        assert!(a.container.is_none());
        assert!(a.since_seconds.is_none());
        assert_eq!(a.max_pods, 8);
    }

    #[test]
    fn follow_flag_short_and_long() {
        assert!(parse(&["webhook", "-f"]).follow);
        assert!(parse(&["webhook", "--follow"]).follow);
    }

    #[test]
    fn namespace_short_and_long() {
        assert_eq!(parse(&["operator", "-n", "other"]).namespace.as_deref(), Some("other"));
        assert_eq!(parse(&["operator", "--namespace", "ns2"]).namespace.as_deref(), Some("ns2"));
    }

    #[test]
    fn tail_accepts_integer() {
        assert_eq!(parse(&["operator", "--tail", "200"]).tail, Some(200));
    }

    #[test]
    fn since_seconds_accepts_integer() {
        assert_eq!(parse(&["operator", "--since-seconds", "300"]).since_seconds, Some(300));
    }

    #[test]
    fn missing_component_rejected() {
        let r = Harness::try_parse_from(["test", "logs"]);
        assert!(r.is_err(), "logs without <component> should fail to parse");
    }

    #[test]
    fn component_label_constant_matches_chart() {
        // Defensive: if the helm chart ever moves off
        // app.kubernetes.io/component, this constant + the chart
        // must move together. The chart values are checked at apply
        // time by the integration test; this is just a sanity guard
        // that the CLI side is using the documented label.
        assert_eq!(COMPONENT_LABEL, "app.kubernetes.io/component");
    }
}
