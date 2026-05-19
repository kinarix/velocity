//! velocity-warm-reader — Phase 4 service that answers warm-tier event
//! lookups out of Parquet objects on `object_store` (S3 in prod, file://
//! in dev/CI).
//!
//! This crate exists as a separate service from `velocity-api` because:
//!   - warm scans have a different resource profile (IO + memory) than the
//!     API's QPS-heavy hot path. Mixing them puts warm scans on the API's
//!     memory budget.
//!   - failure isolation: an S3 degradation only contaminates this
//!     service's tail latency, not the API's.
//!   - future engine growth (DataFusion, decode caches) does not bloat
//!     every API replica.
//!   - read/write of warm tier are separate concerns: write lives in
//!     `velocity-archive-worker`, read here.
//!
//! See ADR-004 revision 2026-05-18 (docs/decisions.md) for the full
//! rationale and CLAUDE.md §Inter-Service RPC for the auth / tracing /
//! failure-semantics conventions this crate establishes.

#![forbid(unsafe_code)]
#![deny(unused_must_use)]

pub mod config;
pub mod datafusion_reader;
pub mod error;
pub mod http;
pub mod object_layout;
pub mod startup;
pub mod types;

pub use config::WarmReaderConfig;
pub use error::WarmReaderError;
pub use startup::{build_app_state, build_object_store, build_session};
pub use types::{EventRow, EventsRequest, EventsResponse};
