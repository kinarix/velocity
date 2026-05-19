//! Generic CRUD handlers — one set of functions serves every schema.
//!
//! The handlers look up `ResolvedSchema` from the registry on each request
//! (an atomic pointer load — see ADR-006), then run the work inside a
//! transaction with the ADR-007 role/identity prelude. The handler logic is
//! schema-agnostic; what differs per schema is the field list, the table
//! name, and the role names — all sourced from `ResolvedSchema`.
//!
//! Phase 1 deliberately keeps validation, idempotency, and auth as seams
//! (tasks #15, #16, #17). Identity is stubbed to `anonymous`; payload is
//! taken as-is.

use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::{json, Value};
use sqlx::Row;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{FieldKind, SearchTier};

use crate::audit::{self, action, outcome};
use crate::event_log::{self, EventLogRow, EventSource};
use crate::auth::AuthDecision;
use crate::error::ApiError;
use crate::field_filter::FieldFilterIndex;
use crate::identity::Identity;
use crate::idempotency::{self, CachedResponse, Lookup};
use crate::policy;
use crate::query::{build_list, ListQuery};
use crate::rbac::{check_access, op};
use crate::registry::ResolvedSchema;
use crate::row_filter;
use crate::session::{with_session_context, RoleClass};
use crate::state::AppState;
use crate::validate::{validate_fields, validate_rules, WriteMode};

const IDEMPOTENCY_HEADER: &str = "idempotency-key";
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Read the request id the `SetRequestIdLayer` attached. Returns `None`
/// when the header is absent or non-ASCII (it shouldn't be either, but
/// audit-write paths must never blow up on header weirdness).
fn request_id_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers.get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok())
}

/// Wrap a result and, if it is a 401/403-class `ApiError`, write a
/// denial audit row in a short side-tx before returning the error.
///
/// Audit-write failure does NOT block the response — we log + continue.
/// The intent is observability, not a security gate; the 403 is
/// already happening upstream.
async fn audit_if_denied<T>(
    state: &AppState,
    schema: &ResolvedSchema,
    identity: &Identity,
    action: &str,
    decision: Option<&AuthDecision>,
    request_id: Option<&str>,
    result: Result<T, ApiError>,
) -> Result<T, ApiError> {
    match result {
        Ok(v) => Ok(v),
        Err(err) => {
            let status = err.status();
            if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
                let code = err.code();
                if let Err(e) = audit::write_audit_denial(
                    &state.pool,
                    schema,
                    identity,
                    action,
                    code,
                    decision,
                    request_id,
                )
                .await
                {
                    tracing::error!(
                        error = %e,
                        code = %code,
                        action = %action,
                        actor = %identity.actor_id,
                        "denial audit write failed"
                    );
                }
            }
            Err(err)
        }
    }
}

/// URL path: `/api/{org}/{app}/{domain}/{object}/{version}`.
pub(crate) type SchemaPathParts = (String, String, String, String, String);

pub(crate) fn path_from_parts(parts: SchemaPathParts) -> SchemaPath {
    SchemaPath::new(parts.0, parts.1, parts.2, parts.3, parts.4)
}

pub(crate) fn resolve_schema(
    state: &AppState,
    parts: SchemaPathParts,
) -> Result<std::sync::Arc<ResolvedSchema>, ApiError> {
    let path = path_from_parts(parts);
    state.registry.resolve(&path).ok_or(ApiError::SchemaNotFound)
}

/// Take the `Identity` the auth middleware attached to the request, falling
/// back to `Identity::anonymous()` when the middleware isn't wired (Phase 1
/// integration tests, healthcheck-only deployments). The RBAC gate decides
/// what an anonymous identity may actually do — see [`crate::rbac`].
pub(crate) fn identity_from_ext(ext: Option<Extension<Identity>>) -> Identity {
    ext.map(|Extension(id)| id).unwrap_or_else(Identity::anonymous)
}

/// Layer-5 write gate. Returns [`ApiError::FieldWriteDenied`] with the
/// sorted list of forbidden field names so an integrator can fix their
/// payload without guessing.
fn check_field_writes(
    filter: &FieldFilterIndex,
    payload: &serde_json::Map<String, Value>,
    identity: &Identity,
) -> Result<(), ApiError> {
    let denied = filter.check_writes(payload, &identity.roles);
    if denied.is_empty() {
        Ok(())
    } else {
        Err(ApiError::FieldWriteDenied(denied))
    }
}

/// Render a `$N` placeholder with the cast appropriate to `kind`.
///
/// We bind every field value as `jsonb` (sqlx encodes `serde_json::Value`
/// that way) and let Postgres unpack it into the column's real type. For
/// scalar text-shaped types (`text`, `uuid`, `date`, `timestamptz`) we use
/// the `#>> '{}'` operator to drop the surrounding JSON quotes; for numeric
/// and boolean we cast the jsonb value directly.
pub fn cast_placeholder(idx: usize, kind: FieldKind) -> String {
    match kind {
        FieldKind::String | FieldKind::Enum | FieldKind::Ref => {
            format!("(${idx}::jsonb #>> '{{}}')")
        }
        FieldKind::Integer => format!("(${idx}::jsonb)::bigint"),
        FieldKind::Number => format!("(${idx}::jsonb)::numeric"),
        FieldKind::Boolean => format!("(${idx}::jsonb)::boolean"),
        FieldKind::Date => format!("(${idx}::jsonb #>> '{{}}')::date"),
        FieldKind::Datetime => format!("(${idx}::jsonb #>> '{{}}')::timestamptz"),
        FieldKind::Uuid => format!("(${idx}::jsonb #>> '{{}}')::uuid"),
        FieldKind::Json => format!("${idx}::jsonb"),
    }
}

