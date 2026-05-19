//! Phase 7 slice 2 — SLO → Prometheus alerting-rule sweeper.
//!
//! Periodically lists every `SchemaDefinition`, walks each one's
//! `observability.slos`, renders a single PrometheusRule-shaped YAML
//! body, and Server-Side-Applies it to a cluster-wide ConfigMap.
//!
//! The ConfigMap is portable two ways:
//! * Prometheus Operator users mount it and reference it in a
//!   `PrometheusRule` CR (the YAML body matches `spec.groups` exactly).
//! * Vanilla Prometheus users mount the file under `--rule-files`.
//!
//! ## Why a sweeper, not per-CRD reconcile
//!
//! Mirrors `log_policy` — the rendered output depends on every
//! schema's SLOs in aggregate, not on a single CRD. A 60s sweep keeps
//! the operator simple and the lag bounded.
//!
//! ## Metric shape we generate against
//!
//! - `velocity_operation_duration_seconds{operation,outcome}` — histogram
//!   exported by `velocity-api` (no schema label by design — see
//!   CLAUDE.md › cardinality split). Per-schema latency lives in traces.
//! - `velocity_operations_total{schema,operation,outcome,...}` — counter
//!   with `schema` and `outcome` labels.
//!
//! Latency rules therefore can't be per-schema (no label to filter on);
//! they're per-operation. Availability rules ARE per-schema (the counter
//! has the label).

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use serde::{Deserialize, Serialize};
use velocity_types::crds::schema::SloSpec;
use velocity_types::crds::SchemaDefinition;

const CONFIGMAP_NAMESPACE: &str = "velocity-system";
const CONFIGMAP_NAME: &str = "velocity-slo-rules";
const CONFIGMAP_KEY: &str = "slo-rules.yaml";
const MANAGER: &str = "velocity-operator";

/// One minute — fast enough that adding a new SLO to a CRD shows up
/// before an operator notices, slow enough that we're not hammering
/// the API server. Matches the operator's other periodic sweeps.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Run forever. Errors are logged + skipped; the next tick re-tries.
pub async fn run(kube: Client) {
    tracing::info!(
        interval_secs = SWEEP_INTERVAL.as_secs(),
        cm_namespace = CONFIGMAP_NAMESPACE,
        cm_name = CONFIGMAP_NAME,
        "SLO rules sweeper started"
    );
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match sweep_once(&kube).await {
            Ok(n) => tracing::debug!(rules = n, "SLO rules bundle applied"),
            Err(e) => {
                tracing::warn!(error = %e, "SLO rules sweep failed; retrying next interval");
            }
        }
    }
}

/// One sweep tick. Returns the total number of rules in the bundle so
/// a future metric or alert can fire when SLO coverage suddenly drops.
pub async fn sweep_once(kube: &Client) -> Result<usize, kube::Error> {
    let schemas: Api<SchemaDefinition> = Api::all(kube.clone());
    let list = schemas.list(&Default::default()).await?;
    let bundle = render_bundle(&list.items);
    let total_rules: usize = bundle.groups.iter().map(|g| g.rules.len()).sum();
    let yaml = serde_yaml::to_string(&bundle).unwrap_or_else(|e| {
        tracing::error!(error = %e, "SLO rules YAML serialize failed; shipping empty");
        String::new()
    });
    apply_configmap(kube, &yaml).await?;
    Ok(total_rules)
}

