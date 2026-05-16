//! ADR-003 — Single source of truth for dependency failure-mode decisions.
//!
//! Every code site that checks a dependency (Redis, JWKS, Postgres, registry,
//! CEL, Typesense, Kafka, hooks) must call into [`FailMode::resolve`] rather
//! than making a local decision. The fail-mode that was applied is then
//! recorded in the audit log entry for the request.

use serde::{Deserialize, Serialize};

/// What a dependency-aware code site should do when its dependency is unavailable.
///
/// Mirrors the matrix in `docs/decisions.md §ADR-003`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    /// Reject the request (default for any auth/access dependency).
    Deny,
    /// Continue without the dependency — strictly for non-security paths.
    Continue,
    /// Use a cached / degraded answer (e.g., JWKS cached keys).
    UseCache,
    /// Fall back to a different mechanism (e.g., Typesense → Postgres FTS).
    Fallback,
    /// Queue the work for later (e.g., Kafka hook delivery).
    Queue,
    /// Surface a 503 Service Unavailable to the caller.
    ServiceUnavailable,
}

/// The dependency a fail-mode decision is being made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dependency {
    JwksFetch,
    JwksCacheEmpty,
    RedisRevocation,
    PostgresRbac,
    SchemaRegistry,
    CelEvaluator,
    HookTarget,
    Typesense,
    Kafka,
}

/// Outcome of a fail-mode resolution: what to do, and a stable label to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailModeOutcome {
    pub mode: FailMode,
    pub dependency: Dependency,
    /// Lowercase, stable label suitable for audit logs and metrics.
    pub label: &'static str,
    /// `true` if the operator/admin opted out of the strict default.
    pub overridden: bool,
}

impl FailMode {
    /// Resolve the fail-mode for a dependency outage. `fail_open` is honored
    /// only where the matrix allows an override; ignored otherwise.
    pub fn resolve(dep: Dependency, fail_open: bool) -> FailModeOutcome {
        use Dependency::*;
        match dep {
            JwksFetch => FailModeOutcome {
                mode: FailMode::UseCache,
                dependency: dep,
                label: "jwks_use_cache",
                overridden: false,
            },
            JwksCacheEmpty => FailModeOutcome {
                mode: FailMode::Deny,
                dependency: dep,
                label: "jwks_cache_empty_deny",
                overridden: false,
            },
            RedisRevocation => {
                if fail_open {
                    FailModeOutcome {
                        mode: FailMode::Continue,
                        dependency: dep,
                        label: "redis_revocation_fail_open",
                        overridden: true,
                    }
                } else {
                    FailModeOutcome {
                        mode: FailMode::Deny,
                        dependency: dep,
                        label: "redis_revocation_deny",
                        overridden: false,
                    }
                }
            }
            PostgresRbac => FailModeOutcome {
                mode: FailMode::Deny,
                dependency: dep,
                label: "postgres_rbac_deny",
                overridden: false,
            },
            SchemaRegistry => FailModeOutcome {
                mode: FailMode::ServiceUnavailable,
                dependency: dep,
                label: "registry_unavailable",
                overridden: false,
            },
            CelEvaluator => FailModeOutcome {
                mode: FailMode::Deny,
                dependency: dep,
                label: "cel_evaluator_deny",
                overridden: false,
            },
            HookTarget => FailModeOutcome {
                mode: FailMode::Queue,
                dependency: dep,
                label: "hook_queue",
                overridden: false,
            },
            Typesense => FailModeOutcome {
                mode: FailMode::Fallback,
                dependency: dep,
                label: "typesense_fallback_fts",
                overridden: false,
            },
            Kafka => FailModeOutcome {
                mode: FailMode::Queue,
                dependency: dep,
                label: "kafka_queue",
                overridden: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_default_denies() {
        let r = FailMode::resolve(Dependency::RedisRevocation, false);
        assert_eq!(r.mode, FailMode::Deny);
        assert!(!r.overridden);
    }

    #[test]
    fn redis_failopen_continues_marked_overridden() {
        let r = FailMode::resolve(Dependency::RedisRevocation, true);
        assert_eq!(r.mode, FailMode::Continue);
        assert!(r.overridden);
    }

    #[test]
    fn auth_access_dependencies_default_deny() {
        for dep in [Dependency::PostgresRbac, Dependency::CelEvaluator, Dependency::JwksCacheEmpty]
        {
            let r = FailMode::resolve(dep, false);
            assert_eq!(r.mode, FailMode::Deny, "{dep:?} should default to Deny");
            assert!(!r.overridden);
        }
    }

    #[test]
    fn non_security_deps_remain_available() {
        assert_eq!(FailMode::resolve(Dependency::Kafka, false).mode, FailMode::Queue);
        assert_eq!(FailMode::resolve(Dependency::Typesense, false).mode, FailMode::Fallback);
        assert_eq!(FailMode::resolve(Dependency::HookTarget, false).mode, FailMode::Queue);
    }

    #[test]
    fn fail_open_ignored_on_non_overridable_deps() {
        let r = FailMode::resolve(Dependency::PostgresRbac, true);
        assert_eq!(r.mode, FailMode::Deny);
        assert!(!r.overridden);
    }
}
