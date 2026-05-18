//! Reconciler for `velocity.sh/v1/SchemaDefinition`.
//!
//! Phase 1 scope: given a SchemaDefinition CRD, build the [`DdlPlan`] and apply
//! it via the provisioner — creating the main, history, and (Tier-3) outbox
//! tables along with auto-generated indexes and triggers.
//!
//! Breaking schema changes (DropColumn / ChangeType / etc.) are rejected
//! unless the CRD carries `velocity.sh/breaking-change: approved` in its
//! annotations (CLAUDE.md › Blocking breaking changes).

use std::sync::Arc;

use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::SearchTier;
use velocity_types::crds::{ReconcilePhase, SchemaDefinition};
use velocity_typesense::{
    concrete_collection_spec, schema_collection_name, schema_concrete_collection_name,
};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};
use crate::ddl_builder::build_ddl;
use crate::search_rebuild::{self, RebuildArgs};

const BREAKING_CHANGE_ANN: &str = "velocity.sh/breaking-change";

pub async fn reconcile(
    obj: Arc<SchemaDefinition>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj.namespace().ok_or_else(|| {
        ReconcileError::Invalid(format!("SchemaDefinition {name} has no namespace"))
    })?;

    let org = obj.labels().get("velocity.sh/org").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!(
            "SchemaDefinition {namespace}/{name} missing velocity.sh/org label"
        ))
    })?;
    let app = obj.labels().get("velocity.sh/app").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!(
            "SchemaDefinition {namespace}/{name} missing velocity.sh/app label"
        ))
    })?;
    let domain = obj.labels().get("velocity.sh/domain").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!(
            "SchemaDefinition {namespace}/{name} missing velocity.sh/domain label"
        ))
    })?;

    let path = SchemaPath::new(&org, &app, &domain, &name, &obj.spec.version);
    let allow_breaking = obj
        .annotations()
        .get(BREAKING_CHANGE_ANN)
        .is_some_and(|v| v.eq_ignore_ascii_case("approved"));

    tracing::info!(
        %org, %app, %domain, object = %name, version = %obj.spec.version, %namespace,
        allow_breaking,
        "reconciling SchemaDefinition"
    );

    // Skip-if-unchanged. The hash includes the breaking-change annotation so
    // that toggling it forces a re-evaluation.
    let hash = hash_spec(&obj, allow_breaking);
    let uid = obj.uid().unwrap_or_default();
    if let Some(prev) = ctx.last_hash.get(&uid) {
        if *prev == hash {
            tracing::debug!(uid, "no-op reconcile (hash unchanged)");
            return Ok(Action::requeue(std::time::Duration::from_secs(300)));
        }
    }

    let plan = build_ddl(&obj.spec, &path).map_err(|e| ReconcileError::Invalid(e.to_string()))?;
    let provisioned = ctx.provisioner.sync_schema_tables(&plan, allow_breaking).await?;

    // Phase 5d-2 + 5d-3a: eagerly provision the Typesense collection
    // for Tier-3 schemas. The concrete collection name carries a
    // content-hash suffix; an alias under the stable name routes
    // reads/writes to it. On re-reconcile after a spec change we
    // create the new concrete collection but **leave the alias
    // alone** — the explicit swap (with backfill) is Phase 5d-3b.
    //
    // If `ctx.typesense` is `None`, the operator was started without
    // a Typesense URL; the API's CDC worker handles lazy creation as
    // a backstop. If the client is present and Typesense errors,
    // the reconcile fails — kube-runtime requeues.
    //
    // `rebuild_spawned` tracks whether we handed `phase` ownership to
    // a background rebuild task. If we did, the reconciler MUST NOT
    // patch `phase` itself — the task sets `Rebuilding` and later
    // `Ready`, and a race here would clobber that state.
    let mut rebuild_spawned = false;
    // `active_revision` — the concrete the alias points at when the
    // reconciler finishes. While a rebuild is in flight this is still
    // the *source* (pre-flip); the rebuild task overwrites this field
    // with the new target on successful flip.
    let mut active_revision: Option<String> = None;
    if matches!(obj.spec.search.tier, SearchTier::Tier3) {
        if let Some(ts) = ctx.typesense.as_ref() {
            let alias = schema_collection_name(&path);
            let concrete = schema_concrete_collection_name(&path, &obj.spec);
            let spec = concrete_collection_spec(&path, &obj.spec);
            tracing::info!(
                alias = %alias,
                concrete = %concrete,
                schema = %path,
                "ensuring Typesense concrete collection"
            );
            ts.create_collection(&spec).await?;
            // Phase 5d-3b: branch on alias state.
            //   - alias missing                 → first-time bind (no rebuild needed)
            //   - alias points at `concrete`    → nothing to do
            //   - alias points at older target  → spawn blue-green rebuild
            match ts.get_alias(&alias).await? {
                None => {
                    tracing::info!(alias = %alias, concrete = %concrete, "binding new Typesense alias");
                    ts.upsert_alias(&alias, &concrete).await?;
                    active_revision = Some(concrete.clone());
                }
                Some(current) if current == concrete => {
                    tracing::debug!(alias = %alias, "alias already correct; no rebuild");
                    active_revision = Some(current);
                }
                Some(source) => {
                    active_revision = Some(source.clone());
                    if ctx.rebuilds.supersede(&uid, &concrete) {
                        spawn_rebuild(
                            ctx.clone(),
                            &uid,
                            &namespace,
                            &name,
                            &path,
                            &provisioned.qualified,
                            alias.clone(),
                            source,
                            concrete.clone(),
                            ts.clone(),
                        );
                        rebuild_spawned = true;
                    } else {
                        tracing::debug!(
                            alias = %alias,
                            concrete = %concrete,
                            "rebuild already in flight for this target"
                        );
                        // A rebuild for this exact target is already running;
                        // it owns the `phase` field. Don't clobber it.
                        rebuild_spawned = true;
                    }
                }
            }
        } else {
            tracing::warn!(
                schema = %path,
                "Tier-3 schema reconciled without VELOCITY_OPERATOR_TYPESENSE_URL — relying on CDC lazy collection creation"
            );
        }
    }

    let api: Api<SchemaDefinition> = Api::namespaced(ctx.kube.clone(), &namespace);
    // Skip `phase` when a rebuild task owns it (see comment on
    // `rebuild_spawned`). The other fields are safe to write — they
    // reflect the PG/spec state that just succeeded, independent of
    // the search-tier convergence the task is driving.
    let mut status_fields = serde_json::Map::new();
    status_fields.insert("pgTable".into(), json!(provisioned.qualified));
    status_fields.insert("policyHash".into(), json!(hash));
    status_fields.insert("provisionedAt".into(), json!(chrono::Utc::now().to_rfc3339()));
    if !rebuild_spawned {
        status_fields.insert("phase".into(), json!(ReconcilePhase::Ready));
    }
    if let Some(rev) = &active_revision {
        status_fields.insert("activeRevision".into(), json!(rev));
    }
    let status_patch = json!({ "status": serde_json::Value::Object(status_fields) });
    api.patch_status(&name, &PatchParams::apply("velocity-operator"), &Patch::Merge(&status_patch))
        .await?;

    ctx.last_hash.insert(uid, hash);
    tracing::info!(object = %name, qualified = %provisioned.qualified, "SchemaDefinition ready");

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