/// Fetch a single row by id as JSON. Returns Ok(None) when the row exists but
/// is soft-deleted, Ok(Some) for an alive row, Err for DB issues.
///
/// `scope` is the Layer-4 row-filter predicate for the caller, AND'd into the
/// WHERE. Passing `None` is intentional (open schema / unrestricted role);
/// callers must never default to `None` to "skip" the gate — they have to
/// have asked the row-filter index and gotten back `None` first.
async fn fetch_one_json(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    qualified_table: &str,
    id: &str,
    scope: Option<&row_filter::Predicate>,
) -> Result<Option<Value>, sqlx::Error> {
    let scope_sql = scope.map(|p| format!(" AND {}", p.sql)).unwrap_or_default();
    // Subtract `__fts` so the Phase-5b generated tsvector column
    // never leaks to clients. The jsonb `-` operator is a no-op when
    // the key is absent, so this is correct on Tier-1 tables too.
    let sql = format!(
        "SELECT (to_jsonb(t) - '__fts') AS row FROM {qualified_table} t \
         WHERE id = $1::uuid AND deleted_at IS NULL{scope_sql}"
    );
    let mut q = sqlx::query(&sql).bind(id);
    if let Some(p) = scope {
        for v in &p.params {
            q = row_filter::bind_json_param(q, v);
        }
    }
    let row = q.fetch_optional(&mut **tx).await?;
    Ok(row.map(|r| r.get::<Value, _>("row")))
}

// ---------- LIST ----------------------------------------------------------

pub async fn list(
    State(state): State<AppState>,
    Path(parts): Path<SchemaPathParts>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, parts)?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::READ,
        decision.as_ref(),
        request_id,
        check_access(&schema, &identity, op::READ),
    )
    .await?;
    let compiled = build_list(&schema, &q, &identity)?;

    let items = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Reader,
        &identity,
        move |tx| {
            Box::pin(async move {
                // Wrap each row in `row_to_json` so we can stream arbitrary
                // user-declared columns back without per-schema struct
                // mapping.
                let select = compiled.sql.replacen(
                    "SELECT * FROM",
                    "SELECT (to_jsonb(t.*) - '__fts') AS row FROM",
                    1,
                );
                let mut q = sqlx::query(&select);
                for v in &compiled.params {
                    q = row_filter::bind_json_param(q, v);
                }
                let rows = q.fetch_all(&mut **tx).await?;
                let items: Vec<Value> =
                    rows.into_iter().map(|r| r.get::<Value, _>("row")).collect();
                Ok(items)
            })
        },
    )
    .await?;

    // Layer-5 read strip — applied row-by-row so an actor without the
    // role for a sensitive column never sees it leave the server. The
    // SQL fetched all columns because filtering at SQL would force every
    // schema to know its role-to-column matrix; strip-in-app is cheaper.
    //
    // Layer-6 masking runs immediately after the strip on the same row.
    // Ordering matters: a stripped field is gone, so masking can't
    // resurrect it; a masked field is still present, just transformed.
    let mut items = items;
    for row in &mut items {
        schema.field_filter.strip_for_read(row, &identity.roles);
        schema.masking.apply_for_read(row, &identity.roles);
    }

    // Phase 6a-1: every successful read produces an audit row. Entity
    // id is NULL — `list` returns a set, not a single record. Payload
    // carries only the result count; redaction backstops anything we
    // accidentally widen later.
    if let Err(e) = audit::write_audit_standalone(
        &state.pool,
        &schema,
        &identity,
        action::READ,
        outcome::SUCCESS,
        None,
        &json!({ "count": items.len() }),
        decision.as_ref(),
        request_id,
    )
    .await
    {
        tracing::warn!(error = %e, "list audit write failed");
    }

    Ok(Json(json!({
        "items": items,
        "cursor": Value::Null,
    })))
}

