//! Prometheus metrics registry for the operator.
//!
//! We use a process-local `Registry` rather than the prometheus default
//! one so tests don't accidentally see metrics from other crates and
//! vice versa. Counters are constructed once and exported via the
//! health server's `/metrics` route.

use std::sync::OnceLock;

use prometheus::{IntCounterVec, Opts, Registry};

/// Process-wide registry. The health server's `/metrics` handler
/// gathers from this; tests can also pull it to assert counter values.
fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(Registry::new)
}

/// Counter for drift detections. `kind` lets dashboards split
/// orphan-table vs missing-column vs missing-index totals once those
/// are implemented; the v1 sweep only emits `kind="orphan_table"`.
pub fn drift_detected_total() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        // The metric definition is static — the only way these calls
        // could fail at runtime is the constant labels list being
        // malformed or the name colliding with an already-registered
        // metric. Either case is a programmer error caught at first
        // boot; expect with a clear message so the operator fails to
        // start rather than silently lose drift observability.
        #[allow(clippy::expect_used)]
        let c = IntCounterVec::new(
            Opts::new(
                "velocity_drift_detected_total",
                "Number of drift conditions detected by the operator's periodic sweep",
            ),
            &["kind"],
        )
        .expect("constructing drift counter (static definition)");
        #[allow(clippy::expect_used)]
        registry()
            .register(Box::new(c.clone()))
            .expect("registering drift counter (must be unique)");
        c
    })
}

/// Render the registry's current state in the standard text exposition
/// format. Called by the `/metrics` HTTP handler.
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
        // Calling twice must not panic — the second call should return
        // the same cached counter rather than re-register.
        let a = drift_detected_total();
        let b = drift_detected_total();
        assert_eq!(a as *const _, b as *const _);
    }

    #[test]
    fn gather_includes_counter() {
        drift_detected_total().with_label_values(&["orphan_table"]).inc();
        let body = gather();
        assert!(body.contains("velocity_drift_detected_total"));
        assert!(body.contains("kind=\"orphan_table\""));
    }
}
