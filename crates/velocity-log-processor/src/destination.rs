//! Destinations for kept log records.
//!
//! v1 ships two implementations:
//!
//! - `Stdout` — always available; writes one JSON line per record to
//!   the processor's own stdout. Ops can rely on `kubectl logs
//!   velocity-log-processor` as the fallback view.
//! - `HttpWebhook` — POST `application/json` per batch. Same shape as
//!   the anomaly webhook so a single tooling endpoint can subscribe
//!   to both.
//!
//! `Loki` and `S3` destinations are recognised by name but log a
//! warning and behave as no-ops, so misconfigured CRDs surface
//! immediately rather than silently dropping data.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::policy::LogRoutingDestSpec;

/// Outcome of a single record dispatch. `Sent` increments the success
/// metric; everything else increments the failure metric so an alert
/// can fire on rising error rates.
#[derive(Debug, Clone, PartialEq)]
pub enum DestinationOutcome {
    Sent,
    Skipped(&'static str),
    Failed(String),
}

#[async_trait::async_trait]
pub trait Destination: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &str;
    async fn send(&self, record: &Value) -> DestinationOutcome;
}

/// Build a destination from its CRD spec. Returns `None` for
/// unknown kinds (caller logs and skips) so the bundle as a whole
/// still starts even if one destination is misconfigured.
pub fn build(spec: &LogRoutingDestSpec) -> Option<Arc<dyn Destination>> {
    match spec.kind.as_str() {
        "stdout" => Some(Arc::new(Stdout { name: spec.name.clone() })),
        "http_webhook" => {
            let url = spec.config.get("url")?.as_str()?.to_string();
            let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().ok()?;
            Some(Arc::new(HttpWebhook { name: spec.name.clone(), url, client }))
        }
        // Recognised-but-unimplemented kinds: return a "skip" sink so
        // operators see a clear "configured but no-op" log line.
        "loki" | "s3" => {
            Some(Arc::new(NotYet { name: spec.name.clone(), kind: spec.kind.clone() }))
        }
        _ => None,
    }
}

#[derive(Debug)]
pub struct Stdout {
    name: String,
}

#[async_trait::async_trait]
impl Destination for Stdout {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, record: &Value) -> DestinationOutcome {
        // Write directly to stdout — tracing's JSON formatter would wrap
        // us in another envelope, but we want the line itself to be the
        // shipped JSON record. `println!` is clippy-banned crate-wide
        // (good default in services), so write via the `Write` trait.
        use std::io::Write as _;
        match serde_json::to_vec(record) {
            Ok(mut s) => {
                s.push(b'\n');
                let _ = std::io::stdout().write_all(&s);
                DestinationOutcome::Sent
            }
            Err(e) => DestinationOutcome::Failed(e.to_string()),
        }
    }
}

#[derive(Debug)]
pub struct HttpWebhook {
    name: String,
    url: String,
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl Destination for HttpWebhook {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, record: &Value) -> DestinationOutcome {
        match self.client.post(&self.url).json(record).send().await {
            Ok(r) if r.status().is_success() => DestinationOutcome::Sent,
            Ok(r) => DestinationOutcome::Failed(format!("status {}", r.status())),
            Err(e) => DestinationOutcome::Failed(e.to_string()),
        }
    }
}

/// Sink for destination kinds we acknowledge in CRDs but haven't built
/// yet (loki, s3). The `kind` is held so log lines name what's pending.
#[derive(Debug)]
pub struct NotYet {
    name: String,
    #[allow(dead_code)]
    kind: String,
}

#[async_trait::async_trait]
impl Destination for NotYet {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, _record: &Value) -> DestinationOutcome {
        DestinationOutcome::Skipped("destination kind not yet implemented")
    }
}

/// Build every destination in the bundle. Logs an error for each
/// failure but never panics — a typo in one destination shouldn't
/// stop the others from running.
pub fn build_all(specs: &[LogRoutingDestSpec]) -> Vec<Arc<dyn Destination>> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        match build(spec) {
            Some(d) => out.push(d),
            None => tracing::error!(
                name = %spec.name,
                kind = %spec.kind,
                "unknown log destination kind; ignoring"
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::BTreeMap;

    fn spec(name: &str, kind: &str) -> LogRoutingDestSpec {
        LogRoutingDestSpec { name: name.into(), kind: kind.into(), config: BTreeMap::new() }
    }

    #[tokio::test]
    async fn stdout_sends() {
        let d = build(&spec("c", "stdout")).unwrap();
        assert_eq!(d.send(&serde_json::json!({"k":"v"})).await, DestinationOutcome::Sent);
    }

    #[test]
    fn http_webhook_requires_url() {
        let d = build(&spec("h", "http_webhook"));
        assert!(d.is_none(), "missing url must produce None");
    }

    #[tokio::test]
    async fn unknown_kind_returns_none() {
        assert!(build(&spec("x", "bogus")).is_none());
    }

    #[tokio::test]
    async fn loki_and_s3_are_skipped() {
        let d = build(&spec("l", "loki")).unwrap();
        assert!(matches!(d.send(&serde_json::json!({})).await, DestinationOutcome::Skipped(_)));
        let d = build(&spec("s", "s3")).unwrap();
        assert!(matches!(d.send(&serde_json::json!({})).await, DestinationOutcome::Skipped(_)));
    }

    #[test]
    fn build_all_skips_unknowns_and_keeps_known() {
        let specs = vec![spec("a", "stdout"), spec("b", "bogus"), spec("c", "loki")];
        let dests = build_all(&specs);
        assert_eq!(dests.len(), 2);
        assert_eq!(dests[0].name(), "a");
        assert_eq!(dests[1].name(), "c");
    }
}
