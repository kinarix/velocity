//! Velocity log processor.
//!
//! Receives log batches from the DaemonSet `velocity-log-collector`,
//! enriches them with `velocity.{org,app,domain,schema}` labels
//! derived from the originating pod's metadata, applies the rules
//! from the merged `LogFilterPolicy` set, and ships the kept lines
//! to one or more destinations declared in `LogRoutingPolicy`.
//!
//! ## v1 scope (per Phase 6b)
//!
//! - Filter actions: `keep`, `drop`, `sample`, `redact` — first
//!   matching `keep`/`drop` wins; `redact` mutates and continues.
//! - `when` clause matchers: equality and `*`-glob only. CEL, regex,
//!   and JSONPath are explicit non-goals for v1.
//! - Destinations: `stdout` (always available) and `http_webhook`
//!   (POST application/json). `loki` and `s3` are stubs that log a
//!   warning so misconfigured CRDs surface fast.
//! - Policy loading: read from a YAML file at startup, reload on
//!   SIGHUP. The operator's reconciler is responsible for keeping
//!   that file in sync with the cluster's CRDs.
//!
//! ## Not v1
//!
//! - No batching/retry queue/backpressure — single inbound POST,
//!   synchronous per-line destination dispatch. Overflow drops with
//!   a metric.
//! - No multi-line / log-rotation handling — `velocity-log-collector`
//!   line-buffers from `notify` events.
//! - No mTLS — bearer token (constant-time compare, same pattern as
//!   the warm-reader RPC).

pub mod config;
pub mod destination;
pub mod enrich;
pub mod policy;
pub mod rules;
pub mod server;

pub use config::ProcessorConfig;
pub use destination::{Destination, DestinationOutcome};
pub use policy::{LogFilterRuleSpec, LogPolicyBundle, LogRoutingDestSpec, RuleAction};
pub use rules::{evaluate, Decision, LogRecord};