// ---------- CREATE --------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    Path(parts): Path<SchemaPathParts>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
    Json(payload): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let schema = resolve_schema(&state, parts)?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers).map(str::to_owned);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::CREATE,
        decision.as_ref(),
        request_id.as_deref(),
        check_access(&schema, &identity, op::CREATE),
    )
    .await?;

    let payload_obj = payload
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("payload must be a JSON object".into()))?
        .clone();

    // Layer-2 ABAC. Runs after RBAC (cheap allow/deny by role) and before
    // validation (deeper field/CEL checks). A policy that needs the full
    // payload sees the un-touched submission — same view the SQL builder
    // will operate on.
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::CREATE,
        decision.as_ref(),
        request_id.as_deref(),
        policy::evaluate_for(
            &schema.compiled_policies,
            op::CREATE,
            &Value::Object(payload_obj.clone()),
            &identity,
        )
        .await,
    )
    .await?;

    // Layer-5 field-filter — reject before idempotency stores anything,
    // so a 403 on the first attempt doesn't poison the idempotency-key
    // cache with a no-op response that future retries would replay.
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::CREATE,
        decision.as_ref(),
        request_id.as_deref(),
        check_field_writes(&schema.field_filter, &payload_obj, &identity),
    )
    .await?;

    // ── Idempotency (#16) — if present, replay or 409 before doing work.
    let idempotency_key = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let request_hash = idempotency_key
        .as_ref()
        .map(|_| idempotency::hash_payload(&Value::Object(payload_obj.clone())));
    if let (Some(key), Some(hash)) = (idempotency_key.as_deref(), request_hash.as_deref()) {
        idempotency::validate_key(key)?;
        match idempotency::lookup(&state.pool, key, hash).await? {
            Lookup::Replay(cached) => {
                let status = StatusCode::from_u16(cached.status)
                    .unwrap_or(StatusCode::CREATED);
                let mut body = cached.body;
                schema.field_filter.strip_for_read(&mut body, &identity.roles);
                schema.masking.apply_for_read(&mut body, &identity.roles);
                return Ok((status, Json(body)));
            }
            Lookup::Conflict => return Err(ApiError::IdempotencyConflict),
            Lookup::Miss => {}
        }
    }

    // ── Validation (#15) — runs before any SQL is built.
    validate_fields(&schema, &payload_obj, WriteMode::Create)?;
    validate_rules(&schema.compiled_validations, &Value::Object(payload_obj.clone())).await?;

    // Build column/value/cast lists from declared fields only. Anything the
    // payload contains that isn't in the schema is silently ignored — this
    // matches the validator's "unknown fields are out-of-band" stance.
    let mut cols: Vec<String> = Vec::new();
    let mut casts: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for f in schema.fields.ordered.iter() {
        if let Some(v) = payload_obj.get(&f.name) {
            vals.push(v.clone());
            cols.push(f.name.clone());
            casts.push(cast_placeholder(vals.len(), f.kind));
        }
    }

    let tier = schema.spec.search.tier;
    let table = schema.pg_qualified.clone();
    let outbox_table = format!("{}.{}_outbox", schema.pg_schema, schema.pg_table);
    let audit_schema = schema.clone();
    let audit_identity = identity.clone();
    let audit_decision = decision.clone();
    let event_schema = schema.clone();
    let event_identity = identity.clone();
    // Phase 6a-3: captured before the closure-move so the post-image
    // we write into the audit row can carry `__fields_changed` = the
    // user-submitted top-level field names (id/timestamps/version are
    // server-managed and filtered out by `submitted_field_names`).
    let submitted_fields =
        audit::submitted_field_names(&Value::Object(payload_obj.clone()));

    let inserted = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Writer,
        &identity,
        move |tx| {
            Box::pin(async move {
                let row = insert_row(tx, &table, &cols, &casts, &vals).await?;
                // `RETURNING row_to_json(table.*)` must always include `id`
                // for any schema-managed table. A missing/non-string `id`
                // means the table shape diverged from the schema invariants
                // — fail closed so we never bind an empty id into the audit
                // chain or the outbox.
                let id = row["id"]
                    .as_str()
                    .ok_or_else(|| {
                        sqlx::Error::Protocol(
                            "insert returned row without string `id` column".into(),
                        )
                    })?
                    .to_string();
                if matches!(tier, SearchTier::Tier3) {
                    write_outbox(tx, &outbox_table, "create", &id, &row).await?;
                }
                // ADR-005: audit row writes in the same tx as the data
                // change. A best-effort log line *after* commit would leave
                // the data and audit chain visibly out of sync on a crash.
                //
                // Phase 6a-3: record which fields the caller submitted so
                // `__fields_changed` is queryable from `platform.audit_log`
                // without diffing two payloads.
                let mut audit_payload = row.clone();
                audit::attach_fields_changed(&mut audit_payload, &submitted_fields);
                audit::write_audit(
                    tx,
                    &audit_schema,
                    &audit_identity,
                    action::CREATE,
                    outcome::SUCCESS,
                    Some(&id),
                    &audit_payload,
                    audit_decision.as_ref(),
                    None,
                )
                .await?;
                // Phase 3 — append to platform.event_log in the same tx so
                // /history reflects the create the moment the row is
                // visible. No `diff` on create (the payload IS the diff
                // from `null`); no `request_id` plumbing yet — the
                // middleware layer that surfaces it lands later.
                event_log::write(
                    tx,
                    EventLogRow {
                        schema: &event_schema,
                        entity_id: &id,
                        operation: action::CREATE,
                        source: EventSource::Api,
                        identity: &event_identity,
                        request_id: None,
                        diff: None,
                        payload: Some(row.clone()),
                        reason: None,
                    },
                )
                .await?;
                Ok(row)
            })
        },
    )
    .await?;

    // ── Idempotency record — write after the work commits so replays
    // return the same body the first caller got. A lost race here is
    // harmless: the existing row already has the same hash.
    //
    // Note: we cache the FULL row (pre-strip) so a future replay that
    // happens to carry a wider-role identity sees what they're entitled
    // to. Strip runs after the cache lookup on every request, including
    // replays — see the Lookup::Replay arm above.
    if let (Some(key), Some(hash)) = (idempotency_key.as_deref(), request_hash.as_deref()) {
        let cached =
            CachedResponse { status: StatusCode::CREATED.as_u16(), body: inserted.clone() };
        idempotency::record(&state.pool, key, hash, &cached).await?;
    }

    let mut body = inserted;
    schema.field_filter.strip_for_read(&mut body, &identity.roles);
    schema.masking.apply_for_read(&mut body, &identity.roles);
    Ok((StatusCode::CREATED, Json(body)))
}

