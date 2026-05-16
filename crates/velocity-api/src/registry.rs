//! Lock-free, in-memory schema registry (ADR-006).
//!
//! Reads are an atomic pointer load — every request handler hits this on the
//! hot path, so we use [`arc_swap::ArcSwap`] instead of `RwLock`. Writes
//! arrive from the kube informer (see [`crate::informer`]) and replace the
//! inner state via a single `store()`.
//!
//! Readiness: the `/readyz` probe gates on `ready_rx`, which flips to `true`
//! after the informer's first `InitDone` event. Until then, the Kubernetes
//! Service excludes this pod and no traffic arrives.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use velocity_types::common::SchemaPath;
use velocity_types::crds::SchemaDefinitionSpec;

/// A `SchemaDefinition` resolved into the form the API needs to serve traffic.
///
/// Phase 1 keeps this thin — just the path, Postgres coordinates, and a
/// snapshot of the spec the handlers will read. Resolved AuthStrategy /
/// ArchivePolicy / RBAC merges land in Phase 2.
#[derive(Debug, Clone)]
pub struct ResolvedSchema {
    pub path: SchemaPath,
    pub pg_schema: String,
    pub pg_table: String,
    pub pg_qualified: String,
    /// Domain role to `SET LOCAL ROLE` to inside a write transaction.
    /// `<pg_schema>_writer` for create/update, `_admin` for delete.
    pub pg_role_writer: String,
    pub pg_role_admin: String,
    pub pg_role_reader: String,
    /// Spec snapshot. Wrapped in Arc so it can be cheaply shared across
    /// concurrent handler invocations.
    pub spec: Arc<SchemaDefinitionSpec>,
}

impl ResolvedSchema {
    pub fn from_spec(path: SchemaPath, spec: SchemaDefinitionSpec) -> Self {
        let pg_schema = path.pg_schema();
        let pg_table = path.pg_table();
        let pg_qualified = path.pg_qualified_table();
        Self {
            pg_role_writer: format!("{pg_schema}_writer"),
            pg_role_admin: format!("{pg_schema}_admin"),
            pg_role_reader: format!("{pg_schema}_reader"),
            path,
            pg_schema,
            pg_table,
            pg_qualified,
            spec: Arc::new(spec),
        }
    }
}

/// Key used to look up a schema in the registry: the URL path components,
/// joined with `/`. Matches `/api/{org}/{app}/{domain}/{object}/{version}`.
pub fn registry_key(path: &SchemaPath) -> String {
    format!("{}/{}/{}/{}/{}", path.org, path.app, path.domain, path.object, path.version)
}

#[derive(Debug, Default)]
pub struct RegistryInner {
    /// `org/app/domain/object/version` → resolved schema.
    pub by_path: HashMap<String, Arc<ResolvedSchema>>,
}

impl RegistryInner {
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

/// Lock-free registry of resolved schemas.
#[derive(Debug)]
pub struct SchemaRegistry {
    inner: ArcSwap<RegistryInner>,
    ready_tx: watch::Sender<bool>,
}

impl SchemaRegistry {
    pub fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        let registry =
            Arc::new(Self { inner: ArcSwap::from_pointee(RegistryInner::default()), ready_tx: tx });
        (registry, rx)
    }

    /// Atomic pointer load — cheap, lock-free, safe on the hot path.
    pub fn snapshot(&self) -> Arc<RegistryInner> {
        self.inner.load_full()
    }

    pub fn resolve(&self, path: &SchemaPath) -> Option<Arc<ResolvedSchema>> {
        self.snapshot().by_path.get(&registry_key(path)).cloned()
    }

    /// Replace the entire registry contents. Called on informer `Restart` and
    /// on the initial bootstrap.
    pub fn replace_all(&self, schemas: Vec<ResolvedSchema>) {
        let mut by_path = HashMap::with_capacity(schemas.len());
        for s in schemas {
            by_path.insert(registry_key(&s.path), Arc::new(s));
        }
        self.inner.store(Arc::new(RegistryInner { by_path }));
    }

    pub fn upsert(&self, schema: ResolvedSchema) {
        let prev = self.snapshot();
        let mut next = (*prev).clone_inner();
        next.by_path.insert(registry_key(&schema.path), Arc::new(schema));
        self.inner.store(Arc::new(next));
    }

    pub fn remove(&self, path: &SchemaPath) {
        let prev = self.snapshot();
        let mut next = (*prev).clone_inner();
        next.by_path.remove(&registry_key(path));
        self.inner.store(Arc::new(next));
    }

    /// Signal readiness — call once after the informer's first `InitDone`.
    /// Subsequent calls are a no-op.
    pub fn mark_ready(&self) {
        let _ = self.ready_tx.send(true);
    }

    pub fn is_ready(&self) -> bool {
        *self.ready_tx.borrow()
    }
}

impl RegistryInner {
    fn clone_inner(&self) -> Self {
        Self { by_path: self.by_path.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, ObservabilitySpec, SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    fn spec() -> SchemaDefinitionSpec {
        SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "acme-platform".into(),
                },
                overrides: Vec::new(),
            },
            access: AccessSpec::default(),
            fields: Vec::new(),
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        }
    }

    fn make(org: &str) -> ResolvedSchema {
        let p = SchemaPath::new(org, "supply-chain", "procurement", "purchase-order", "v1");
        ResolvedSchema::from_spec(p, spec())
    }

    #[test]
    fn upsert_then_resolve() {
        let (r, _) = SchemaRegistry::new();
        r.upsert(make("acme"));
        let p = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let got = r.resolve(&p).unwrap();
        assert_eq!(got.pg_qualified, "acme_supply_chain_procurement.purchase_order_v1");
        assert_eq!(got.pg_role_reader, "acme_supply_chain_procurement_reader");
    }

    #[test]
    fn replace_all_clears_previous() {
        let (r, _) = SchemaRegistry::new();
        r.upsert(make("acme"));
        r.replace_all(vec![make("globex")]);
        let acme = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let globex =
            SchemaPath::new("globex", "supply-chain", "procurement", "purchase-order", "v1");
        assert!(r.resolve(&acme).is_none());
        assert!(r.resolve(&globex).is_some());
    }

    #[test]
    fn remove_drops_entry() {
        let (r, _) = SchemaRegistry::new();
        r.upsert(make("acme"));
        let p = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        r.remove(&p);
        assert!(r.resolve(&p).is_none());
    }

    #[test]
    fn ready_flag_starts_false_and_flips() {
        let (r, rx) = SchemaRegistry::new();
        assert!(!*rx.borrow());
        assert!(!r.is_ready());
        r.mark_ready();
        assert!(*rx.borrow());
        assert!(r.is_ready());
    }
}
