//! Reconciler for `velocity.sh/v1/RoleBinding`.
//!
//! Two outputs per reconcile:
//!
//! 1. Upsert the binding into `platform.role_bindings`. The DB row is the
//!    durable record; everything downstream — audit, debug queries,
//!    cross-region replication — reads from here, not from etcd.
//! 2. Push a revocation signal to Redis when the binding has expired (or
//!    been deleted, via finalizer). The API's revocation checker reads the
//!    same key, so a refresh of the set takes effect on the next request.
//!
//! ## Revocation semantics
//!
//! - `spec.expiresAt` in the past → revoked. The DB row carries
//!   `revoked_at`, the Redis set carries the actor id.
//! - `spec.expiresAt` absent or in the future → active. Any prior
//!   revocation for *this* actor is cleared from Redis only if no other
//!   active binding has them flagged. We approximate this with a single
//!   SREM under the assumption Phase 2a installs one binding per actor;
//!   the multi-binding case is a Phase 2b refinement (see comment in
//!   [`should_clear_revoke`]).
//!
//! ## Finalizer
//!
//! Deletion of the CRD is a revocation event. We add a finalizer so the
//! reconciler runs once on the delete path, lets us write the audit-bearing
//! Redis SADD, and *then* removes the finalizer to release the object.

use std::sync::Arc;

use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use serde_json::json;
use velocity_types::crds::{ReconcilePhase, RoleBinding};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};

/// Finalizer string we attach so we can run the Redis SADD on delete.
/// Kubernetes will block the resource's actual deletion until we patch
/// the finalizer off.
pub const FINALIZER: &str = "velocity.sh/rolebinding-revoke";

const MANAGER: &str = "velocity-operator";

pub async fn reconcile(obj: Arc<RoleBinding>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj
        .namespace()
        .ok_or_else(|| ReconcileError::Invalid(format!("RoleBinding {name} has no namespace")))?;

    tracing::info!(
        %name, %namespace, actor = %obj.spec.actor_id, roles = ?obj.spec.roles,
        "reconciling RoleBinding"
    );

    let api: Api<RoleBinding> = Api::namespaced(ctx.kube.clone(), &namespace);

    // ── Delete path: finalizer present + deletionTimestamp set ───────────
    if obj.meta().deletion_timestamp.is_some() {
        // Tombstone the DB row (revoke), poke Redis, then drop the finalizer.
        revoke_in_db(&ctx, &namespace, &name, &obj.spec.actor_id).await?;
        notify_revoke(&ctx, &obj.spec.actor_id).await;
        remove_finalizer(&api, &name).await?;
        tracing::info!(%name, %namespace, "RoleBinding finalised — actor revoked");
        return Ok(Action::await_change());
    }

    // ── Apply path ───────────────────────────────────────────────────────
    if !has_finalizer(&obj) {
        add_finalizer(&api, &name).await?;
        // The patch above triggers another reconcile via the watcher; bail
        // here rather than racing the next event.
        return Ok(Action::await_change());
    }

    let now = chrono::Utc::now();
    let expired = parse_expiry(obj.spec.expires_at.as_deref()).map(|t| t <= now).unwrap_or(false);

    upsert_db_row(&ctx, &namespace, &name, &obj, expired).await?;

    if expired {
        notify_revoke(&ctx, &obj.spec.actor_id).await;
    } else if should_clear_revoke(&obj) {
        notify_unrevoke(&ctx, &obj.spec.actor_id).await;
    }

    // Status patch — phase + revoked flag mirror what we wrote to Postgres.
    let status_patch = json!({
        "status": {
            "phase": ReconcilePhase::Ready,
            "revoked": expired,
        }
    });
    api.patch_status(&name, &PatchParams::apply(MANAGER), &Patch::Merge(&status_patch)).await?;

    // If we set an expiry, wake up at the boundary so the revoke fires on
    // time without an extra poke from the user.
    let requeue = parse_expiry(obj.spec.expires_at.as_deref())
        .and_then(|t| (t - now).to_std().ok())
        .map(|d| d.min(std::time::Duration::from_secs(600)))
        .unwrap_or(std::time::Duration::from_secs(300));
    Ok(Action::requeue(requeue))
}

