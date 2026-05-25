//! Velocity shared core library.
//!
//! The common foundation the three API tiers (`velocity-platform-api`,
//! `velocity-data-api`, `velocity-search`) build on: auth (JWT/OIDC/API key +
//! middleware), the arc_swap-backed [`SchemaRegistry`] fed by a kube informer,
//! config, the schema + access model (validation, RBAC, row/field filters,
//! masking, policy), the audit-write primitive, shared HTTP helpers, the
//! cursor signer, and the per-tier bootstrap (`server`). Each tier links this
//! crate and adds its own state struct + router on top.
//!
//! See `CLAUDE.md › SchemaRegistry Implementation`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod audit;
pub mod auth;
pub mod auth_handlers;
pub mod auth_informer;
pub mod config;
pub mod cursor;
pub mod error;
pub mod field_filter;
pub mod handler_util;
pub mod health;
pub mod identity;
pub mod informer;
pub mod masking;
pub mod metrics;
pub mod metrics_middleware;
pub mod policy;
pub mod query;
pub mod rbac;
pub mod registry;
pub mod router;
pub mod row_filter;
pub mod server;
pub mod startup;
pub mod validate;

pub use auth::{AuthRegistry, JwksCache, ResolvedAuthStrategy};
pub use config::{ApiConfig, ApiMode, AuthMode};
pub use cursor::CursorSigner;
pub use error::ApiError;
pub use identity::Identity;
pub use registry::{registry_key, AccessIndex, FieldIndex, ResolvedSchema, SchemaRegistry};
