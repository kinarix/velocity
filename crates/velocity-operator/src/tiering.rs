//! Phase 4 — Time Machine tiering (ADR-004).
//!
//! Owns moving hot-tier monthly partitions of `platform.event_log` out
//! to warm-tier Parquet objects on `object_store` (S3 in prod,
//! `file://` in dev/CI), then dropping the now-redundant partition.
//!
//! Sibling to `partition_manager`:
//!   - `partition_manager` adds new monthly partitions on the leading
//!     edge so writes never fail at month boundaries.
//!   - `tiering::exporter` removes old monthly partitions on the
//!     trailing edge (>= 90 days old) and ships their data to warm.
//!
//! Coordinated split — they touch the same parent table but never the
//! same partition at the same time, and we serialize each side with a
//! distinct `pg_advisory_xact_lock` constant so they can run on
//! independent tasks without stepping on each other or on the
//! provisioner.

pub mod exporter;
pub mod object_store_url;
pub mod orphan_recovery;
pub mod schema;

pub use exporter::{run, tick};
