//! Reconciler for `velocity.sh/v1/ArchivePolicy`.
//!
//! Phase 8 slice 1 scope: **validation only**. The reconcile loop walks the
//! `ArchivePolicySpec`, checks the shape of `schedule`, `trigger`, and
//! `destination`, and reports the result as status conditions + a phase.
//! The actual archival worker, cold-tier provisioning, and S3 export land
//! in subsequent slices — for now the operator's job is to tell the user
//! whether the policy they applied is well-formed.
//!
//! Phase reporting:
//!
//! - `Ready`  — spec passes every check.
//! - `Failed` — at least one check failed. The conditions list spells out
//!   which fields are wrong and why; the actor fixing the policy reads
//!   them via `kubectl describe archivepolicy ...`.
//!
//! No DB writes, no Kafka, no Redis. Pure validation + status patch.

use std::sync::Arc;

use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::common::SchemaPath;
use velocity_types::crds::{
    ArchivePolicy, ArchivePolicySpec, Condition, ReconcilePhase, SchemaDefinition,
};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};
use crate::ddl_builder::build_ddl;

const MANAGER: &str = "velocity-operator";

/// Field operators accepted for a `field` trigger.
const FIELD_OPS: &[&str] = &["lt", "le", "gt", "ge", "eq", "ne", "in", "contains"];

/// Backends accepted for `destination`.
const BACKENDS: &[&str] = &["postgres-cold", "s3"];

/// Object-store formats accepted when `destination.backend == "s3"`.
const S3_FORMATS: &[&str] = &["parquet", "jsonl", "csv"];