async fn insert_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    qualified_table: &str,
    cols: &[String],
    casts: &[String],
    vals: &[Value],
) -> Result<Value, sqlx::Error> {
    let col_list = if cols.is_empty() {
        // INSERT with all defaults — relies on system columns having
        // sensible defaults (id uuid_generate_v4(), created_at now(), …).
        "DEFAULT VALUES".to_string()
    } else {
        format!("({}) VALUES ({})", cols.join(", "), casts.join(", "))
    };
    let sql = format!(
        "INSERT INTO {qualified_table} {col_list} \
         RETURNING (to_jsonb({qualified_table}.*) - '__fts') AS row"
    );
    let mut q = sqlx::query(&sql);
    for v in vals {
        q = q.bind(v);
    }
    let row = q.fetch_one(&mut **tx).await?;
    Ok(row.get::<Value, _>("row"))
}

async fn write_outbox(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    outbox_table: &str,
    op: &str,
    entity_id: &str,
    payload: &Value,
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "INSERT INTO {outbox_table} (op, entity_id, payload) VALUES ($1, $2::uuid, $3)"
    );
    sqlx::query(&sql).bind(op).bind(entity_id).bind(payload).execute(&mut **tx).await?;
    Ok(())
}

// ---------- GET -----------------------------------------------------------

pub async fn get_one(
    State(state): State<AppState>,
    Path((org, app, domain, object, version, id)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::READ,
        decision.as_ref(),
        request_id,
        check_access(&schema, &identity, op::READ),
    )
    .await?;
    let table = schema.pg_qualified.clone();
    // id is $1, so row-filter binds start at $2.
    let scope = row_filter::predicate_for(&schema, &identity, 2)?;
    let id_for_audit = id.clone();

    let row = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Reader,
        &identity,
        move |tx| {
            Box::pin(async move { fetch_one_json(tx, &table, &id, scope.as_ref()).await })
        },
    )
    .await?;

    let mut row = row.ok_or(ApiError::NotFound)?;
    schema.field_filter.strip_for_read(&mut row, &identity.roles);
    schema.masking.apply_for_read(&mut row, &identity.roles);

    // Phase 6a-1: success-path audit for `get_one`. Entity id is known
    // (single-row read), payload is a minimal `{ "id": ... }` to mirror
    // the delete shape — never the row body, which would defeat the
    // sensitivity redaction we apply on writes.
    if let Err(e) = audit::write_audit_standalone(
        &state.pool,
        &schema,
        &identity,
        action::READ,
        outcome::SUCCESS,
        Some(&id_for_audit),
        &json!({ "id": id_for_audit }),
        decision.as_ref(),
        request_id,
    )
    .await
    {
        tracing::warn!(error = %e, "get_one audit write failed");
    }

    Ok(Json(row))
}

// ---------- UPDATE --------------------------------------------------------

