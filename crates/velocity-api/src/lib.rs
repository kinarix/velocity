//! Velocity REST API server.
//!
//! Phase 1 scaffolding: Axum + arc_swap-backed [`SchemaRegistry`] fed by a
//! kube informer. The registry is the only knob that decides what routes the
//! server serves — apply a `SchemaDefinition`, the informer event lands, the
//! registry updates, and the next request sees the new schema.
//!
//! See `CLAUDE.md › SchemaRegistry Implementation`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod health;
pub mod informer;
pub mod registry;
pub mod router;
pub mod startup;
pub mod state;

pub use config::ApiConfig;
pub use registry::{registry_key, ResolvedSchema, SchemaRegistry};
pub use state::AppState;