pub async fn reconcile(
    obj: Arc<ArchivePolicy>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj
        .namespace()
        .ok_or_else(|| ReconcileError::Invalid(format!("ArchivePolicy {name} has no namespace")))?;

    tracing::info!(
        %name, %namespace, trigger = %obj.spec.trigger.kind,
        backend = %obj.spec.destination.backend,
        "reconciling ArchivePolicy"
    );

    let mut conditions = validate_spec(&obj.spec);
    let spec_valid = conditions.iter().all(|c| c.status == "True");

    let mut archive_schema: Option<String> = None;
    let mut archive_roles: Vec<String> = Vec::new();
    let mut mirrored_tables: Vec<String> = Vec::new();

    if spec_valid && obj.spec.destination.backend == "postgres-cold" {
        match resolve_path_parts(&obj) {
            Ok((org, app, domain)) => {
                match ctx.provisioner.sync_archive_schema(&org, &app, &domain).await {
                    Ok(provisioned) => {
                        archive_schema = Some(provisioned.pg_schema.clone());
                        archive_roles = provisioned.pg_roles.clone();
                        conditions.push(check("ArchiveSchemaProvisioned", Ok(())));

                        // Mirror every SchemaDefinition in the namespace into
                        // the archive schema. Order is deterministic (sorted by
                        // name) so the status `mirroredTables` list is stable
                        // across reconciles.
                        match mirror_tables(
                            &ctx,
                            &namespace,
                            &provisioned.pg_schema,
                            &org,
                            &app,
                            &domain,
                        )
                        .await
                        {
                            Ok(tables) => {
                                mirrored_tables = tables;
                                conditions.push(check("ArchiveMirrorsProvisioned", Ok(())));
                            }
                            Err(msg) => {
                                conditions.push(check("ArchiveMirrorsProvisioned", Err(msg)));
                            }
                        }
                    }
                    Err(e) => {
                        conditions.push(check(
                            "ArchiveSchemaProvisioned",
                            Err(format!("archive schema provisioning failed: {e}")),
                        ));
                    }
                }
            }
            Err(msg) => {
                conditions.push(check("ArchiveSchemaProvisioned", Err(msg)));
            }
        }
    }

    let phase = if conditions.iter().all(|c| c.status == "True") {
        ReconcilePhase::Ready
    } else {
        ReconcilePhase::Failed
    };

    let api: Api<ArchivePolicy> = Api::namespaced(ctx.kube.clone(), &namespace);
    let mut status = json!({
        "phase": phase,
        "conditions": conditions,
    });
    if let Some(s) = &archive_schema {
        status["archiveSchema"] = json!(s);
    }
    if !archive_roles.is_empty() {
        status["archiveRoles"] = json!(archive_roles);
    }
    if !mirrored_tables.is_empty() {
        status["mirroredTables"] = json!(mirrored_tables);
    }
    let status_patch = json!({ "status": status });
    api.patch_status(&name, &PatchParams::apply(MANAGER), &Patch::Merge(&status_patch)).await?;

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

/// Resolve `(org, app, domain)` from the ArchivePolicy's labels (same
/// convention as Domain/SchemaDefinition — `velocity.sh/{org,app,domain}`).
fn resolve_path_parts(obj: &ArchivePolicy) -> Result<(String, String, String), String> {
    let labels = obj.labels();
    let org = labels
        .get("velocity.sh/org")
        .ok_or_else(|| "ArchivePolicy missing label velocity.sh/org".to_string())?
        .clone();
    let app = labels
        .get("velocity.sh/app")
        .ok_or_else(|| "ArchivePolicy missing label velocity.sh/app".to_string())?
        .clone();
    let domain = labels
        .get("velocity.sh/domain")
        .ok_or_else(|| "ArchivePolicy missing label velocity.sh/domain".to_string())?
        .clone();
    Ok((org, app, domain))
}

/// List every `SchemaDefinition` in the policy's namespace and provision a
/// mirror table in the archive schema for each. Returns the list of mirror
/// tables actually created/verified, sorted for status stability.
///
/// Failure handling: if a SchemaDefinition's spec can't be built into a DDL
/// plan (e.g. invalid spec) it is logged and skipped — one bad schema
/// shouldn't block mirroring of its siblings. If a mirror CREATE fails the
/// whole reconcile errors via the surfacing condition.
async fn mirror_tables(
    ctx: &Context,
    namespace: &str,
    archive_schema: &str,
    org: &str,
    app: &str,
    domain: &str,
) -> Result<Vec<String>, String> {
    let sd_api: Api<SchemaDefinition> = Api::namespaced(ctx.kube.clone(), namespace);
    let list = sd_api
        .list(&ListParams::default())
        .await
        .map_err(|e| format!("list SchemaDefinitions: {e}"))?;

    let mut out: Vec<String> = Vec::with_capacity(list.items.len());
    for sd in list.items {
        let object = sd.name_any();
        let version = sd.spec.version.clone();
        let path = SchemaPath::new(org, app, domain, object.clone(), version.clone());
        let plan = match build_ddl(&sd.spec, &path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    %object, %version, error = %e,
                    "skipping mirror for invalid SchemaDefinition"
                );
                continue;
            }
        };
        ctx.provisioner
            .sync_archive_mirror_table(archive_schema, &path.pg_table(), &plan.columns)
            .await
            .map_err(|e| format!("mirror {}: {e}", path.pg_table()))?;
        out.push(path.pg_table());
    }
    out.sort();
    Ok(out)
}

pub fn error_policy(_obj: Arc<ArchivePolicy>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "ArchivePolicy reconcile failed");
    error_action(err)
}

// ─── Validation ────────────────────────────────────────────────────────────

/// Pure validator: returns one `Condition` per check, with `status="True"`
/// when the check passed and `status="False"` otherwise.
///
/// The condition list is the source of truth for the reconciled phase —
/// any `False` condition flips the policy to `Failed`.
pub fn validate_spec(spec: &ArchivePolicySpec) -> Vec<Condition> {
    let mut out = Vec::new();
    out.push(check("ScheduleValid", validate_schedule(&spec.schedule)));
    out.push(check("TriggerValid", validate_trigger(spec)));
    out.push(check("DestinationValid", validate_destination(spec)));
    if let Some(pa) = &spec.purge_after {
        out.push(check("PurgeAfterValid", validate_duration(pa)));
    }
    if let Some(md) = &spec.max_duration {
        out.push(check("MaxDurationValid", validate_duration(md)));
    }
    out
}

