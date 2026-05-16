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
pub mod health;
pub mod migration_diff;
pub mod provisioner;
pub mod startup;

pub use config::OperatorConfig;
pub use context::Context;
pub use ddl_builder::{build_ddl, ColumnSpec, DdlError, DdlPlan};
pub use migration_diff::{classify, diff_columns, DiffError, MigrationOp};
pub use provisioner::{PostgresProvisioner, ProvisionError};