/// PrometheusRule-shaped body. The top-level `groups` array matches
/// `monitoring.coreos.com/v1/PrometheusRule.spec.groups` exactly so an
/// operator user can copy it into a CR without translation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RulesBundle {
    pub groups: Vec<RuleGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleGroup {
    pub name: String,
    pub rules: Vec<AlertingRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertingRule {
    pub alert: String,
    pub expr: String,
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub for_: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

/// Pure render — every input is a `SchemaDefinition`, output is a
/// deterministic bundle (sorted) so two consecutive sweeps with the
/// same input produce byte-identical YAML.
///
/// Rules are grouped by `{namespace}/{name}` so they stay aligned with
/// the schema they came from and the group name is meaningful in
/// alert routing.
pub fn render_bundle(schemas: &[SchemaDefinition]) -> RulesBundle {
    let mut groups: Vec<RuleGroup> = Vec::new();
    for sd in schemas {
        let ns = sd.metadata.namespace.as_deref().unwrap_or("default");
        let name = sd.metadata.name.as_deref().unwrap_or("unnamed");

        // SchemaDefinition does not carry its own `org/app/domain` in
        // spec — those live in labels (the same labels the validating
        // webhook checks). Anything missing means the CRD is mis-
        // labelled; skip rather than emit a rule with empty segments.
        let labels = sd.metadata.labels.as_ref();
        let Some(org) = labels.and_then(|l| l.get("velocity.sh/org")) else { continue };
        let Some(app) = labels.and_then(|l| l.get("velocity.sh/app")) else { continue };
        let Some(domain) = labels.and_then(|l| l.get("velocity.sh/domain")) else { continue };
        let schema_label = format!("{}/{}/{}/{}/{}", org, app, domain, name, sd.spec.version);

        let mut rules: Vec<AlertingRule> = Vec::new();
        for slo in &sd.spec.observability.slos {
            rules.extend(render_slo_rules(&schema_label, slo, ns, name));
        }
        if rules.is_empty() {
            continue;
        }
        rules.sort_by(|a, b| a.alert.cmp(&b.alert));
        groups.push(RuleGroup {
            name: format!("velocity.{ns}.{name}"),
            rules,
        });
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    RulesBundle { groups }
}

/// Render the (latency, availability) rule pair for one SLO. Either
/// half is skipped if the corresponding budget isn't specified, so a
/// CRD that only sets `target_p99_ms` produces only a latency rule.
fn render_slo_rules(
    schema_label: &str,
    slo: &SloSpec,
    ns: &str,
    name: &str,
) -> Vec<AlertingRule> {
    let mut out = Vec::new();

    // Latency. The histogram has no `schema` label by design (see
    // CLAUDE.md), so the alert fires when *any* schema's p99 for this
    // operation breaches the budget. The schema annotation pinpoints
    // which CRD owns this SLO — operators decide whether to act.
    if let Some(target_ms) = slo.target_p99_ms {
        let target_seconds = (target_ms as f64) / 1000.0;
        let expr = format!(
            "histogram_quantile(0.99, sum(rate(velocity_operation_duration_seconds_bucket{{operation=\"{}\"}}[5m])) by (le)) > {}",
            slo.operation, target_seconds
        );
        out.push(AlertingRule {
            alert: format!("VelocityP99Latency_{}_{}_{}", ns, name, slo.operation),
            expr,
            for_: Some("5m".into()),
            labels: BTreeMap::from([
                ("severity".to_string(), "warning".to_string()),
                ("velocity_schema".to_string(), schema_label.to_string()),
                ("velocity_operation".to_string(), slo.operation.clone()),
            ]),
            annotations: BTreeMap::from([
                (
                    "summary".to_string(),
                    format!(
                        "p99 latency for `{}` exceeded {}ms budget (owner: {}/{})",
                        slo.operation, target_ms, ns, name
                    ),
                ),
                (
                    "description".to_string(),
                    "histogram_quantile is computed cluster-wide (no schema label on the histogram by cardinality design). Check traces for the offending schema.".to_string(),
                ),
            ]),
        });
    }

    // Availability. Counter carries the `schema` label, so the alert
    // is per-schema. `availability` is expressed as a fraction (0..1);
    // error budget = 1 - availability.
    if let Some(avail) = slo.availability {
        let budget = (1.0 - avail).max(0.0);
        let window = slo.window.as_deref().unwrap_or("30d");
        let expr = format!(
            "(sum(rate(velocity_operations_total{{schema=\"{schema}\",operation=\"{op}\",outcome=\"error\"}}[{window}])) \
             / \
             sum(rate(velocity_operations_total{{schema=\"{schema}\",operation=\"{op}\"}}[{window}]))) > {budget}",
            schema = schema_label,
            op = slo.operation,
            window = window,
            budget = budget,
        );
        out.push(AlertingRule {
            alert: format!("VelocityErrorBudget_{}_{}_{}", ns, name, slo.operation),
            expr,
            for_: Some("15m".into()),
            labels: BTreeMap::from([
                ("severity".to_string(), "warning".to_string()),
                ("velocity_schema".to_string(), schema_label.to_string()),
                ("velocity_operation".to_string(), slo.operation.clone()),
            ]),
            annotations: BTreeMap::from([
                (
                    "summary".to_string(),
                    format!(
                        "Error budget burn for `{}` on {} over {}",
                        slo.operation, schema_label, window
                    ),
                ),
                (
                    "description".to_string(),
                    format!(
                        "Availability target {avail} over {window} window; tolerated error rate {budget}."
                    ),
                ),
            ]),
        });
    }

    out
}

async fn apply_configmap(kube: &Client, yaml: &str) -> Result<(), kube::Error> {
    let api: Api<ConfigMap> = Api::namespaced(kube.clone(), CONFIGMAP_NAMESPACE);
    let mut data = BTreeMap::new();
    data.insert(CONFIGMAP_KEY.to_string(), yaml.to_string());

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(CONFIGMAP_NAME.to_string()),
            namespace: Some(CONFIGMAP_NAMESPACE.to_string()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()),
                ("app.kubernetes.io/component".to_string(), "slo-rules".to_string()),
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
    use velocity_types::common::NamespacedRef;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    /// Build a SchemaDefinition fixture with the labels the renderer
    /// reads. Mirrors what the validating webhook enforces.
    fn sd(
        ns: &str,
        name: &str,
        org: &str,
        app: &str,
        domain: &str,
        version: &str,
        slos: Vec<SloSpec>,
    ) -> SchemaDefinition {
        let labels: BTreeMap<String, String> = BTreeMap::from([
            ("velocity.sh/org".into(), org.into()),
            ("velocity.sh/app".into(), app.into()),
            ("velocity.sh/domain".into(), domain.into()),
        ]);
        SchemaDefinition {
            metadata: KubeMeta {
                name: Some(name.into()),
                namespace: Some(ns.into()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: SchemaDefinitionSpec {
                version: version.into(),
                partitioning: None,
                auth: AuthSpec {
                    strategy_ref: NamespacedRef {
                        namespace: "ns".into(),
                        name: "strategy".into(),
                    },
                    overrides: vec![],
                },
                access: AccessSpec::default(),
                fields: vec![],
                validations: vec![],
                search: SearchSpec {
                    tier: SearchTier::Tier1,
                    ..Default::default()
                },
                time_machine: None,
                audit: None,
                archive: None,
                observability: ObservabilitySpec {
                    slos,
                    extras: Default::default(),
                },
                scaling: None,
            },
            status: None,
        }
    }

    #[test]
    fn empty_input_yields_empty_bundle() {
        let b = render_bundle(&[]);
        assert!(b.groups.is_empty());
    }

    fn slo_lat(op: &str, p99_ms: u32) -> SloSpec {
        SloSpec {
            operation: op.into(),
            target_p99_ms: Some(p99_ms),
            availability: None,
            window: None,
        }
    }

    fn slo_avail(op: &str, avail: f64, window: Option<&str>) -> SloSpec {
        SloSpec {
            operation: op.into(),
            target_p99_ms: None,
            availability: Some(avail),
            window: window.map(Into::into),
        }
    }

    fn slo_both(op: &str, p99_ms: u32, avail: f64, window: &str) -> SloSpec {
        SloSpec {
            operation: op.into(),
            target_p99_ms: Some(p99_ms),
            availability: Some(avail),
            window: Some(window.into()),
        }
    }

    #[test]
    fn schema_with_no_slos_produces_no_group() {
        let s = sd("tenants", "po", "acme", "sc", "proc", "v1", vec![]);
        assert!(render_bundle(&[s]).groups.is_empty());
    }

    #[test]
    fn unlabelled_schema_is_skipped() {
        // Missing org/app/domain labels — webhook should reject but we
        // belt-and-braces here so the operator never emits a rule with
        // empty path segments.
        let mut s = sd("tenants", "po", "acme", "sc", "proc", "v1", vec![slo_lat("create", 100)]);
        s.metadata.labels = None;
        assert!(render_bundle(&[s]).groups.is_empty());
    }

    #[test]
    fn latency_only_slo_emits_only_latency_rule() {
        let s = sd("tenants", "po", "acme", "sc", "proc", "v1", vec![slo_lat("create", 100)]);
        let b = render_bundle(&[s]);
        assert_eq!(b.groups.len(), 1);
        assert_eq!(b.groups[0].rules.len(), 1);
        let r = &b.groups[0].rules[0];
        assert!(r.alert.starts_with("VelocityP99Latency_"));
        assert!(r.expr.contains("velocity_operation_duration_seconds_bucket"));
        assert!(r.expr.contains("operation=\"create\""));
        assert!(r.expr.contains("> 0.1"));
        assert_eq!(r.labels.get("velocity_schema").unwrap(), "acme/sc/proc/po/v1");
    }

    #[test]
    fn availability_only_slo_emits_only_error_budget_rule() {
        let s = sd(
            "tenants",
            "po",
            "acme",
            "sc",
            "proc",
            "v1",
            vec![slo_avail("read", 0.999, Some("7d"))],
        );
        let b = render_bundle(&[s]);
        assert_eq!(b.groups[0].rules.len(), 1);
        let r = &b.groups[0].rules[0];
        assert!(r.alert.starts_with("VelocityErrorBudget_"));
        assert!(r.expr.contains("schema=\"acme/sc/proc/po/v1\""));
        assert!(r.expr.contains("operation=\"read\""));
        assert!(r.expr.contains("[7d]"));
        assert!(r.expr.contains("> 0.001"), "{}", r.expr);
    }

    #[test]
    fn both_slo_fields_emit_two_rules_sorted() {
        let s = sd(
            "tenants",
            "po",
            "acme",
            "sc",
            "proc",
            "v1",
            vec![slo_both("create", 50, 0.99, "30d")],
        );
        let b = render_bundle(&[s]);
        assert_eq!(b.groups[0].rules.len(), 2);
        assert!(b.groups[0].rules[0].alert.starts_with("VelocityErrorBudget_"));
        assert!(b.groups[0].rules[1].alert.starts_with("VelocityP99Latency_"));
    }

    #[test]
    fn groups_sorted_by_name_for_deterministic_output() {
        let s1 = sd("ns-b", "po", "acme", "sc", "proc", "v1", vec![slo_lat("create", 100)]);
        let s2 = sd("ns-a", "supplier", "acme", "sc", "proc", "v1", vec![slo_lat("list", 200)]);
        let b = render_bundle(&[s1, s2]);
        let names: Vec<&str> = b.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["velocity.ns-a.supplier", "velocity.ns-b.po"]);
    }

    #[test]
    fn bundle_round_trips_through_yaml_with_prometheus_shape() {
        let s = sd(
            "tenants",
            "po",
            "acme",
            "sc",
            "proc",
            "v1",
            vec![slo_both("create", 100, 0.99, "30d")],
        );
        let b = render_bundle(&[s]);
        let yaml = serde_yaml::to_string(&b).unwrap();
        let parsed: RulesBundle = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, b);
        assert!(yaml.contains("groups:"), "{yaml}");
        assert!(yaml.contains("- alert:"), "{yaml}");
        assert!(yaml.contains("for: 5m"), "{yaml}");
    }
}
