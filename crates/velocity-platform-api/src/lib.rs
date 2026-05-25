//! `velocity-platform-api` — the admin/UI backend (Phase 12a / ADR-011).
//!
//! Owns the platform control surface: registry index + build info, the
//! platform audit read/verify endpoints, and the embedded portal SPA. It
//! links the shared `velocity-api` core for auth, the schema registry, and
//! the audit-write primitive, but **none** of the data plane (CRUD/query/
//! tiering) or search/CDC. The admin CRD read/write endpoints live in the
//! binary's `platform_objects` module.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod audit_query;
pub mod platform_handlers;
pub mod router;
pub mod state;
pub mod static_files;

pub use state::PlatformState;
