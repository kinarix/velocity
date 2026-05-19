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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::FieldSpec;
use velocity_types::crds::SchemaDefinitionSpec;

/// Pre-computed views over `SchemaDefinitionSpec.fields` so the request hot
/// path never iterates the Vec. Built once at resolve time; cheap to clone
/// because the contents are wrapped in `Arc`.
///
/// Only **user-declared** fields are in here. System columns (`id`,
/// `created_at`, `updated_at`, `deleted_at`, `version`, `created_by`,
/// `updated_by`) are handled by the SQL builders directly and are not part of
/// the user-facing schema surface.
#[derive(Debug, Clone, Default)]
pub struct FieldIndex {
    /// O(1) `name -> spec` lookup, keyed by field name as declared in the CRD.
    pub by_name: HashMap<String, Arc<FieldSpec>>,
    /// Names of fields with `filterable: true` — used by `QueryBuilder` to
    /// gate `WHERE` clauses.
    pub filterable: HashSet<String>,
    /// Names of fields with `sortable: true` — used to gate `ORDER BY`.
    pub sortable: HashSet<String>,
    /// Field list in spec order, for introspection responses.
    pub ordered: Vec<Arc<FieldSpec>>,
}

impl FieldIndex {
    pub(crate) fn from_spec(spec: &SchemaDefinitionSpec) -> Self {
        let mut by_name = HashMap::with_capacity(spec.fields.len());
        let mut filterable = HashSet::new();
        let mut sortable = HashSet::new();
        let mut ordered = Vec::with_capacity(spec.fields.len());
        for f in &spec.fields {
            let arc = Arc::new(f.clone());
            if f.filterable {
                filterable.insert(f.name.clone());
            }
            if f.sortable {
                sortable.insert(f.name.clone());
            }
            by_name.insert(f.name.clone(), arc.clone());
            ordered.push(arc);
        }
        Self { by_name, filterable, sortable, ordered }
    }
}

/// Precomputed RBAC index — `operation -> set of roles that grant it`.
///
/// Built once at `ResolvedSchema::from_spec` time so the request hot path
/// is a single `HashSet` lookup, same discipline as [`FieldIndex`].
///
/// `is_open` carries the load-bearing rule: a schema with no
/// `access.roles` declared at all is treated as world-readable/writable.
/// Flipping that default to "deny" would silently lock out every Phase 1
/// schema; we make the choice explicit so a future change has to update
/// this field (and the test that pins it).
#[derive(Debug, Clone, Default)]
pub struct AccessIndex {
    by_op: HashMap<String, HashSet<String>>,
    pub is_open: bool,
}

impl AccessIndex {
    fn from_spec(spec: &SchemaDefinitionSpec) -> Self {
        let mut by_op: HashMap<String, HashSet<String>> = HashMap::new();
        for entry in &spec.access.roles {
            for op in &entry.operations {
                // Operation strings on the wire are canonical lowercase per
                // CLAUDE.md › Metric label cardinality. We normalise on
                // ingest so a CRD typo (`Read`) doesn't accidentally bypass
                // the check — it just won't match anything.
                by_op.entry(op.to_lowercase()).or_default().insert(entry.role.clone());
            }
        }
        Self { is_open: spec.access.roles.is_empty(), by_op }
    }

    /// Returns `true` iff any role in `identity_roles` grants `op` on this
    /// schema. Open schemas (no `access.roles` declared) always return `true`.
    pub fn allows(&self, op: &str, identity_roles: &[String]) -> bool {
        if self.is_open {
            return true;
        }
        let Some(granted) = self.by_op.get(op) else {
            return false;
        };
        identity_roles.iter().any(|r| granted.contains(r))
    }

