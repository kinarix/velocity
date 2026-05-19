//! Pure rule evaluation: take a parsed log record + the sorted rules,
//! produce a `Decision` (keep / drop / sampled-out) plus optional
//! mutated payload for redacts.
//!
//! Kept pure — no IO, no async — so the fuzz/property surface is
//! exactly the rule semantics and nothing else.

use std::collections::BTreeMap;

use rand::Rng;
use serde_json::{json, Value};

use crate::policy::{LogFilterRuleSpec, RuleAction};

/// A single inbound log line. We accept anything serde-shaped so the
/// collector can ship JSON or wrap a plaintext line as
/// `{"message": "..."}` and the rules engine still walks it.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub payload: Value,
}

impl LogRecord {
    pub fn new(payload: Value) -> Self {
        Self { payload }
    }
}

/// Outcome of evaluating the rule chain against a record.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Forward to all destinations (possibly with the in-place
    /// redactions applied).
    Keep,
    /// Drop silently — no destination dispatch, just bump the metric.
    Drop,
    /// Sampled-out — drops with a distinct metric so operators can
    /// tell the difference between "rule dropped it" and "sampling
    /// rolled the dice".
    Sampled,
}

/// Evaluate the rule chain. The rules list MUST be sorted by priority
/// already (caller's job — `LogPolicyBundle::sort_filters` does it
/// once, then the bundle is shared across many calls).
///
/// Semantics:
/// 1. `Redact` rules mutate `record.payload` in place and the chain
///    continues. Lets the operator stack multiple redactions before a
///    `keep`/`drop` makes a terminal call.
/// 2. `Sample` rules roll a die; on "out" the chain terminates with
///    `Sampled`; on "in" the chain continues so a later `redact` can
///    still scrub fields before forwarding.
/// 3. The first `Keep` or `Drop` whose `when` matches is terminal.
/// 4. If the chain ends with no `keep`/`drop` match, the record is
///    kept by default — fail-open is the right call for an empty
///    policy; an operator who wants fail-closed adds a final
///    `priority: i32::MAX, action: drop, when: {}` rule.
pub fn evaluate(rules: &[LogFilterRuleSpec], record: &mut LogRecord) -> Decision {
    let mut rng = rand::thread_rng();
    for rule in rules {
        if !matches_when(&rule.when, &record.payload) {
            continue;
        }
        match rule.action {
            RuleAction::Redact => {
                redact_fields(&mut record.payload, &rule.fields);
            }
            RuleAction::Sample => {
                let rate = rule.sample_rate.unwrap_or(1.0).clamp(0.0, 1.0);
                let kept: bool = rng.gen_bool(rate);
                if !kept {
                    return Decision::Sampled;
                }
            }
            RuleAction::Keep => return Decision::Keep,
            RuleAction::Drop => return Decision::Drop,
        }
    }
    Decision::Keep
}

/// AND across `when` keys. Missing keys → no match. Glob is `*` only
/// — kept deliberately simple so a malformed pattern can't loop the
/// processor (no regex backtracking).
fn matches_when(when: &BTreeMap<String, Value>, record: &Value) -> bool {
    for (key, expected) in when {
        let Some(actual) = walk(record, key) else { return false };
        if !value_matches(expected, actual) {
            return false;
        }
    }
    true
}

fn value_matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::String(e), Value::String(a)) => glob_match(e, a),
        // Numbers and bools fall through to strict equality.
        _ => expected == actual,
    }
}