pub async fn update(
    State(state): State<AppState>,
    Path((org, app, domain, object, version, id)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers).map(str::to_owned);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::UPDATE,
        decision.as_ref(),
        request_id.as_deref(),
        check_access(&schema, &identity, op::UPDATE),
    )
    .await?;

    let payload_obj = payload
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("payload must be a JSON object".into()))?
        .clone();

    // Layer-2 ABAC — see comment on `create`. For updates the policy sees
    // the *requested* mutation, not the merged post-image; that's
    // intentional, so a "you may only set X to Y" rule fires on the
    // submitted value rather than on the row that already exists.
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::UPDATE,
        decision.as_ref(),
        request_id.as_deref(),
        policy::evaluate_for(
            &schema.compiled_policies,
            op::UPDATE,
            &Value::Object(payload_obj.clone()),
            &identity,
        )
        .await,
    )
    .await?;

    // Layer-5 field-filter on writes. `version` is on every UPDATE payload
    // but it isn't a user-declared field, so `check_writes` will pass it
    // through (FieldFilterIndex keys on declared field names only).
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::UPDATE,
        decision.as_ref(),
        request_id.as_deref(),
        check_field_writes(&schema.field_filter, &payload_obj, &identity),
    )
    .await?;

    let expected_version: i32 = payload_obj
        .get("version")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ApiError::BadRequest("`version` is required for update".into()))?
        as i32;

    // ── Validation (#15) — partial-update semantics: only the supplied
    // fields are field-checked, but rule-level validations still run over
    // the (partial) payload. Rules that reference fields not present will
    // see Null and decide on their own.
    validate_fields(&schema, &payload_obj, WriteMode::Update)?;
    validate_rules(&schema.compiled_validations, &Value::Object(payload_obj.clone())).await?;

    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for f in schema.fields.ordered.iter() {
        // `version` is the optimistic-lock guard, not user-updatable.
        if f.name == "version" {
            continue;
        }
        if let Some(v) = payload_obj.get(&f.name) {
            vals.push(v.clone());
            sets.push(format!("{} = {}", f.name, cast_placeholder(vals.len(), f.kind)));
        }
    }
    if sets.is_empty() {
        return Err(ApiError::BadRequest("no updatable fields supplied".into()));
    }

    // Add updated_at/updated_by/version as fixed expressions and append the
    // id/version params after the value binds.
    sets.push("updated_at = now()".into());
    sets.push("updated_by = current_setting('app.current_user', true)".into());
    sets.push("version = version + 1".into());

    let id_idx = vals.len() + 1;
    let ver_idx = vals.len() + 2;
    let table = schema.pg_qualified.clone();
    // Row-filter binds (if any) live after id/version — start at $(vals + 3).
    let scope = row_filter::predicate_for(&schema, &identity, vals.len() + 3)?;
    let scope_sql = scope
        .as_ref()
        .map(|p| format!(" AND {}", p.sql))
        .unwrap_or_default();
    let sql = format!(
        "UPDATE {table} SET {} WHERE id = ${id_idx}::uuid AND version = ${ver_idx} AND deleted_at IS NULL{scope_sql} \
         RETURNING (to_jsonb({table}.*) - '__fts') AS row",
        sets.join(", ")
    );

    let tier = schema.spec.search.tier;
    let outbox_table = format!("{}.{}_outbox", schema.pg_schema, schema.pg_table);
    let audit_schema = schema.clone();
    let audit_identity = identity.clone();
    let audit_decision = decision.clone();
    let event_schema = schema.clone();
    let event_identity = identity.clone();
    let pre_select_sql = format!(
        "SELECT (to_jsonb({table}.*) - '__fts') AS row FROM {table} \
         WHERE id = $1::uuid AND deleted_at IS NULL"
    );

    let updated = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Writer,
        &identity,
        move |tx| {
            Box::pin(async move {
                // Snapshot the pre-update state inside the same tx so the
                // event_log `diff` reflects what actually changed under
                // the lock the UPDATE will take. SELECT-then-UPDATE in
                // one tx is race-free because the UPDATE takes a row
                // lock and the optimistic-lock check on version detects
                // any interleaved write.
                let before: Option<Value> = sqlx::query(&pre_select_sql)
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .map(|r| r.get::<Value, _>("row"));

                let mut q = sqlx::query(&sql);
                for v in &vals {
                    q = q.bind(v);
                }
                q = q.bind(&id).bind(expected_version);
                if let Some(p) = &scope {
                    for v in &p.params {
                        q = row_filter::bind_json_param(q, v);
                    }
                }
                let result = q.fetch_optional(&mut **tx).await?;
                let row = match result {
                    Some(r) => r.get::<Value, _>("row"),
                    None => {
                        // Either no such id, version mismatch, or the row is
                        // outside the caller's row-scope. We collapse the
                        // last case into NotFound (don't leak existence to
                        // scoped readers) and reuse the existing probe for
                        // id-existence to distinguish 404 vs 409. The probe
                        // does *not* re-apply the scope — a non-existent id
                        // and a scope-hidden id both produce NotFound here.
                        let table = qualified_table_from_sql(&sql);
                        let probe_sql =
                            format!("SELECT id FROM {table} WHERE id = $1::uuid LIMIT 1");
                        let exists = sqlx::query(&probe_sql)
                            .bind(&id)
                            .fetch_optional(&mut **tx)
                            .await?;
                        // If we had a scope, "exists" alone isn't enough —
                        // the row may exist but be hidden. Treat as NotFound
                        // unless there's no scope at all.
                        return Err(if exists.is_some() && scope.is_none() {
                            sqlx::Error::Protocol("__version_conflict__".into())
                        } else {
                            sqlx::Error::RowNotFound
                        });
                    }
                };
                if matches!(tier, SearchTier::Tier3) {
                    write_outbox(tx, &outbox_table, "update", &id, &row).await?;
                }
                // Phase 6a-3: strict before/after delta — only fields
                // whose value actually changed. UPDATEs that no-op a
                // field (re-submitting the same value) won't pollute
                // the changed-list.
                let changed = audit::changed_field_names(before.as_ref(), &row);
                let mut audit_payload = row.clone();
                audit::attach_fields_changed(&mut audit_payload, &changed);
                audit::write_audit(
                    tx,
                    &audit_schema,
                    &audit_identity,
                    action::UPDATE,
                    outcome::SUCCESS,
                    Some(&id),
                    &audit_payload,
                    audit_decision.as_ref(),
                    None,
                )
                .await?;
                // Phase 3 — event_log row. `before` is `Some(...)` on the
                // common path; `None` would only happen if the row vanished
                // between our pre-SELECT and the UPDATE (e.g., concurrent
                // hard delete), in which case the UPDATE would have already
                // returned RowNotFound and we wouldn't be here. So the
                // diff is unconditionally meaningful.
                let diff_value = before.as_ref().map(|b| event_log::diff(b, &row));
                event_log::write(
                    tx,
                    EventLogRow {
                        schema: &event_schema,
                        entity_id: &id,
                        operation: action::UPDATE,
                        source: EventSource::Api,
                        identity: &event_identity,
                        request_id: None,
                        diff: diff_value,
                        payload: Some(row.clone()),
                        reason: None,
                    },
                )
                .await?;
                Ok(row)
            })
        },
    )
    .await
    .map_err(map_update_err)?;

    let mut body = updated;
    schema.field_filter.strip_for_read(&mut body, &identity.roles);
    schema.masking.apply_for_read(&mut body, &identity.roles);
    Ok(Json(body))
}