    /// Number of distinct operations covered by the spec. Used in tests and
    /// in startup logging so an operator can spot a schema that locks out
    /// every operation by accident.
    pub fn operations_count(&self) -> usize {
        self.by_op.len()
    }
}

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
    /// Pre-computed user-field index. See [`FieldIndex`].
    pub fields: Arc<FieldIndex>,
    /// Pre-computed RBAC index over `spec.access.roles`. See [`AccessIndex`].
    pub access: Arc<AccessIndex>,
    /// CEL / compare rules from `spec.validations`, compiled at resolve
    /// time so the request hot path is allocation- and parser-free. See
    /// [`crate::validate`].
    pub compiled_validations: Arc<Vec<crate::validate::CompiledRule>>,
    /// Layer-2 ABAC policies from `spec.access.policies`, compiled the
    /// same way as `compiled_validations`. See [`crate::policy`].
    pub compiled_policies: Arc<Vec<crate::policy::CompiledPolicy>>,
    /// Layer-4 row-filter index from `spec.access.rowFilter`. See
    /// [`crate::row_filter`].
    pub row_filter: Arc<crate::row_filter::RowFilterIndex>,
    /// Layer-5 per-field read/write index built from `FieldSpec.access`.
    /// See [`crate::field_filter`].
    pub field_filter: Arc<crate::field_filter::FieldFilterIndex>,
    /// Layer-6 masking index built from `FieldSpec.mask`. Applied on
    /// every read response *after* the Layer-5 strip has already run.
    /// See [`crate::masking`].
    pub masking: Arc<crate::masking::MaskingIndex>,
}

