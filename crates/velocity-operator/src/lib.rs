//! Velocity operator.
//!
//! Phase 0 ships the **HierarchyOperator** — watches `Organisation`,
//! `Application`, `Domain` and provisions Postgres schemas + per-domain
//! roles. Other operators (SchemaOperator, etc.) land in later phases.
//!
//! See `CLAUDE.md › Operator Patterns` and `docs/phases.md › Phase 0`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod context;
pub mod controllers;
pub mod ddl_builder;
pub mod drift_sweep;
pub mod health;
pub mod metrics;
pub mod migration_diff;
pub mod partition_manager;
pub mod provisioner;
pub mod redis_notify;
pub mod search_rebuild;
pub mod startup;
pub mod tiering;

pub use config::OperatorConfig;
pub use context::Context;
pub use ddl_builder::{build_ddl, ColumnSpec, DdlError, DdlPlan};
pub use migration_diff::{classify, diff_columns, DiffError, MigrationOp};
pub use provisioner::{PostgresProvisioner, ProvisionError};
pub use redis_notify::{RedisNotify, RedisNotifyError, DEFAULT_REVOKED_SET_KEY};
