//! Velocity ValidatingWebhook.
//!
//! Phase 0 scope: namespace-vs-labels check for the hierarchy CRDs +
//! basic CEL syntactic safety. Cross-domain ref existence, quota
//! enforcement, etc. land in their respective phases.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod handler;
pub mod validators;

pub use config::WebhookConfig;
