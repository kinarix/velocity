//! Phase 6b — LogFilterPolicy + LogRoutingPolicy sweeper.
//!
//! Periodically lists every `LogFilterPolicy` and `LogRoutingPolicy`
//! across the cluster, renders them into a single YAML bundle (the
//! same shape `velocity-log-processor` reads from disk), and Server-
//! Side-Applies that bundle to a ConfigMap mounted by every processor
//! pod. The processor's own poller picks up the change.
//!
//! ## Why a sweeper and not a per-CRD Controller
//!
//! A Controller fires per object; the rendered bundle depends on
//! every policy in the cluster. We'd need to aggregate inside every
//! reconcile, which makes the reconcile a fan-out and not idempotent
//! with respect to a single object's revision. A 30-second sweep is
//! both simpler and reload-aligned with the processor's own poll
//! interval — the worst-case observed-policy lag is one sweep period.
//!
//! ## Why aggregate cluster-wide (not per-namespace)
//!
//! There's one `velocity-log-processor` Deployment shared across the
//! cluster; it ingests pod logs from every namespace via the
//! DaemonSet. A per-namespace ConfigMap would force the processor to
//! mount a dozen volumes and merge them itself, duplicating logic
//! we'd have to put here anyway.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use serde::{Deserialize, Serialize};
use velocity_types::crds::{LogFilterPolicy, LogRoutingPolicy};

/// Where the rendered bundle lands. The processor mounts this same
/// `name`/`key` from its Pod spec.
const CONFIGMAP_NAMESPACE: &str = "velocity-system";
const CONFIGMAP_NAME: &str = "velocity-log-policies";
const CONFIGMAP_KEY: &str = "log-policies.yaml";
const MANAGER: &str = "velocity-operator";

/// Default sweep cadence. Aligned to (and not faster than) the
/// processor's policy-reload poll so we don't write ConfigMap revisions
/// the processor can't observe within the same window.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Run the sweeper forever. `webhook_disabled` is just to give the spawn
/// site one less unwrap to forget — the sweeper itself never wires HTTP.
pub async fn run(kube: Client) {
    tracing::info!(
        interval_secs = SWEEP_INTERVAL.as_secs(),
        cm_namespace = CONFIGMAP_NAMESPACE,
        cm_name = CONFIGMAP_NAME,
        "log-policy sweeper started"
    );
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match sweep_once(&kube).await {
            Ok(n) => tracing::debug!(rules = n, "log-policy bundle applied"),
            Err(e) => tracing::warn!(error = %e, "log-policy sweep failed; retrying next interval"),
        }
    }
}

/// One sweep tick. Returns the number of filter rules in the rendered
/// bundle (so a metric or alert can fire on "suddenly 0 rules where
/// there used to be many").
pub async fn sweep_once(kube: &Client) -> Result<usize, kube::Error> {
    let filters: Api<LogFilterPolicy> = Api::all(kube.clone());
    let routings: Api<LogRoutingPolicy> = Api::all(kube.clone());
    let filter_list = filters.list(&Default::default()).await?;
    let routing_list = routings.list(&Default::default()).await?;

    let bundle = render_bundle(&filter_list.items, &routing_list.items);
    let yaml = serde_yaml::to_string(&bundle).unwrap_or_else(|e| {
        // Should be infallible — every field is a plain String / Vec /
        // BTreeMap of serde-friendly values. Log + ship an empty bundle
        // rather than crash the operator.
        tracing::error!(error = %e, "log-policy bundle YAML serialize failed; shipping empty");
        String::new()
    });

    apply_configmap(kube, &yaml).await?;
    Ok(bundle.filters.len())
}

/// Mirror of `velocity_log_processor::LogPolicyBundle`. Kept local so
/// `velocity-operator` doesn't depend on the processor crate just for
/// the on-wire shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderedBundle {
    #[serde(default)]
    pub filters: Vec<RenderedFilterRule>,
    #[serde(default)]
    pub destinations: Vec<RenderedDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderedFilterRule {
    pub name: String,
    pub priority: i32,
    pub action: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub when: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderedDestination {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Pure render — merges every filter policy's rules into one list
/// (sorted by `(priority, namespace/name)` for deterministic output)
/// and every routing policy's destinations into another. Pure so a
/// unit test can drive it without a kube cluster.
///
/// Rule names are namespaced as `{ns}/{policyName}/{ruleName}` to keep
/// collisions impossible across teams and to make the processor's logs
/// say which CRD a rule originated from.
pub fn render_bundle(
    filters: &[LogFilterPolicy],
    routings: &[LogRoutingPolicy],
) -> RenderedBundle {
    let mut filter_rules: Vec<RenderedFilterRule> = Vec::new();
    for policy in filters {
        let ns = policy.metadata.namespace.as_deref().unwrap_or("default");
        let pname = policy.metadata.name.as_deref().unwrap_or("unnamed");
        for r in &policy.spec.rules {
            filter_rules.push(RenderedFilterRule {
                name: format!("{ns}/{pname}/{}", r.name),
                priority: r.priority,
                action: r.action.clone(),
                when: r.when.clone(),
                fields: r.fields.clone(),
                sample_rate: r.sample_rate,
            });
        }
    }
    filter_rules.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));

    let mut destinations: Vec<RenderedDestination> = Vec::new();
    for policy in routings {
        let ns = policy.metadata.namespace.as_deref().unwrap_or("default");
        let pname = policy.metadata.name.as_deref().unwrap_or("unnamed");
        for d in &policy.spec.destinations {
            destinations.push(RenderedDestination {
                name: format!("{ns}/{pname}/{}", d.name),
                kind: d.kind.clone(),
                config: d.config.clone(),
            });
        }
    }
    destinations.sort_by(|a, b| a.name.cmp(&b.name));

    RenderedBundle { filters: filter_rules, destinations }
}