/// Walk a dotted JSON path. Returns `None` for missing intermediate
/// keys or out-of-bounds array indices. Array indices are bare
/// integers in the path (`items.0.id`); JSON object keys win over a
/// numeric interpretation if both apply.
fn walk<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Same walk but yields `&mut Value`. Used for in-place redaction.
fn walk_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get_mut(seg)?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get_mut(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn redact_fields(payload: &mut Value, fields: &[String]) {
    for f in fields {
        if let Some(target) = walk_mut(payload, f) {
            *target = json!("***");
        }
        // Missing field: silently ignored. Redact is best-effort —
        // we don't want a typo'd rule to log-spam on every line.
    }
}

/// `*` matches any run of characters (including empty). No `?`, no
/// character classes. A pattern with no `*` is an exact match.
fn glob_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    // Splitting on `*` gives the ordered sequence of literal chunks
    // that must appear in order. Anchored at the ends iff the pattern
    // doesn't start/end with `*`.
    let parts: Vec<&str> = pattern.split('*').collect();
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let mut idx = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match value[idx..].find(part) {
            None => return false,
            Some(pos) => {
                if i == 0 && anchored_start && pos != 0 {
                    return false;
                }
                idx += pos + part.len();
            }
        }
    }
    !(anchored_end && idx != value.len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn rule(name: &str, priority: i32, action: RuleAction) -> LogFilterRuleSpec {
        LogFilterRuleSpec {
            name: name.into(),
            priority,
            action,
            when: BTreeMap::new(),
            fields: vec![],
            sample_rate: None,
        }
    }

    fn rec(v: Value) -> LogRecord {
        LogRecord::new(v)
    }

    #[test]
    fn empty_rules_keeps_default() {
        let mut r = rec(json!({"level": "INFO"}));
        assert_eq!(evaluate(&[], &mut r), Decision::Keep);
    }

    #[test]
    fn drop_rule_drops_matching_record() {
        let mut r = rule("d", 10, RuleAction::Drop);
        r.when.insert("level".into(), json!("DEBUG"));
        let mut rec = rec(json!({"level": "DEBUG", "msg": "x"}));
        assert_eq!(evaluate(&[r], &mut rec), Decision::Drop);
    }

    #[test]
    fn drop_rule_skips_non_matching_record() {
        let mut r = rule("d", 10, RuleAction::Drop);
        r.when.insert("level".into(), json!("DEBUG"));
        let mut rec = rec(json!({"level": "INFO"}));
        assert_eq!(evaluate(&[r], &mut rec), Decision::Keep);
    }

    #[test]
    fn missing_field_in_when_does_not_match() {
        let mut r = rule("d", 10, RuleAction::Drop);
        r.when.insert("absent".into(), json!("anything"));
        let mut rec = rec(json!({"level": "INFO"}));
        assert_eq!(evaluate(&[r], &mut rec), Decision::Keep);
    }

    #[test]
    fn redact_mutates_then_chain_continues_to_keep() {
        let mut redact = rule("r", 10, RuleAction::Redact);
        redact.fields = vec!["headers.authorization".into()];
        let keep = rule("k", 20, RuleAction::Keep);
        let mut record = rec(json!({
            "headers": {"authorization": "Bearer abc"},
            "msg": "x",
        }));
        let d = evaluate(&[redact, keep], &mut record);
        assert_eq!(d, Decision::Keep);
        assert_eq!(record.payload["headers"]["authorization"], json!("***"));
    }

    #[test]
    fn glob_matches_prefix_and_suffix() {
        // anchored prefix
        let mut r = rule("d", 10, RuleAction::Drop);
        r.when.insert("source".into(), json!("kube-system/*"));
        let mut a = rec(json!({"source": "kube-system/coredns"}));
        assert_eq!(evaluate(&[r.clone()], &mut a), Decision::Drop);
        let mut b = rec(json!({"source": "default/app"}));
        assert_eq!(evaluate(&[r], &mut b), Decision::Keep);
    }

    #[test]
    fn glob_matches_substring() {
        let mut r = rule("d", 10, RuleAction::Drop);
        r.when.insert("msg".into(), json!("*health*"));
        let mut a = rec(json!({"msg": "GET /healthz 200"}));
        assert_eq!(evaluate(&[r.clone()], &mut a), Decision::Drop);
        let mut b = rec(json!({"msg": "GET /api 200"}));
        assert_eq!(evaluate(&[r], &mut b), Decision::Keep);
    }

    #[test]
    fn sample_rate_zero_always_drops() {
        let mut r = rule("s", 10, RuleAction::Sample);
        r.sample_rate = Some(0.0);
        let mut rec = rec(json!({"x": 1}));
        assert_eq!(evaluate(&[r], &mut rec), Decision::Sampled);
    }

    #[test]
    fn sample_rate_one_always_keeps() {
        let mut r = rule("s", 10, RuleAction::Sample);
        r.sample_rate = Some(1.0);
        let mut rec = rec(json!({"x": 1}));
        // Sample-keep falls through to the default Keep.
        assert_eq!(evaluate(&[r], &mut rec), Decision::Keep);
    }

    #[test]
    fn first_terminal_rule_wins() {
        // Two drop rules both match — first one returns, never reach the
        // second (which would return the same answer but the test
        // catches a rewrite that runs the whole chain regardless).
        let mut r1 = rule("first", 10, RuleAction::Drop);
        r1.when.insert("k".into(), json!("v"));
        let r2 = rule("second", 20, RuleAction::Keep);
        let mut rec = rec(json!({"k": "v"}));
        assert_eq!(evaluate(&[r1, r2], &mut rec), Decision::Drop);
    }

    #[test]
    fn walk_handles_arrays_and_objects() {
        let v = json!({"a": [{"b": 7}]});
        assert_eq!(walk(&v, "a.0.b"), Some(&json!(7)));
        assert_eq!(walk(&v, "a.5.b"), None);
        assert_eq!(walk(&v, "absent"), None);
    }

    #[test]
    fn redact_missing_field_is_noop() {
        let mut redact = rule("r", 10, RuleAction::Redact);
        redact.fields = vec!["absent.field".into()];
        let keep = rule("k", 20, RuleAction::Keep);
        let mut record = rec(json!({"msg": "hi"}));
        assert_eq!(evaluate(&[redact, keep], &mut record), Decision::Keep);
        assert_eq!(record.payload, json!({"msg": "hi"}));
    }
}
