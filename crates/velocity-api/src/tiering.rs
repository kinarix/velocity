//! Phase 4 tier router for time-machine reads.
//!
//! Hot tier (Postgres `platform.event_log`) for `until` within the
//! hot window. Warm tier (`velocity-warm-reader` over HTTP, reading
//! Parquet on `object_store`) outside the hot window but inside the
//! warm window. Cold tier (deferred Glacier integration) emits a 202
//! with a job ID — see `cold_stub`.
//!
//! The trait is intentionally narrow: `events_for(path, entity, until,
//! limit) -> Vec<EventRow>`. The JSON-Patch fold lives above the trait
//! in `time_machine.rs`, unchanged across tiers. That keeps the
//! tier-router's surface small and the fold logic single-implementation.
//!
//! What does NOT go through this router:
//!   - `history()` listings — they return audit-metadata-rich rows
//!     (actor, source, request_id) the warm Parquet schema doesn't
//!     carry yet. Listings stay hot-only; users wanting warm-tier
//!     audit replay will use a future dedicated endpoint. See the
//!     ADR-004 revision (docs/decisions.md) for context.
//!   - `restore()` writes — only the hot tier supports them. The
//!     handler explicitly returns 422 RESTORE_TIER_UNSUPPORTED when
//!     the target timestamp is outside the hot window.

pub mod cold_stub;
pub mod event_reader;
pub mod postgres_reader;
pub mod router;
pub mod warm_reader;

pub use event_reader::{EventReader, EventRow, TierError};
pub use postgres_reader::PostgresEventReader;
pub use router::{Tier, TierWindows, TieredEventReader};
pub use warm_reader::WarmEventReader;