async fn apply_configmap(kube: &Client, yaml: &str) -> Result<(), kube::Error> {
    let api: Api<ConfigMap> = Api::namespaced(kube.clone(), CONFIGMAP_NAMESPACE);
    let mut data = BTreeMap::new();
    data.insert(CONFIGMAP_KEY.to_string(), yaml.to_string());

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(CONFIGMAP_NAME.to_string()),
            namespace: Some(CONFIGMAP_NAMESPACE.to_string()),
            // Labels make the ConfigMap discoverable + identify the owner
            // so a human grepping `kubectl get cm -A -l ...` can find it.
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()),
                ("app.kubernetes.io/component".to_string(), "log-policies".to_string()),
            ])),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let pp = PatchParams::apply(MANAGER).force();
    api.patch(CONFIGMAP_NAME, &pp, &Patch::Apply(&cm)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use kube::api::ObjectMeta as KubeMeta;
    use velocity_types::crds::policies::{
        LogDestination, LogFilterPolicy, LogFilterPolicySpec, LogFilterRule, LogRoutingPolicy,
        LogRoutingPolicySpec,
    };

    fn filter(ns: &str, name: &str, rules: Vec<LogFilterRule>) -> LogFilterPolicy {
        LogFilterPolicy {
            metadata: KubeMeta { name: Some(name.into()), namespace: Some(ns.into()), ..Default::default() },
            spec: LogFilterPolicySpec { rules },
            status: None,
        }
    }

    fn routing(ns: &str, name: &str, dests: Vec<LogDestination>) -> LogRoutingPolicy {
        LogRoutingPolicy {
            metadata: KubeMeta { name: Some(name.into()), namespace: Some(ns.into()), ..Default::default() },
            spec: LogRoutingPolicySpec { destinations: dests },
            status: None,
        }
    }

    fn rule(name: &str, priority: i32, action: &str) -> LogFilterRule {
        LogFilterRule {
            name: name.into(),
            priority,
            action: action.into(),
            when: BTreeMap::new(),
            fields: vec![],
            sample_rate: None,
        }
    }

    #[test]
    fn empty_inputs_yield_empty_bundle() {
        let b = render_bundle(&[], &[]);
        assert!(b.filters.is_empty());
        assert!(b.destinations.is_empty());
    }

    #[test]
    fn filter_rules_are_namespaced_and_sorted_by_priority_then_name() {
        let p1 = filter("ns-a", "policy-1", vec![
            rule("z-third", 30, "drop"),
            rule("a-first", 10, "drop"),
        ]);
        let p2 = filter("ns-b", "policy-2", vec![rule("middle", 20, "keep")]);
        let b = render_bundle(&[p1, p2], &[]);
        let names: Vec<_> = b.filters.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ns-a/policy-1/a-first", "ns-b/policy-2/middle", "ns-a/policy-1/z-third"]
        );
    }

    #[test]
    fn destinations_are_namespaced_and_sorted_by_name() {
        let r1 = routing(
            "ns-a",
            "routing",
            vec![LogDestination {
                name: "stdout".into(),
                kind: "stdout".into(),
                config: BTreeMap::new(),
            }],
        );
        let r2 = routing(
            "ns-b",
            "routing",
            vec![LogDestination {
                name: "loki".into(),
                kind: "loki".into(),
                config: BTreeMap::new(),
            }],
        );
        let b = render_bundle(&[], &[r1, r2]);
        let names: Vec<_> = b.destinations.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["ns-a/routing/stdout", "ns-b/routing/loki"]);
    }

    #[test]
    fn rendered_bundle_round_trips_through_yaml() {
        // The processor parses with serde_yaml. Lock the shape so a
        // future field rename here doesn't silently break the wire.
        let mut when = BTreeMap::new();
        when.insert("level".to_string(), serde_json::json!("DEBUG"));
        let mut config = BTreeMap::new();
        config.insert("url".to_string(), serde_json::json!("https://hook.test"));
        let bundle = RenderedBundle {
            filters: vec![RenderedFilterRule {
                name: "ns/p/r".into(),
                priority: 10,
                action: "drop".into(),
                when,
                fields: vec![],
                sample_rate: None,
            }],
            destinations: vec![RenderedDestination {
                name: "ns/p/hook".into(),
                kind: "http_webhook".into(),
                config,
            }],
        };
        let yaml = serde_yaml::to_string(&bundle).unwrap();
        let parsed: RenderedBundle = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, bundle);
        // Sanity-check the YAML actually contains what we expect — the
        // processor reads `filters[].when.level` and `destinations[].url`.
        assert!(yaml.contains("filters:"), "{yaml}");
        assert!(yaml.contains("level: DEBUG"), "{yaml}");
        assert!(yaml.contains("type: http_webhook"), "{yaml}");
        assert!(yaml.contains("url: https://hook.test"), "{yaml}");
    }
}
