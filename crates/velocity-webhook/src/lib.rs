//! Velocity ValidatingWebhook.
//!
//! Phase 0 scope: namespace-vs-labels check for the hierarchy CRDs +
//! basic CEL syntactic safety. Cross-domain ref existence, quota
//! enforcement, etc. land in their respective phases.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod handler;
pub mod startup;
pub mod strategy_check;
pub mod validators;

pub use config::WebhookConfig;
pub use startup::{build_admission_router, build_health_router};
pub use strategy_check::{
    validate_auth_strategy_ref, AuthStrategyExists, KubeStrategyChecker, MockStrategyChecker,
};