fn check(kind: &str, res: Result<(), String>) -> Condition {
    match res {
        Ok(()) => Condition {
            kind: kind.into(),
            status: "True".into(),
            reason: Some("Ok".into()),
            message: None,
            last_transition_time: None,
        },
        Err(msg) => Condition {
            kind: kind.into(),
            status: "False".into(),
            reason: Some("Invalid".into()),
            message: Some(msg),
            last_transition_time: None,
        },
    }
}

/// Accepts standard 5-field cron (`m h dom mon dow`) or 6-field cron with
/// seconds. Full parser arrives with the worker — this slice just gates the
/// obvious shape.
fn validate_schedule(s: &str) -> Result<(), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("schedule is empty".into());
    }
    let n = trimmed.split_whitespace().count();
    if !(5..=6).contains(&n) {
        return Err(format!("schedule must have 5 or 6 whitespace-separated fields, got {n}"));
    }
    Ok(())
}

fn validate_trigger(spec: &ArchivePolicySpec) -> Result<(), String> {
    let t = &spec.trigger;
    match t.kind.as_str() {
        "age" => {
            let v = t
                .value
                .as_ref()
                .ok_or_else(|| "age trigger requires value (e.g. \"30d\")".to_string())?;
            let s = v
                .as_str()
                .ok_or_else(|| "age trigger value must be a duration string".to_string())?;
            validate_duration(s)
        }
        "field" => {
            let field =
                t.field.as_ref().ok_or_else(|| "field trigger requires field".to_string())?;
            if field.trim().is_empty() {
                return Err("field trigger field is empty".into());
            }
            let op = t.op.as_ref().ok_or_else(|| "field trigger requires op".to_string())?;
            if !FIELD_OPS.contains(&op.as_str()) {
                return Err(format!("field trigger op {op:?} not in {{{}}}", FIELD_OPS.join(",")));
            }
            if t.value.is_none() {
                return Err("field trigger requires value".into());
            }
            Ok(())
        }
        "tableSize" => {
            let v = t
                .value
                .as_ref()
                .ok_or_else(|| "tableSize trigger requires value (bytes)".to_string())?;
            // accept integer bytes or string like "10GiB" / "500MiB"
            if v.as_u64().is_some() {
                return Ok(());
            }
            if let Some(s) = v.as_str() {
                return validate_byte_size(s);
            }
            Err("tableSize value must be an integer or a size string like \"10GiB\"".into())
        }
        "cel" => {
            let rule = t.rule.as_ref().ok_or_else(|| "cel trigger requires rule".to_string())?;
            if rule.trim().is_empty() {
                return Err("cel trigger rule is empty".into());
            }
            if rule.len() > 10_000 {
                return Err(format!("cel rule is {} bytes; CLAUDE.md caps at 10KB", rule.len()));
            }
            Ok(())
        }
        other => Err(format!("trigger.type {other:?} not in {{age, field, tableSize, cel}}")),
    }
}

fn validate_destination(spec: &ArchivePolicySpec) -> Result<(), String> {
    let d = &spec.destination;
    if !BACKENDS.contains(&d.backend.as_str()) {
        return Err(format!(
            "destination.backend {:?} not in {{{}}}",
            d.backend,
            BACKENDS.join(",")
        ));
    }
    if d.backend == "s3" {
        let bucket =
            d.bucket.as_ref().ok_or_else(|| "s3 destination requires bucket".to_string())?;
        if bucket.trim().is_empty() {
            return Err("s3 destination bucket is empty".into());
        }
        if let Some(fmt) = &d.format {
            if !S3_FORMATS.contains(&fmt.as_str()) {
                return Err(format!(
                    "s3 destination format {fmt:?} not in {{{}}}",
                    S3_FORMATS.join(",")
                ));
            }
        }
    }
    Ok(())
}

