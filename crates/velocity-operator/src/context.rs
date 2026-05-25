//! Shared reconciler context.

use std::sync::Arc;

use dashmap::DashMap;
use kube::Client;
use sqlx::PgPool;
use tokio::sync::watch;
use velocity_typesense::TypesenseClient;

use crate::provisioner::PostgresProvisioner;
use crate::redis_notify::RedisNotify;
use crate::search_rebuild::RebuildRegistry;

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
    /// Redis publisher for actor revocations. `None` disables the notify
    /// half of the RoleBinding reconciler — the DB row is still written, so
    /// audit/replay reasoning stays intact. Operators in dev environments
    /// without Redis can run without it; production wires it from
    /// `VELOCITY_OPERATOR_REDIS_URL`.
    pub redis: Option<RedisNotify>,
    /// Typesense client used by the SchemaDefinition reconciler to
    /// eagerly create per-schema collections (Phase 5d-2). `None` means
    /// the operator was started without `VELOCITY_OPERATOR_TYPESENSE_URL`
    /// — Tier-3 reconciles skip eager provisioning and the API's CDC
    /// worker handles collection creation lazily on first write.
    /// `TypesenseClient` is itself clone-cheap (the inner `reqwest::Client`
    /// is `Arc`-wrapped), so we don't double-wrap it here.
    pub typesense: Option<TypesenseClient>,
    /// Phase 5d-3b: in-flight Tier-3 Typesense rebuilds, keyed by
    /// SchemaDefinition uid. Lets the reconciler detect "a rebuild
    /// for this schema is already running with the same target" and
    /// avoid spawning a duplicate, or cancel the running one when
    /// the user has applied a newer-yet spec.
    pub rebuilds: Arc<RebuildRegistry>,
    /// Phase 12a (ADR-011): settings for materialising per-domain data-API
    /// Deployments. `None` when `VELOCITY_OPERATOR_DATA_API_IMAGE` is unset —
    /// the workload orchestrator is then disabled and a `dedicated` Domain
    /// reconciles its Postgres state but creates no Deployment (logged).
    pub data_api: Option<DataApiSettings>,
}

/// Inputs the workload orchestrator needs to build a data-API Deployment.
#[derive(Clone, Debug)]
pub struct DataApiSettings {
    /// Container image for the data-API (the same `velocity-api` binary,
    /// run with `VELOCITY_API_MODE=data`).
    pub image: String,
    /// Propagate the Phase 12b anonymous auth bypass onto the data-API pod.
    pub anonymous_auth: bool,
    /// Shared Ingress host under which per-domain paths are generated
    /// (`/api/{org}/{app}/{domain}`). `None` → the operator creates the
    /// Deployment, Service and HPA but no Ingress (e.g. routing managed
    /// externally).
    pub ingress_host: Option<String>,
    /// Name of the Secret the operator *creates* in each domain namespace
    /// (referenced by the data-API Deployment's `envFrom`). The operator
    /// fills it with the source-secret env plus the per-domain minted DB
    /// credential. `None` → no projection (pod runs without DB env).
    pub env_secret: Option<String>,
    /// Name of the source Secret (in `system_namespace`) holding the shared
    /// `VELOCITY_API_*` env (`PG_HOST`/`PORT`/`DB`, cursor key, …) the
    /// operator copies into each domain namespace.
    pub env_source_secret: Option<String>,
    /// The operator's own namespace, where the source Secret lives.
    pub system_namespace: String,
}

impl Context {
    pub fn new(kube: Client, pg: PgPool, ready_tx: watch::Sender<bool>) -> Self {
        let provisioner = Arc::new(PostgresProvisioner::new(pg.clone()));
        Self {
            kube,
            pg,
            provisioner,
            last_hash: Arc::new(DashMap::new()),
            ready_tx,
            redis: None,
            typesense: None,
            rebuilds: Arc::new(RebuildRegistry::new()),
            data_api: None,
        }
    }

    /// Builder-style: enable the Phase 12a workload orchestrator.
    pub fn with_data_api(mut self, settings: DataApiSettings) -> Self {
        self.data_api = Some(settings);
        self
    }

    /// Builder-style: install the Redis revocation publisher.
    pub fn with_redis(mut self, redis: RedisNotify) -> Self {
        self.redis = Some(redis);
        self
    }

    /// Builder-style: install the Typesense client used for eager
    /// collection provisioning (Phase 5d-2).
    pub fn with_typesense(mut self, ts: TypesenseClient) -> Self {
        self.typesense = Some(ts);
        self
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("hash_cache_len", &self.last_hash.len())
            .finish_non_exhaustive()
    }
}
