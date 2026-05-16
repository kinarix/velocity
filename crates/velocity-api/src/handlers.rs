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
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use sqlx::Row;
use velocity_types::common::SchemaPath;
use velocity_types::crds::schema::{FieldKind, SearchTier};

use crate::error::ApiError;
use crate::identity::Identity;
use crate::query::{build_list, ListQuery};
use crate::registry::ResolvedSchema;
use crate::session::{with_session_context, RoleClass};
use crate::state::AppState;
use crate::validate::{validate_fields, validate_rules, WriteMode};

/// URL path: `/api/{org}/{app}/{domain}/{object}/{version}`.
type SchemaPathParts = (String, String, String, String, String);

fn path_from_parts(parts: SchemaPathParts) -> SchemaPath {
    SchemaPath::new(parts.0, parts.1, parts.2, parts.3, parts.4)
}

fn resolve_schema(
    state: &AppState,
    parts: SchemaPathParts,
) -> Result<std::sync::Arc<ResolvedSchema>, ApiError> {
    let path = path_from_parts(parts);
    state.registry.resolve(&path).ok_or(ApiError::SchemaNotFound)
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
async fn fetch_one_json(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    qualified_table: &str,
    id: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let sql = format!(
        "SELECT row_to_json(t) AS row FROM {qualified_table} t \
         WHERE id = $1::uuid AND deleted_at IS NULL"
    );
    let row = sqlx::query(&sql).bind(id).fetch_optional(&mut **tx).await?;
    Ok(row.map(|r| r.get::<Value, _>("row")))
}

// ---------- LIST ----------------------------------------------------------

pub async fn list(
    State(state): State<AppState>,
    Path(parts): Path<SchemaPathParts>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, parts)?;
    let identity = Identity::anonymous();
    let compiled = build_list(&schema, &q)?;

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
                    "SELECT row_to_json(t.*) AS row FROM",
                    1,
                );
                let mut q = sqlx::query(&select);
                for v in &compiled.params {
                    q = q.bind(v);
                }
                let rows = q.fetch_all(&mut **tx).await?;
                let items: Vec<Value> =
                    rows.into_iter().map(|r| r.get::<Value, _>("row")).collect();
                Ok(items)
            })
        },
    )
    .await?;

    Ok(Json(json!({
        "items": items,
        "cursor": Value::Null,
    })))
}

// ---------- CREATE --------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    Path(parts): Path<SchemaPathParts>,
    Json(payload): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let schema = resolve_schema(&state, parts)?;
    let identity = Identity::anonymous();

    let payload_obj = payload
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("payload must be a JSON object".into()))?
        .clone();

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

    let inserted = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Writer,
        &identity,
        move |tx| {
            Box::pin(async move {
                let row = insert_row(tx, &table, &cols, &casts, &vals).await?;
                if matches!(tier, SearchTier::Tier3) {
                    let id = row["id"].as_str().unwrap_or_default().to_string();
                    write_outbox(tx, &outbox_table, "create", &id, &row).await?;
                }
                Ok(row)
            })
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(inserted)))
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
         RETURNING row_to_json({qualified_table}.*) AS row"
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
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = Identity::anonymous();
    let table = schema.pg_qualified.clone();

    let row = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Reader,
        &identity,
        move |tx| Box::pin(async move { fetch_one_json(tx, &table, &id).await }),
    )
    .await?;

    row.map(Json).ok_or(ApiError::NotFound)
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
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = Identity::anonymous();

    let payload_obj = payload
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("payload must be a JSON object".into()))?
        .clone();

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
    let sql = format!(
        "UPDATE {table} SET {} WHERE id = ${id_idx}::uuid AND version = ${ver_idx} AND deleted_at IS NULL \
         RETURNING row_to_json({table}.*) AS row",
        sets.join(", ")
    );

    let tier = schema.spec.search.tier;
    let outbox_table = format!("{}.{}_outbox", schema.pg_schema, schema.pg_table);

    let updated = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Writer,
        &identity,
        move |tx| {
            Box::pin(async move {
                let mut q = sqlx::query(&sql);
                for v in &vals {
                    q = q.bind(v);
                }
                let result = q.bind(&id).bind(expected_version).fetch_optional(&mut **tx).await?;
                let row = match result {
                    Some(r) => r.get::<Value, _>("row"),
                    None => {
                        // Either no such id, or version mismatch. The handler
                        // wrapper will translate the special sentinel into a
                        // VersionConflict / NotFound via a follow-up probe.
                        let table = qualified_table_from_sql(&sql);
                        let probe_sql =
                            format!("SELECT id FROM {table} WHERE id = $1::uuid LIMIT 1");
                        let exists = sqlx::query(&probe_sql)
                            .bind(&id)
                            .fetch_optional(&mut **tx)
                            .await?;
                        return Err(if exists.is_some() {
                            sqlx::Error::Protocol("__version_conflict__".into())
                        } else {
                            sqlx::Error::RowNotFound
                        });
                    }
                };
                if matches!(tier, SearchTier::Tier3) {
                    write_outbox(tx, &outbox_table, "update", &id, &row).await?;
                }
                Ok(row)
            })
        },
    )
    .await
    .map_err(map_update_err)?;

    Ok(Json(updated))
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
) -> Result<StatusCode, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = Identity::anonymous();
    let table = schema.pg_qualified.clone();
    let tier = schema.spec.search.tier;
    let outbox_table = format!("{}.{}_outbox", schema.pg_schema, schema.pg_table);

    let result = with_session_context(
        &state.pool,
        &schema,
        RoleClass::Admin,
        &identity,
        move |tx| {
            Box::pin(async move {
                let sql = format!(
                    "UPDATE {table} \
                     SET deleted_at = now(), updated_at = now(), \
                         updated_by = current_setting('app.current_user', true) \
                     WHERE id = $1::uuid AND deleted_at IS NULL"
                );
                let result = sqlx::query(&sql).bind(&id).execute(&mut **tx).await?;
                if result.rows_affected() == 0 {
                    return Err(sqlx::Error::RowNotFound);
                }
                if matches!(tier, SearchTier::Tier3) {
                    write_outbox(tx, &outbox_table, "delete", &id, &json!({ "id": id }))
                        .await?;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_table_parsing() {
        let sql = "UPDATE acme_x_y.po_v1 SET foo = $1 WHERE id = $2";
        assert_eq!(qualified_table_from_sql(sql), "acme_x_y.po_v1");
    }
}