/// Translate the sentinel errors raised inside the UPDATE closure into
/// real `ApiError`s. `sqlx::Error` is the closure return type, so we
/// signal "version conflict" via a recognisable Protocol error string.
fn map_update_err(e: sqlx::Error) -> ApiError {
    match &e {
        sqlx::Error::RowNotFound => ApiError::NotFound,
        sqlx::Error::Protocol(msg) if msg == "__version_conflict__" => ApiError::VersionConflict,
        _ => ApiError::Database(e),
    }
}

/// Extract the qualified table name from the UPDATE SQL we just built.
/// (We don't have it directly in scope at the point we need it because the
/// closure moved everything; rather than restructure the lifetimes we parse
/// it back. The SQL is built locally so this is safe.)
fn qualified_table_from_sql(sql: &str) -> String {
    sql.split_whitespace()
        .skip_while(|w| !w.eq_ignore_ascii_case("UPDATE"))
        .nth(1)
        .unwrap_or("")
        .to_string()
}

// ---------- DELETE (soft) -------------------------------------------------

pub async fn delete_soft(
    State(state): State<AppState>,
    Path((org, app, domain, object, version, id)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
) -> Result<StatusCode, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers).map(str::to_owned);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::DELETE,
        decision.as_ref(),
        request_id.as_deref(),
        check_access(&schema, &identity, op::DELETE),
    )
    .await?;
    // Layer-2 ABAC on delete. The policy sees `self = null` since the
    // request body is empty — useful for "only admin actors may delete"
    // expressed as `identity.attributes.role == 'admin'`.
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::DELETE,
        decision.as_ref(),
        request_id.as_deref(),
        policy::evaluate_for(&schema.compiled_policies, op::DELETE, &Value::Null, &identity)
            .await,
    )
    .await?;
    let table = schema.pg_qualified.clone();
    // id is $1, so row-filter binds start at $2.
    let scope = row_filter::predicate_for(&schema, &identity, 2)?;
    let tier = schema.spec.search.tier;
    let outbox_table = format!("{}.{}_outbox", schema.pg_schema, schema.pg_table);
    let audit_schema = schema.clone();
    let audit_identity = identity.clone();
    let audit_decision = decision.clone();
    let event_schema = schema.clone();
    let event_identity = identity.clone();

    let result = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Admin,
        &identity,
        move |tx| {
            Box::pin(async move {
                let scope_sql = scope
                    .as_ref()
                    .map(|p| format!(" AND {}", p.sql))
                    .unwrap_or_default();
                let sql = format!(
                    "UPDATE {table} \
                     SET deleted_at = now(), updated_at = now(), \
                         updated_by = current_setting('app.current_user', true) \
                     WHERE id = $1::uuid AND deleted_at IS NULL{scope_sql}"
                );
                let mut q = sqlx::query(&sql).bind(&id);
                if let Some(p) = &scope {
                    for v in &p.params {
                        q = row_filter::bind_json_param(q, v);
                    }
                }
                let result = q.execute(&mut **tx).await?;
                if result.rows_affected() == 0 {
                    return Err(sqlx::Error::RowNotFound);
                }
                if matches!(tier, SearchTier::Tier3) {
                    write_outbox(tx, &outbox_table, "delete", &id, &json!({ "id": id }))
                        .await?;
                }
                audit::write_audit(
                    tx,
                    &audit_schema,
                    &audit_identity,
                    action::DELETE,
                    outcome::SUCCESS,
                    Some(&id),
                    &json!({ "id": id }),
                    audit_decision.as_ref(),
                    None,
                )
                .await?;
                // Phase 3 — delete event. No payload (the entity is now
                // tombstoned; readers can rebuild prior state from the
                // preceding event_log rows). No diff for the same reason
                // — `null - prior_state` would just be a giant remove op
                // that adds no information beyond "this row was deleted."
                event_log::write(
                    tx,
                    EventLogRow {
                        schema: &event_schema,
                        entity_id: &id,
                        operation: action::DELETE,
                        source: EventSource::Api,
                        identity: &event_identity,
                        request_id: None,
                        diff: None,
                        payload: None,
                        reason: None,
                    },
                )
                .await?;
                Ok(())
            })
        },
    )
    .await;

    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(sqlx::Error::RowNotFound) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Database(e)),
    }
}

// ---------- QUERY (Phase 5a — POST /query DSL) ----------------------------

