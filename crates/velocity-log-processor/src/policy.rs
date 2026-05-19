//! On-disk policy file shape. Mirrors the CRDs but keeps the parsed
//! form decoupled from the CRD structs — the reconciler is responsible
//! for the CRD → file projection so this crate doesn't drag in
//! `kube`/`schemars`.
//!
//! File layout (YAML):
//!
//! ```yaml
//! filters:
//!   - name: drop-debug
//!     priority: 10
//!     action: drop
//!     when: { level: "DEBUG" }
//!   - name: redact-tokens
//!     priority: 20
//!     action: redact
//!     fields: [ "headers.authorization" ]
//! destinations:
//!   - name: stdout
//!     kind: stdout
//!   - name: alerts
//!     kind: http_webhook
//!     url: https://hooks.example/log
//! ```
//!
//! Missing `filters` → "keep everything"; missing `destinations` →
//! "stdout only", so a freshly-deployed processor with no policy still
//! behaves like a tee.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level bundle as written by the operator's reconciler.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogPolicyBundle {
    #[serde(default)]
    pub filters: Vec<LogFilterRuleSpec>,
    #[serde(default)]
    pub destinations: Vec<LogRoutingDestSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogFilterRuleSpec {
    pub name: String,
    /// Lower runs first. Within a single bundle, ties broken by
    /// insertion order (stable sort).
    pub priority: i32,
    pub action: RuleAction,
    /// Equality / `*`-glob match. AND across keys. Missing fields don't
    /// match. Keys may be dotted to walk into nested JSON.
    #[serde(default)]
    pub when: BTreeMap<String, serde_json::Value>,
    /// Used by `redact`: fields to overwrite with `"***"`. Also dotted.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Used by `sample`: 0.0–1.0 probability of keep.
    #[serde(default)]
    pub sample_rate: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Keep,
    Drop,
    Sample,
    Redact,
}

/// `deny_unknown_fields` is intentionally absent: the `#[flatten]`
/// `config` bag collects any extra keys per destination kind, so unknown
/// keys are not a typo to report — they're how `url`, `bucket`, etc.
/// reach the destination builder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LogRoutingDestSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, flatten)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl LogPolicyBundle {
    /// Parse from a YAML string. Empty input → an empty bundle, which
    /// keeps the "no policy file yet" case ergonomic.
    pub fn from_yaml(s: &str) -> Result<Self> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml::from_str(s).context("parsing log policy bundle YAML")
    }

    /// Read+parse the file. Missing file is treated as an empty bundle;
    /// that way a fresh processor before the operator's first reconcile
    /// still starts.
    pub async fn load_or_empty(path: &Path) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(s) => Self::from_yaml(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context(format!("reading policy file {}", path.display())),
        }
    }

    /// Sort filters by `(priority, insertion order)`. Operates in-place
    /// so callers can keep a single `Arc<Bundle>` and reuse it for many
    /// log lines without re-sorting per record.
    pub fn sort_filters(&mut self) {
        self.filters.sort_by_key(|r| r.priority);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn empty_yaml_yields_empty_bundle() {
        let b = LogPolicyBundle::from_yaml("").unwrap();
        assert!(b.filters.is_empty());
        assert!(b.destinations.is_empty());
    }

    #[test]
    fn parses_full_example() {
        let yaml = r#"
filters:
  - name: drop-debug
    priority: 10
    action: drop
    when:
      level: DEBUG
  - name: redact-tokens
    priority: 20
    action: redact
    fields: ["headers.authorization"]
destinations:
  - name: console
    type: stdout
  - name: hook
    type: http_webhook
    url: https://example.test/log
"#;
        let b = LogPolicyBundle::from_yaml(yaml).unwrap();
        assert_eq!(b.filters.len(), 2);
        assert_eq!(b.filters[0].action, RuleAction::Drop);
        assert_eq!(b.filters[1].action, RuleAction::Redact);
        assert_eq!(b.destinations.len(), 2);
        assert_eq!(b.destinations[1].kind, "http_webhook");
        assert_eq!(
            b.destinations[1].config["url"].as_str(),
            Some("https://example.test/log")
        );
    }

    #[test]
    fn sort_filters_orders_by_priority_ascending() {
        let mut b = LogPolicyBundle {
            filters: vec![
                LogFilterRuleSpec {
                    name: "third".into(),
                    priority: 30,
                    action: RuleAction::Keep,
                    when: BTreeMap::new(),
                    fields: vec![],
                    sample_rate: None,
                },
                LogFilterRuleSpec {
                    name: "first".into(),
                    priority: 10,
                    action: RuleAction::Drop,
                    when: BTreeMap::new(),
                    fields: vec![],
                    sample_rate: None,
                },
                LogFilterRuleSpec {
                    name: "second".into(),
                    priority: 20,
                    action: RuleAction::Redact,
                    when: BTreeMap::new(),
                    fields: vec![],
                    sample_rate: None,
                },
            ],
            destinations: vec![],
        };
        b.sort_filters();
        assert_eq!(
            b.filters.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        // Catches typos in operator-generated YAML rather than silently
        // ignoring them (deny_unknown_fields).
        let yaml = "filtersz: []\n";
        assert!(LogPolicyBundle::from_yaml(yaml).is_err());
    }
}
