//! kube-rs reconcilers for the hierarchy CRDs.

pub mod application;
pub mod archive_policy;
pub mod domain;
pub mod organisation;
pub mod role_binding;
pub mod schema_definition;

use std::time::Duration;

use kube::runtime::controller::Action;
use thiserror::Error;

/// What any reconciler can produce. Errors are typed so the error_policy can
/// classify retries.
#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("kube client: {0}")]
    Kube(#[from] kube::Error),

    #[error("provisioning: {0}")]
    Provision(#[from] crate::provisioner::ProvisionError),

    #[error("invalid object: {0}")]
    Invalid(String),

    #[error("typesense: {0}")]
    Typesense(#[from] velocity_typesense::TypesenseError),
}

/// Backoff applied when a reconcile errors. Conservative for Phase 0;
/// per-controller jitter lands when the cascade-to-children logic from
/// `CLAUDE.md › Reconcile storm prevention` is wired in.
pub fn error_action(err: &ReconcileError) -> Action {
    let backoff = match err {
        ReconcileError::Invalid(_) => Duration::from_secs(600), // bad spec — slow down
        _ => Duration::from_secs(15),
    };
    Action::requeue(backoff)
}