/// POST /api/{...}/query — DSL endpoint backing
/// nested AND/OR/NOT WHERE, ranged sort, projection, cross-schema
/// includes (`FieldSpec.ref`), and HMAC-signed keyset cursor.
///
/// Layer ordering mirrors `list`:
///   1. Resolve schema + identity
///   2. Layer-1 RBAC (`op::READ`) — caller must hold read on THIS schema
///   3. DSL compile (which embeds Layer-3 cross-schema RBAC for every
///      `include`, and Layer-4 row-filter)
///   4. Run SQL under reader role
///   5. Layer-5 strip + Layer-6 masking per row
///   6. Mint `next_cursor` if the plus-one fetch overflowed
pub async fn query(
    State(state): State<AppState>,
    Path(parts): Path<SchemaPathParts>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
    Json(dsl): Json<crate::dsl::QueryDsl>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, parts)?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers).map(str::to_owned);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::QUERY,
        decision.as_ref(),
        request_id.as_deref(),
        check_access(&schema, &identity, op::READ),
    )
    .await?;

    let signer = state.cursor_signer.as_deref();
    // dsl::build raises CrossSchemaAccessDenied from inside the include
    // resolver — wrap it so that denial path produces an audit row.
    let compiled = audit_if_denied(
        &state,
        &schema,
        &identity,
        action::QUERY,
        decision.as_ref(),
        request_id.as_deref(),
        crate::dsl::build(&schema, &dsl, &identity, &state.registry, signer),
    )
    .await?;

    let include_names: Vec<String> = dsl.include.clone();
    let page_limit = compiled.limit;
    let cursor_sort_sig = compiled.cursor_sort_sig.clone();
    let cursor_sort_fields = compiled.cursor_sort_fields.clone();
    let schema_key = compiled.schema_key.clone();

    let rows: Vec<Value> = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Reader,
        &identity,
        move |tx| {
            Box::pin(async move {
                let mut q = sqlx::query(&compiled.sql);
                for v in &compiled.params {
                    q = row_filter::bind_json_param(q, v);
                }
                let rows = q.fetch_all(&mut **tx).await?;
                let mut out: Vec<Value> = Vec::with_capacity(rows.len());
                for r in rows {
                    // Compiler always emits `__row` as a jsonb-shaped
                    // column. Include columns sit alongside as
                    // `__inc_<name>` — lift them into the main row
                    // under the friendly include name.
                    let mut obj = r
                        .try_get::<Value, _>("__row")
                        .map_err(|_| {
                            sqlx::Error::Protocol(
                                "query: SELECT did not produce __row".into(),
                            )
                        })?;
                    for inc in &include_names {
                        let alias = format!("__inc_{inc}");
                        if let Ok(joined) = r.try_get::<Value, _>(alias.as_str()) {
                            if let Some(m) = obj.as_object_mut() {
                                m.insert(inc.clone(), joined);
                            }
                        }
                    }
                    out.push(obj);
                }
                Ok(out)
            })
        },
    )
    .await?;

    // Plus-one fetch: if we got more than `limit`, drop the trailing
    // sentinel row and mint a cursor from it.
    let mut rows = rows;
    let has_more = rows.len() as u32 > page_limit;
    let next_cursor = if has_more {
        // The sentinel row is the boundary — but we mint the cursor
        // from the LAST returned row (page_limit-th), so the next
        // page starts from after it.
        rows.truncate(page_limit as usize);
        match (signer, rows.last()) {
            (Some(s), Some(last)) => Some(crate::dsl::mint_cursor(
                s,
                &schema_key,
                &cursor_sort_sig,
                &cursor_sort_fields,
                last,
            )?),
            _ => None,
        }
    } else {
        None
    };

    // Layer-5 strip + Layer-6 masking, same discipline as list().
    for row in &mut rows {
        schema.field_filter.strip_for_read(row, &identity.roles);
        schema.masking.apply_for_read(row, &identity.roles);
    }

    // Phase 6a-1: success-path audit. Entity id is NULL — query
    // returns a set. Payload keeps only the result count; we
    // deliberately do not record the DSL itself (may carry PII in
    // filter values).
    if let Err(e) = audit::write_audit_standalone(
        &state.pool,
        &schema,
        &identity,
        action::QUERY,
        outcome::SUCCESS,
        None,
        &json!({ "count": rows.len() }),
        decision.as_ref(),
        request_id.as_deref(),
    )
    .await
    {
        tracing::warn!(error = %e, "query audit write failed");
    }

    Ok(Json(crate::dsl::build_response(rows, next_cursor)))
}

// ---------- SEARCH (Phase 5c — Tier-3 Typesense) --------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SearchRequest {
    /// The free-text query string. Required.
    pub q: String,
    /// Comma-separated list of fields to query. Default: every
    /// `searchable: true` field on the schema.
    #[serde(default)]
    pub query_by: Option<String>,
    /// Typesense `filter_by` syntax, e.g. `status:approved`.
    #[serde(default)]
    pub filter_by: Option<String>,
    /// `field:asc|desc`.
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub per_page: Option<u32>,
    #[serde(default)]
    pub page: Option<u32>,
}