pub fn error_policy(
    _obj: Arc<SchemaDefinition>,
    err: &ReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    tracing::warn!(error = %err, "SchemaDefinition reconcile failed");
    error_action(err)
}

/// Spawn the Phase 5d-3b blue-green rebuild task. Detached — the
/// reconcile completes immediately and search continues serving from
/// the old concrete. The task itself patches status as it progresses.
#[allow(clippy::too_many_arguments)]
fn spawn_rebuild(
    ctx: Arc<Context>,
    uid: &str,
    namespace: &str,
    crd_name: &str,
    path: &SchemaPath,
    qualified: &str,
    alias: String,
    source_concrete: String,
    target_concrete: String,
    typesense: velocity_typesense::TypesenseClient,
) {
    let (pg_schema, pg_table) = match qualified.split_once('.') {
        Some((s, t)) => (s.trim_matches('"').to_string(), t.trim_matches('"').to_string()),
        None => {
            tracing::error!(qualified, "spawn_rebuild: cannot parse qualified table name");
            return;
        }
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let uid_owned = uid.to_string();
    let target_for_registry = target_concrete.clone();
    let rebuilds = ctx.rebuilds.clone();
    let last_hash = ctx.last_hash.clone();
    let args = RebuildArgs {
        kube: ctx.kube.clone(),
        pool: ctx.pg.clone(),
        typesense,
        namespace: namespace.to_string(),
        crd_name: crd_name.to_string(),
        path: path.clone(),
        pg_schema,
        pg_table,
        alias: alias.clone(),
        source_concrete,
        target_concrete: target_concrete.clone(),
        cancel: cancel.clone(),
    };
    let uid_for_task = uid_owned.clone();
    let target_for_task = target_concrete.clone();
    let join = tokio::spawn(async move {
        match search_rebuild::run(args).await {
            Ok(n) => tracing::info!(
                alias = %alias,
                target = %target_concrete,
                rows = n,
                "search rebuild complete"
            ),
            Err(e) => {
                tracing::error!(
                    alias = %alias,
                    target = %target_concrete,
                    error = %e,
                    "search rebuild failed; next reconcile will retry"
                );
                // Drop the in-memory reconcile-skip hash so the very
                // next reconcile for this CRD doesn't short-circuit
                // on "spec unchanged". Without this, an in-process
                // failure leaves the alias pointing at the stale
                // concrete and the reconciler refuses to re-evaluate
                // until a real spec edit comes in.
                last_hash.remove(&uid_for_task);
            }
        }
        // Compare-and-swap on target: only clear our own registry
        // entry. A superseding spawn will have replaced us under the
        // same uid with a different target, and clobbering it would
        // race a third reconcile into spawning a duplicate.
        rebuilds.forget_if(&uid_for_task, &target_for_task);
    });
    ctx.rebuilds.record(uid_owned, target_for_registry, cancel, join);
}

/// Stable hash over spec + the bits of metadata that affect reconcile output.
/// `serde_json::to_vec` is deterministic on our types (no maps with unstable
/// iteration order on the hot path — BTreeMap is used everywhere).
fn hash_spec(obj: &SchemaDefinition, allow_breaking: bool) -> String {
    let mut h = Sha256::new();
    if let Ok(bytes) = serde_json::to_vec(&obj.spec) {
        h.update(bytes);
    }
    h.update([u8::from(allow_breaking)]);
    format!("{:x}", h.finalize())
}
