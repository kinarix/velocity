//! Shared reconciler context.

use std::sync::Arc;

use dashmap::DashMap;
use kube::Client;
use sqlx::PgPool;
use tokio::sync::watch;

use crate::provisioner::PostgresProvisioner;

/// State shared by every controller. Cheap to `Arc::clone`.
#[derive(Clone)]
pub struct Context {
    pub kube: Client,
    pub pg: PgPool,
    pub provisioner: Arc<PostgresProvisioner>,
    /// Reconcile-skip cache: uid → hash of (spec, effective policy).
    pub last_hash: Arc<DashMap<String, String>>,
    /// Readiness signal — flipped to `true` once the first informer sync completes.
    pub ready_tx: watch::Sender<bool>,
}

impl Context {
    pub fn new(kube: Client, pg: PgPool, ready_tx: watch::Sender<bool>) -> Self {
        let provisioner = Arc::new(PostgresProvisioner::new(pg.clone()));
        Self { kube, pg, provisioner, last_hash: Arc::new(DashMap::new()), ready_tx }
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("hash_cache_len", &self.last_hash.len())
            .finish_non_exhaustive()
    }
}
