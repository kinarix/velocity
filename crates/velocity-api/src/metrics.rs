//! Prometheus metrics for the API server.
//!
//! Process-local `Registry` (mirrors the operator's pattern in
//! `velocity-operator::metrics`). Two distinct registries — one per
//! crate — is deliberate: tests in one crate must not see counters from
//! the other, and the two services scrape independently.
//!
//! Cardinality discipline (CLAUDE.md › Observability Patterns):
//!
//! * `velocity_operations_total` carries `schema` because per-schema
//!   traffic + error rates are essential to dashboards and alerts.
//! * `velocity_operation_duration_seconds` does **not** carry `schema` —
//!   the histogram is high-cardinality already (buckets × operation ×
//!   outcome) and a schema label would explode it. Per-schema latency
//!   distributions belong in traces, not in a metric.
//! * `velocity_validation_failures_total{schema, field, rule}` —
//!   `field` MUST be a CRD-declared field name (resolved against
//!   `ResolvedSchema.fields`). Never a string taken from the user
//!   payload, or we'd let a hostile client pump unbounded series.
//!
//! Labels use a small enumerated vocabulary; the [`label`] module
//! exposes those constants so handlers don't risk typos.

use std::sync::OnceLock;

use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry};

/// Approved label values — used to keep cardinality bounded and to
/// avoid string typos in handlers. Refer to CLAUDE.md › Metric label
/// cardinality.
pub mod label {
    pub mod outcome {
        pub const SUCCESS: &str = "success";
        pub const ERROR: &str = "error";
        pub const DENIED: &str = "denied";
        pub const VALIDATION_ERROR: &str = "validation_error";
        pub const NOT_FOUND: &str = "not_found";
    }

    pub mod operation {
        pub const LIST: &str = "list";
        pub const CREATE: &str = "create";
        pub const READ: &str = "read";
        pub const UPDATE: &str = "update";
        pub const DELETE: &str = "delete";
        pub const QUERY: &str = "query";
        pub const SEARCH: &str = "search";
        pub const CROSS_SEARCH: &str = "cross_search";
        pub const HISTORY: &str = "history";
        pub const DIFF: &str = "diff";
        pub const RESTORE: &str = "restore";
        pub const REPLAY: &str = "replay";
        pub const SNAPSHOT: &str = "snapshot";
        pub const OTHER: &str = "other";
    }

    pub mod actor_type {
        pub const HUMAN: &str = "human";
        pub const API_KEY: &str = "api_key";
        pub const ANONYMOUS: &str = "anonymous";
    }

    /// Used when the metrics middleware can't determine the schema for a
    /// request — this is bounded to a single series for all such requests
    /// (paths under `/api/platform/*`, `/auth/*`, the root index, etc.).
    pub const SCHEMA_UNKNOWN: &str = "unknown";
}

/// Histogram buckets for request latency in seconds — covers the
/// p50-of-fast-reads (~2ms) through the p99-of-slow-queries (~5s)
/// without exploding bucket count.
const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0,
];

fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(Registry::new)
}

/// Per-schema operation counter — driven by the metrics middleware on
/// every API request that exits the handler stack.
///
/// Labels:
/// * `schema` — `{org}/{app}/{domain}/{object}/{version}` or
///   [`label::SCHEMA_UNKNOWN`] for non-schema routes.
/// * `operation` — see [`label::operation`].
/// * `outcome` — see [`label::outcome`].
/// * `actor_type` — see [`label::actor_type`].
/// * `strategy` — `{namespace}/{name}` of the admitting `AuthStrategy`,
///   or `""` for anonymous requests / unauthenticated paths.
pub fn operations_total() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        #[allow(clippy::expect_used)]
        let c = IntCounterVec::new(
            Opts::new(
                "velocity_operations_total",
                "Total API operations by schema, op, outcome, actor type, and admitting strategy",
            ),
            &["schema", "operation", "outcome", "actor_type", "strategy"],
        )
        .expect("constructing operations counter (static definition)");
        #[allow(clippy::expect_used)]
        registry()
            .register(Box::new(c.clone()))
            .expect("registering operations counter (must be unique)");
        c
    })
}

