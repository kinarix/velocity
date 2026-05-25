//! `velocity-search` — the search tier (Phase 12a / ADR-011).
//!
//! Owns ALL Tier-3 search: the per-schema and per-org cross-domain
//! Typesense collections, the search HTTP handlers, and the CDC
//! outbox→Typesense worker. It links the shared `velocity-api` core for
//! auth, the schema registry, RBAC, and audit, but **none** of the data
//! plane (CRUD, query DSL, tiering, time-machine).

pub mod cdc;
pub mod router;
pub mod search_handlers;
pub mod state;