pub fn error_policy(_obj: Arc<RoleBinding>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "RoleBinding reconcile failed");
    error_action(err)
}

// ─── DB writes ─────────────────────────────────────────────────────────────

async fn upsert_db_row(
    ctx: &Context,
    namespace: &str,
    name: &str,
    obj: &RoleBinding,
    expired: bool,
) -> Result<(), ReconcileError> {
    let roles_json = serde_json::to_value(&obj.spec.roles)
        .map_err(|e| ReconcileError::Invalid(format!("roles -> json: {e}")))?;
    let scopes_json = serde_json::to_value(&obj.spec.scopes)
        .map_err(|e| ReconcileError::Invalid(format!("scopes -> json: {e}")))?;
    let expires_at = parse_expiry(obj.spec.expires_at.as_deref());
    let revoked_at = expired.then(chrono::Utc::now);

    sqlx::query(
        r#"
        INSERT INTO platform.role_bindings
            (name, namespace, actor_id, roles, scopes, granted_by, expires_at, revoked_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (namespace, name) DO UPDATE SET
            actor_id   = EXCLUDED.actor_id,
            roles      = EXCLUDED.roles,
            scopes     = EXCLUDED.scopes,
            granted_by = EXCLUDED.granted_by,
            expires_at = EXCLUDED.expires_at,
            revoked_at = EXCLUDED.revoked_at
        "#,
    )
    .bind(name)
    .bind(namespace)
    .bind(&obj.spec.actor_id)
    .bind(roles_json)
    .bind(scopes_json)
    .bind(obj.spec.granted_by.as_deref())
    .bind(expires_at)
    .bind(revoked_at)
    .execute(&ctx.pg)
    .await
    .map_err(|e| ReconcileError::Invalid(format!("upsert role_binding: {e}")))?;
    Ok(())
}

async fn revoke_in_db(
    ctx: &Context,
    namespace: &str,
    name: &str,
    actor_id: &str,
) -> Result<(), ReconcileError> {
    sqlx::query(
        "UPDATE platform.role_bindings
         SET revoked_at = COALESCE(revoked_at, now())
         WHERE namespace = $1 AND name = $2 AND actor_id = $3",
    )
    .bind(namespace)
    .bind(name)
    .bind(actor_id)
    .execute(&ctx.pg)
    .await
    .map_err(|e| ReconcileError::Invalid(format!("tombstone role_binding: {e}")))?;
    Ok(())
}

// ─── Redis notifications ───────────────────────────────────────────────────

/// Best-effort SADD. A Redis outage must not block reconcile — log loudly
/// and let the next reconcile (auto-requeue) retry.
async fn notify_revoke(ctx: &Context, actor_id: &str) {
    let Some(redis) = ctx.redis.as_ref() else {
        tracing::debug!(actor = %actor_id, "no Redis publisher configured — skipping SADD");
        return;
    };
    if let Err(e) = redis.revoke(actor_id).await {
        tracing::error!(
            actor = %actor_id,
            error = %e,
            "failed to publish revocation to Redis — DB row written, will retry on next reconcile",
        );
    } else {
        tracing::info!(actor = %actor_id, "actor revocation published to Redis");
    }
}

/// Best-effort SREM. We only call this on the *apply* path, where the
/// binding's expiry is clear (not past). See [`should_clear_revoke`].
async fn notify_unrevoke(ctx: &Context, actor_id: &str) {
    let Some(redis) = ctx.redis.as_ref() else {
        return;
    };
    if let Err(e) = redis.unrevoke(actor_id).await {
        tracing::error!(
            actor = %actor_id,
            error = %e,
            "failed to clear revocation in Redis — will retry on next reconcile",
        );
    }
}

/// Heuristic: should the apply path SREM this actor from the revoked set?
///
/// Phase 2a assumption — at most one RoleBinding per actor per namespace.
/// Under that assumption a non-expired apply means the actor is intended
/// active, so clearing the revoked set is correct.
///
/// Phase 2b — once multiple bindings per actor are real, this becomes
/// "no other bindings for `actor_id` have `revoked_at IS NOT NULL` AND
/// no other bindings have an expired `expires_at`." The check lives at the
/// SQL layer (one query, returns a bool) and is cheap.
fn should_clear_revoke(_obj: &RoleBinding) -> bool {
    true
}

// ─── Finalizer plumbing ────────────────────────────────────────────────────

fn has_finalizer(obj: &RoleBinding) -> bool {
    obj.meta().finalizers.as_ref().map(|fs| fs.iter().any(|s| s == FINALIZER)).unwrap_or(false)
}

async fn add_finalizer(api: &Api<RoleBinding>, name: &str) -> Result<(), ReconcileError> {
    // JSON-merge-patch: append our finalizer. We read-modify-write the list
    // because a strategic-merge-patch on `finalizers` would replace, not
    // append, and Kubernetes' JSON patch protocol gives no portable way to
    // express "append unique".
    let current = api.get(name).await?;
    let mut finalizers = current.meta().finalizers.clone().unwrap_or_default();
    if !finalizers.iter().any(|s| s == FINALIZER) {
        finalizers.push(FINALIZER.into());
    }
    let patch = json!({ "metadata": { "finalizers": finalizers } });
    api.patch(name, &PatchParams::apply(MANAGER).force(), &Patch::Apply(&patch)).await?;
    Ok(())
}

async fn remove_finalizer(api: &Api<RoleBinding>, name: &str) -> Result<(), ReconcileError> {
    let current = api.get(name).await?;
    let finalizers: Vec<String> = current
        .meta()
        .finalizers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s != FINALIZER)
        .collect();
    let patch = json!({ "metadata": { "finalizers": finalizers } });
    api.patch(name, &PatchParams::apply(MANAGER).force(), &Patch::Apply(&patch)).await?;
    Ok(())
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Parse an RFC3339 timestamp coming off the CRD. We tolerate `None` and
/// malformed values (logged, treated as "no expiry"); a malformed value is
/// far less dangerous than a panicking reconciler.
fn parse_expiry(s: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s?;
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(t) => Some(t.with_timezone(&chrono::Utc)),
        Err(e) => {
            tracing::warn!(value = %s, error = %e, "could not parse RoleBinding expiresAt — treating as no expiry");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_expiry_none() {
        assert!(parse_expiry(None).is_none());
    }

    #[test]
    fn parse_expiry_valid_rfc3339() {
        let t = parse_expiry(Some("2030-01-01T00:00:00Z")).unwrap();
        assert_eq!(t.to_rfc3339(), "2030-01-01T00:00:00+00:00");
    }

    #[test]
    fn parse_expiry_garbage_returns_none() {
        // Surviving garbage is load-bearing: a typo'd expiry would
        // otherwise crash the reconciler on every wakeup.
        assert!(parse_expiry(Some("definitely-not-a-date")).is_none());
    }

    #[test]
    fn should_clear_revoke_phase_2a_default_true() {
        // Pinned so the Phase 2b refinement (multi-binding semantics) has
        // to update both the function and this test.
        let spec = velocity_types::crds::auth::RoleBindingSpec {
            actor_id: "alice".into(),
            roles: vec!["reader".into()],
            scopes: Vec::new(),
            expires_at: None,
            granted_by: None,
        };
        let obj = RoleBinding::new("rb-alice", spec);
        assert!(should_clear_revoke(&obj));
    }
}
