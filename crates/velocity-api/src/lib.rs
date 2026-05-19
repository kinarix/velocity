//! Velocity REST API server.
//!
//! Phase 1 scaffolding: Axum + arc_swap-backed [`SchemaRegistry`] fed by a
//! kube informer. The registry is the only knob that decides what routes the
//! server serves — apply a `SchemaDefinition`, the informer event lands, the
//! registry updates, and the next request sees the new schema.
//!
//! See `CLAUDE.md › SchemaRegistry Implementation`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod audit;
pub mod audit_query;
pub mod auth;
pub mod auth_handlers;
pub mod auth_informer;
pub mod cdc;
pub mod config;
pub mod dsl;
pub mod error;
pub mod event_log;
pub mod field_filter;
pub mod handlers;
pub mod health;
pub mod identity;
pub mod idempotency;
pub mod informer;
pub mod masking;
pub mod metrics;
pub mod metrics_middleware;
pub mod platform_handlers;
pub mod policy;
pub mod query;
pub mod rbac;
pub mod registry;
pub mod row_filter;
pub mod router;
pub mod session;
pub mod startup;
pub mod state;
pub mod tiering;
pub mod time_machine;
pub mod typesense;
pub mod validate;

pub use auth::{AuthRegistry, JwksCache, ResolvedAuthStrategy};
pub use config::ApiConfig;
pub use error::ApiError;
pub use identity::Identity;
pub use registry::{registry_key, AccessIndex, FieldIndex, ResolvedSchema, SchemaRegistry};
pub use state::AppState;
