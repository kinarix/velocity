//! `velocity-data-api` — the per-domain data plane (Phase 12a / ADR-011).
//!
//! Owns CRUD, the query DSL (Tier-1 filters + Tier-2 Postgres FTS),
//! time-machine, and archive. It links the shared `velocity-api` core for
//! auth, the schema registry, RBAC, validation, and audit, but **none** of
//! search/CDC/Typesense (that's `velocity-search`) or the admin/CRD-write
//! surface (that's `velocity-platform-api`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod archive_handlers;
pub mod dsl;
pub mod event_log;
pub mod handlers;
pub mod idempotency;
pub mod router;
pub mod session;
pub mod startup;
pub mod state;
pub mod tiering;
pub mod time_machine;

pub use state::DataState;
