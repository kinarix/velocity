//! Shared types for the Velocity platform.
//!
//! - CRD struct definitions (k8s `velocity.sh/v1`) under [`crds`]
//! - `ResolvedSchema` — the post-merge runtime type used by `SchemaRegistry`
//! - `SchemaPath`, sanitization helpers, common cross-CRD references
//! - `Identity`, `AuditContext`, `FailMode` — request/response context
//!
//! Generated CRD manifests live in `crds/` at the workspace root, produced by
//! `cargo run -p velocity-types --bin generate-crds`.

#![doc(html_root_url = "https://docs.rs/velocity-types/0.1.0")]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod common;
pub mod crds;
pub mod fail_mode;
pub mod identity;
pub mod resolved;

pub use common::{sanitize, ObjectRef, SchemaPath};
pub use fail_mode::{FailMode, FailModeOutcome};
pub use identity::{ActorType, AuditContext, Identity};
pub use resolved::{Lifecycle, ResolvedSchema};

/// CRD API group used by all Velocity custom resources.
pub const API_GROUP: &str = "velocity.sh";

/// CRD API version used by all Velocity custom resources.
pub const API_VERSION: &str = "v1";