/// Operation latency histogram. Intentionally has **no** `schema` label
/// — per-schema latency distributions belong in traces, not metrics.
/// Combining histogram buckets × schema would multiply series by a
/// factor we cannot bound.
pub fn operation_duration_seconds() -> &'static HistogramVec {
    static HIST: OnceLock<HistogramVec> = OnceLock::new();
    HIST.get_or_init(|| {
        #[allow(clippy::expect_used)]
        let h = HistogramVec::new(
            HistogramOpts::new(
                "velocity_operation_duration_seconds",
                "API operation latency seconds, by op + outcome (no schema label — see CLAUDE.md)",
            )
            .buckets(DURATION_BUCKETS.to_vec()),
            &["operation", "outcome"],
        )
        .expect("constructing duration histogram (static definition)");
        #[allow(clippy::expect_used)]
        registry()
            .register(Box::new(h.clone()))
            .expect("registering duration histogram (must be unique)");
        h
    })
}

/// Validation-failure counter. Called from the validation code paths
/// (write, update) when a field fails a typed check or a CEL rule.
///
/// `field` MUST be sourced from a [`ResolvedSchema`] field name (or the
/// fixed sentinel `"<schema>"` for whole-record CEL rules), never from
/// the request payload — otherwise a hostile client pumps cardinality.
pub fn validation_failures_total() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        #[allow(clippy::expect_used)]
        let c = IntCounterVec::new(
            Opts::new(
                "velocity_validation_failures_total",
                "Field- and rule-level validation failures by schema",
            ),
            &["schema", "field", "rule"],
        )
        .expect("constructing validation counter (static definition)");
        #[allow(clippy::expect_used)]
        registry()
            .register(Box::new(c.clone()))
            .expect("registering validation counter (must be unique)");
        c
    })
}

/// Auth-attempt counter. Incremented by the auth middleware on every
/// admission decision. `strategy` is the leaf strategy key
/// (`{namespace}/{name}`); `outcome` is one of [`label::outcome`].
/// Carries `schema` because dashboards split admit/deny by schema.
pub fn auth_attempts_total() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        #[allow(clippy::expect_used)]
        let c = IntCounterVec::new(
            Opts::new(
                "velocity_auth_attempts_total",
                "Authentication attempts by schema, strategy, and outcome",
            ),
            &["schema", "strategy", "outcome"],
        )
        .expect("constructing auth counter (static definition)");
        #[allow(clippy::expect_used)]
        registry()
            .register(Box::new(c.clone()))
            .expect("registering auth counter (must be unique)");
        c
    })
}

/// Render the registry in Prometheus text exposition format. Called
/// from the `/metrics` HTTP handler on the health router.
pub fn gather() -> String {
    use prometheus::Encoder;
    let mut buf = Vec::new();
    let metric_families = registry().gather();
    let encoder = prometheus::TextEncoder::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buf) {
        return format!("# encode error: {e}\n");
    }
    String::from_utf8(buf).unwrap_or_else(|e| format!("# utf8 error: {e}\n"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn counter_registration_is_idempotent() {
        let a = operations_total();
        let b = operations_total();
        assert_eq!(a as *const _, b as *const _);
        let a = operation_duration_seconds();
        let b = operation_duration_seconds();
        assert_eq!(a as *const _, b as *const _);
    }

    #[test]
    fn gather_emits_text_format_for_all_metrics() {
        // Touch each metric so it appears in the registry output.
        operations_total()
            .with_label_values(&[
                "acme/sc/proc/po/v1",
                label::operation::CREATE,
                label::outcome::SUCCESS,
                label::actor_type::HUMAN,
                "ns/jwt",
            ])
            .inc();
        operation_duration_seconds()
            .with_label_values(&[label::operation::CREATE, label::outcome::SUCCESS])
            .observe(0.012);
        validation_failures_total()
            .with_label_values(&["acme/sc/proc/po/v1", "po_number", "regex"])
            .inc();
        auth_attempts_total()
            .with_label_values(&["acme/sc/proc/po/v1", "ns/jwt", label::outcome::SUCCESS])
            .inc();

        let body = gather();
        assert!(body.contains("velocity_operations_total"));
        assert!(body.contains("velocity_operation_duration_seconds"));
        assert!(body.contains("velocity_validation_failures_total"));
        assert!(body.contains("velocity_auth_attempts_total"));
        // Spot-check label rendering on the counter we just bumped.
        assert!(body.contains("operation=\"create\""));
        assert!(body.contains("outcome=\"success\""));
    }
}