/// POST /api/{...}/search — Tier-3 search over a single schema's
/// Typesense collection. Tier 1 / Tier 2 schemas reject with 400 —
/// callers should use the DSL's `q` field for Tier-2 FTS.
pub async fn search(
    State(state): State<AppState>,
    Path(parts): Path<SchemaPathParts>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, parts)?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers);
    audit_if_denied(
        &state,
        &schema,
        &identity,
        action::SEARCH,
        decision.as_ref(),
        request_id,
        check_access(&schema, &identity, op::READ),
    )
    .await?;

    if !matches!(schema.spec.search.tier, SearchTier::Tier3) {
        return Err(ApiError::BadRequest(
            "search is only available on Tier-3 schemas; use POST /query with `q` for Tier-2 FTS"
                .into(),
        ));
    }
    let typesense = state.typesense.as_deref().ok_or(ApiError::SearchUnconfigured)?;

    let query_by = req.query_by.unwrap_or_else(|| {
        // Default to every searchable field, comma-joined.
        let parts: Vec<String> = schema
            .fields
            .ordered
            .iter()
            .filter(|f| f.searchable)
            .map(|f| f.name.clone())
            .collect();
        if parts.is_empty() {
            "id".to_string() // last-resort: search by id so the call doesn't 400
        } else {
            parts.join(",")
        }
    });

    let params = crate::typesense::SearchParams {
        q: req.q,
        query_by,
        filter_by: req.filter_by,
        sort_by: req.sort_by,
        per_page: req.per_page,
        page: req.page,
    };

    let coll = crate::cdc::schema_collection_name(&schema);
    let resp = typesense
        .search(&coll, &params)
        .await
        .map_err(|e| ApiError::SearchUnavailable(e.to_string()))?;

    // Phase 6a-1: success-path audit for `search`. Entity id is NULL
    // (a search returns a set). Payload records only the result
    // `found` count from Typesense — never the query string itself
    // (free text can carry PII the SearchRequest never sees a schema
    // for, so redaction can't help us there).
    let found = resp.get("found").and_then(|v| v.as_u64()).unwrap_or(0);
    if let Err(e) = audit::write_audit_standalone(
        &state.pool,
        &schema,
        &identity,
        action::SEARCH,
        outcome::SUCCESS,
        None,
        &json!({ "count": found }),
        decision.as_ref(),
        request_id,
    )
    .await
    {
        tracing::warn!(error = %e, "search audit write failed");
    }

    Ok(Json(resp))
}

/// POST /api/{org}/search — cross-schema search. Hits the per-org
/// Typesense collection populated by CDC for schemas that opt in via
/// `search.cross_search: true`. RBAC filter: only schemas the caller
/// has read access on are admitted, applied as a Typesense
/// `filter_by` clause.
pub async fn cross_search(
    State(state): State<AppState>,
    Path(org): Path<String>,
    identity_ext: Option<Extension<Identity>>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<Value>, ApiError> {
    let identity = identity_from_ext(identity_ext);
    let typesense = state.typesense.as_deref().ok_or(ApiError::SearchUnconfigured)?;

    // RBAC: enumerate every Tier-3, cross-search-enabled schema in
    // this org that the caller has read access on. The resulting
    // schema-path list becomes the `filter_by` for Typesense.
    let snapshot = state.registry.snapshot();
    let mut allowed_schemas: Vec<String> = Vec::new();
    for (_, schema) in snapshot.by_path.iter() {
        if schema.path.org != org {
            continue;
        }
        if !matches!(schema.spec.search.tier, SearchTier::Tier3) {
            continue;
        }
        if !schema.spec.search.cross_search {
            continue;
        }
        if crate::rbac::check_access(schema, &identity, crate::rbac::op::READ).is_ok() {
            allowed_schemas.push(schema.path.to_string());
        }
    }
    if allowed_schemas.is_empty() {
        // No schemas in this org are visible to the caller. Empty
        // result set is correct (and explicit — not silent).
        return Ok(Json(serde_json::json!({
            "found": 0,
            "hits": [],
            "page": req.page.unwrap_or(1),
        })));
    }

    let schema_filter = format!(
        "__schema:=[{}]",
        allowed_schemas
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(",")
    );
    // Compose with any user-supplied filter_by (AND'd).
    let filter_by = match req.filter_by {
        Some(extra) => Some(format!("{schema_filter} && {extra}")),
        None => Some(schema_filter),
    };

    let params = crate::typesense::SearchParams {
        q: req.q,
        query_by: req.query_by.unwrap_or_else(|| "__body,title".into()),
        filter_by,
        sort_by: req.sort_by,
        per_page: req.per_page,
        page: req.page,
    };

    let coll = crate::cdc::cross_collection_name(&org);
    let resp = typesense
        .search(&coll, &params)
        .await
        .map_err(|e| ApiError::SearchUnavailable(e.to_string()))?;

    // Phase 6a-1: cross-schema audit deferred. write_audit binds
    // `schema_org` from a single ResolvedSchema for sensitivity
    // redaction; this request fans across N schemas with no canonical
    // owner, so an audit row needs a synthetic key (e.g.
    // `{org}/__cross`) and a different redaction story. Tracked as
    // Phase 6a follow-up; the per-schema reads inside Typesense are
    // already covered by collection-side ACLs.
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_table_parsing() {
        let sql = "UPDATE acme_x_y.po_v1 SET foo = $1 WHERE id = $2";
        assert_eq!(qualified_table_from_sql(sql), "acme_x_y.po_v1");
    }
}