/// Accepts `Nu` where `u` is one of `s`, `m`, `h`, `d` and `N > 0`.
/// Wider parsers (humantime, ISO8601) land with the worker.
fn validate_duration(s: &str) -> Result<(), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("duration is empty".into());
    }
    let (num, unit) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("duration {trimmed:?} missing unit (expected s|m|h|d)"))?,
    );
    let n: u64 =
        num.parse().map_err(|_| format!("duration {trimmed:?} has non-numeric magnitude"))?;
    if n == 0 {
        return Err(format!("duration {trimmed:?} must be > 0"));
    }
    if !matches!(unit, "s" | "m" | "h" | "d") {
        return Err(format!("duration {trimmed:?} unit {unit:?} not in {{s,m,h,d}}"));
    }
    Ok(())
}

/// Accepts `N<unit>` where unit is one of `B`, `KiB`, `MiB`, `GiB`, `TiB`.
fn validate_byte_size(s: &str) -> Result<(), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("size is empty".into());
    }
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("size {trimmed:?} missing unit (expected B|KiB|MiB|GiB|TiB)"))?;
    let (num, unit) = trimmed.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("size {trimmed:?} has non-numeric magnitude"))?;
    if n == 0 {
        return Err(format!("size {trimmed:?} must be > 0"));
    }
    if !matches!(unit, "B" | "KiB" | "MiB" | "GiB" | "TiB") {
        return Err(format!("size {trimmed:?} unit {unit:?} not in {{B,KiB,MiB,GiB,TiB}}"));
    }
    Ok(())
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::crds::policies::{ArchiveDestination, ArchiveTrigger};

    fn base() -> ArchivePolicySpec {
        ArchivePolicySpec {
            schedule: "0 2 * * *".into(),
            trigger: ArchiveTrigger {
                kind: "age".into(),
                field: None,
                op: None,
                value: Some(json!("30d")),
                rule: None,
                max_execution_ms: None,
            },
            batch_size: Some(1000),
            max_duration: None,
            destination: ArchiveDestination {
                backend: "postgres-cold".into(),
                bucket: None,
                format: None,
            },
            purge_after: None,
        }
    }

    fn all_true(conds: &[Condition]) -> bool {
        conds.iter().all(|c| c.status == "True")
    }

    #[test]
    fn happy_path_age_postgres_cold() {
        let spec = base();
        let conds = validate_spec(&spec);
        assert!(all_true(&conds), "expected all True: {conds:?}");
    }

    #[test]
    fn schedule_must_have_5_or_6_fields() {
        let mut spec = base();
        spec.schedule = "0 2 *".into();
        let conds = validate_spec(&spec);
        let sched = conds.iter().find(|c| c.kind == "ScheduleValid").unwrap();
        assert_eq!(sched.status, "False");
    }

    #[test]
    fn schedule_empty_rejected() {
        let mut spec = base();
        spec.schedule = "   ".into();
        let conds = validate_spec(&spec);
        let sched = conds.iter().find(|c| c.kind == "ScheduleValid").unwrap();
        assert_eq!(sched.status, "False");
    }

    #[test]
    fn six_field_cron_accepted() {
        let mut spec = base();
        spec.schedule = "*/30 0 2 * * *".into();
        assert!(all_true(&validate_spec(&spec)));
    }

    #[test]
    fn age_trigger_requires_value() {
        let mut spec = base();
        spec.trigger.value = None;
        let conds = validate_spec(&spec);
        let t = conds.iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
        assert!(t.message.as_ref().unwrap().contains("age"));
    }

    #[test]
    fn age_trigger_value_must_be_duration_string() {
        let mut spec = base();
        spec.trigger.value = Some(json!(42));
        let conds = validate_spec(&spec);
        let t = conds.iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
    }

    #[test]
    fn age_zero_duration_rejected() {
        let mut spec = base();
        spec.trigger.value = Some(json!("0d"));
        let conds = validate_spec(&spec);
        let t = conds.iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
    }

    #[test]
    fn field_trigger_requires_field_op_value() {
        let mut spec = base();
        spec.trigger = ArchiveTrigger {
            kind: "field".into(),
            field: Some("status".into()),
            op: Some("eq".into()),
            value: Some(json!("closed")),
            rule: None,
            max_execution_ms: None,
        };
        assert!(all_true(&validate_spec(&spec)));

        spec.trigger.op = Some("regex".into());
        let t = validate_spec(&spec).into_iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
    }

    #[test]
    fn table_size_accepts_int_and_string() {
        let mut spec = base();
        spec.trigger = ArchiveTrigger {
            kind: "tableSize".into(),
            field: None,
            op: None,
            value: Some(json!(10_737_418_240u64)),
            rule: None,
            max_execution_ms: None,
        };
        assert!(all_true(&validate_spec(&spec)));

        spec.trigger.value = Some(json!("10GiB"));
        assert!(all_true(&validate_spec(&spec)));

        spec.trigger.value = Some(json!("10GB"));
        let t = validate_spec(&spec).into_iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
    }

    #[test]
    fn cel_trigger_requires_rule_under_10kb() {
        let mut spec = base();
        spec.trigger = ArchiveTrigger {
            kind: "cel".into(),
            field: None,
            op: None,
            value: None,
            rule: Some("self.status == 'archived'".into()),
            max_execution_ms: Some(10),
        };
        assert!(all_true(&validate_spec(&spec)));

        spec.trigger.rule = Some("x".repeat(10_001));
        let t = validate_spec(&spec).into_iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
    }

    #[test]
    fn unknown_trigger_kind_rejected() {
        let mut spec = base();
        spec.trigger.kind = "manual".into();
        let t = validate_spec(&spec).into_iter().find(|c| c.kind == "TriggerValid").unwrap();
        assert_eq!(t.status, "False");
    }

    #[test]
    fn destination_backend_validated() {
        let mut spec = base();
        spec.destination.backend = "azure-blob".into();
        let d = validate_spec(&spec).into_iter().find(|c| c.kind == "DestinationValid").unwrap();
        assert_eq!(d.status, "False");
    }

    #[test]
    fn s3_destination_requires_bucket() {
        let mut spec = base();
        spec.destination = ArchiveDestination {
            backend: "s3".into(),
            bucket: None,
            format: Some("parquet".into()),
        };
        let d = validate_spec(&spec).into_iter().find(|c| c.kind == "DestinationValid").unwrap();
        assert_eq!(d.status, "False");

        spec.destination.bucket = Some("velocity-archive".into());
        assert!(all_true(&validate_spec(&spec)));
    }

    #[test]
    fn s3_format_validated() {
        let mut spec = base();
        spec.destination = ArchiveDestination {
            backend: "s3".into(),
            bucket: Some("velocity-archive".into()),
            format: Some("avro".into()),
        };
        let d = validate_spec(&spec).into_iter().find(|c| c.kind == "DestinationValid").unwrap();
        assert_eq!(d.status, "False");
    }

    #[test]
    fn purge_after_validated_when_present() {
        let mut spec = base();
        spec.purge_after = Some("90d".into());
        assert!(all_true(&validate_spec(&spec)));

        spec.purge_after = Some("90".into());
        let conds = validate_spec(&spec);
        let p = conds.iter().find(|c| c.kind == "PurgeAfterValid").unwrap();
        assert_eq!(p.status, "False");
    }

    #[test]
    fn max_duration_validated_when_present() {
        let mut spec = base();
        spec.max_duration = Some("4h".into());
        assert!(all_true(&validate_spec(&spec)));

        spec.max_duration = Some("4hours".into());
        let conds = validate_spec(&spec);
        let m = conds.iter().find(|c| c.kind == "MaxDurationValid").unwrap();
        assert_eq!(m.status, "False");
    }
}