impl ResolvedSchema {
    pub fn from_spec(path: SchemaPath, spec: SchemaDefinitionSpec) -> Self {
        let pg_schema = path.pg_schema();
        let pg_table = path.pg_table();
        let pg_qualified = path.pg_qualified_table();
        let fields = Arc::new(FieldIndex::from_spec(&spec));
        let access = Arc::new(AccessIndex::from_spec(&spec));
        let compiled_validations = Arc::new(crate::validate::compile_rules(&spec.validations));
        let compiled_policies = Arc::new(crate::policy::compile_policies(&spec.access.policies));
        let row_filter = Arc::new(crate::row_filter::RowFilterIndex::from_spec(&spec, &fields));
        let field_filter = Arc::new(crate::field_filter::FieldFilterIndex::from_spec(&spec));
        let masking = Arc::new(crate::masking::MaskingIndex::from_spec(&spec));
        Self {
            pg_role_writer: format!("{pg_schema}_writer"),
            pg_role_admin: format!("{pg_schema}_admin"),
            pg_role_reader: format!("{pg_schema}_reader"),
            path,
            pg_schema,
            pg_table,
            pg_qualified,
            spec: Arc::new(spec),
            fields,
            access,
            compiled_validations,
            compiled_policies,
            row_filter,
            field_filter,
            masking,
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
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess,
        SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    fn spec() -> SchemaDefinitionSpec {
        spec_with_fields(Vec::new())
    }

    fn spec_with_fields(fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
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
            fields,
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        }
    }

    fn field(name: &str, filterable: bool, sortable: bool) -> FieldSpec {
        let mut f: FieldSpec = serde_json::from_value(serde_json::json!({
            "name": name,
            "type": "string",
        }))
        .unwrap();
        f.kind = FieldKind::String;
        f.filterable = filterable;
        f.sortable = sortable;
        f
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
    fn registry_inner_len_and_is_empty_track_upserts() {
        let (r, _) = SchemaRegistry::new();
        assert!(r.snapshot().is_empty());
        assert_eq!(r.snapshot().len(), 0);
        r.upsert(make("acme"));
        let snap = r.snapshot();
        assert!(!snap.is_empty());
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn field_index_separates_filterable_and_sortable() {
        let spec = spec_with_fields(vec![
            field("po_number", true, true),
            field("notes", false, false),
            field("supplier_code", true, false),
        ]);
        let idx = FieldIndex::from_spec(&spec);
        assert!(idx.by_name.contains_key("po_number"));
        assert!(idx.by_name.contains_key("notes"));
        assert!(idx.filterable.contains("po_number"));
        assert!(idx.filterable.contains("supplier_code"));
        assert!(!idx.filterable.contains("notes"));
        assert!(idx.sortable.contains("po_number"));
        assert!(!idx.sortable.contains("supplier_code"));
        assert_eq!(idx.ordered.len(), 3);
        assert_eq!(idx.ordered[0].name, "po_number");
    }

    #[test]
    fn resolved_schema_exposes_field_index() {
        let spec = spec_with_fields(vec![field("po_number", true, true)]);
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let rs = ResolvedSchema::from_spec(path, spec);
        assert!(rs.fields.filterable.contains("po_number"));
    }

    fn spec_with_access(roles: Vec<RoleAccess>) -> SchemaDefinitionSpec {
        let mut s = spec();
        s.access = AccessSpec { roles, ..AccessSpec::default() };
        s
    }

    fn role(name: &str, ops: &[&str]) -> RoleAccess {
        RoleAccess { role: name.into(), operations: ops.iter().map(|s| (*s).into()).collect() }
    }

    #[test]
    fn access_index_empty_roles_is_open() {
        // `access.roles == []` means the schema declared no RBAC — the index
        // returns `allows = true` for every op. CLAUDE.md's metric-cardinality
        // ops list pins what callers may legitimately ask about, but the
        // open-schema rule has to hold even for unknown ops to remain useful
        // during Phase 1 / migration.
        let idx = AccessIndex::from_spec(&spec_with_access(vec![]));
        assert!(idx.is_open);
        assert!(idx.allows("read", &[]));
        assert!(idx.allows("create", &["whatever".into()]));
        assert_eq!(idx.operations_count(), 0);
    }

    #[test]
    fn access_index_non_empty_roles_denies_anonymous() {
        // Anonymous identity carries no roles. A schema that declares
        // *any* RBAC must reject it — this is the load-bearing inverse of
        // the open-schema rule, and a foot-gun if we ever flip a default.
        let idx = AccessIndex::from_spec(&spec_with_access(vec![role("reader", &["read"])]));
        assert!(!idx.is_open);
        assert!(!idx.allows("read", &[]));
    }

    #[test]
    fn access_index_role_match_admits() {
        let idx = AccessIndex::from_spec(&spec_with_access(vec![
            role("reader", &["read"]),
            role("writer", &["read", "create", "update"]),
        ]));
        assert!(idx.allows("read", &["reader".into()]));
        assert!(idx.allows("create", &["writer".into()]));
        assert!(idx.allows("read", &["writer".into()]));
    }

    #[test]
    fn access_index_role_mismatch_denies() {
        let idx = AccessIndex::from_spec(&spec_with_access(vec![role("reader", &["read"])]));
        // Caller has a role, but not one that grants `create`.
        assert!(!idx.allows("create", &["reader".into()]));
        // Caller has roles, none of which are declared on this schema.
        assert!(!idx.allows("read", &["stranger".into(), "other".into()]));
    }

    #[test]
    fn access_index_normalises_op_case_on_ingest() {
        // CRDs are hand-written YAML — a typo like `Read` shouldn't
        // accidentally bypass the check by failing to match the canonical
        // lowercase op. We lowercase on ingest; callers always pass canonical.
        let idx = AccessIndex::from_spec(&spec_with_access(vec![RoleAccess {
            role: "reader".into(),
            operations: vec!["Read".into(), "CREATE".into()],
        }]));
        assert!(idx.allows("read", &["reader".into()]));
        assert!(idx.allows("create", &["reader".into()]));
        assert!(!idx.allows("READ", &["reader".into()])); // callers send canonical
    }

    #[test]
    fn resolved_schema_exposes_access_index() {
        let s = spec_with_access(vec![role("reader", &["read"])]);
        let path = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        let rs = ResolvedSchema::from_spec(path, s);
        assert!(!rs.access.is_open);
        assert!(rs.access.allows("read", &["reader".into()]));
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
