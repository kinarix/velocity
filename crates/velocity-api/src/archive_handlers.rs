//! Phase 8 slice 9 — Archive API.
//!
//! Three endpoints over the per-domain `*_archive` schema:
//!
//! - `GET /api/{org}/{app}/{domain}/{object}/{version}/{id}/archive`
//!   — fetch the archive copy of a single row by id.
//! - `POST /api/{org}/{app}/{domain}/{object}/{version}/archive/query`
//!   — paginated list with an optional `archivedAfter` floor.
//! - `POST /api/{org}/{app}/{domain}/{object}/{version}/{id}/unarchive`
//!   — clear `archived_at` on the hot row and drop the archive copy.
//!   Only succeeds when the hot row hasn't been purged via the
//!   `purgeAfter` lifecycle; on a missing hot row we 410 with a clear
//!   error code so the caller knows the archive copy is the last
//!   surviving record.
//!
//! Identifier safety: the archive schema/table names are derived from
//! `ResolvedSchema::pg_schema` and `pg_table` — both have already been
//! validated by the operator's `validate_ident` at provision time and
//! by `schema_path_validator::validate` at registry-resolve time. We
//! sanity-check again at the SQL boundary to keep belt and braces.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthDecision;
use crate::error::ApiError;
use crate::handlers::{identity_from_ext, resolve_schema};
use crate::identity::Identity;
use crate::rbac::{check_access, op};
use crate::registry::ResolvedSchema;
use crate::state::AppState;

const MAX_PAGE: i64 = 1000;

fn archive_qualified(schema: &ResolvedSchema) -> Result<String, ApiError> {
    let s = format!("{}_archive", schema.pg_schema);
    let t = &schema.pg_table;
    if !is_safe_ident(&s) || !is_safe_ident(t) {
        return Err(ApiError::Internal(
            "refusing unsafe archive identifier".into(),
        ));
    }
    Ok(format!("{s}.{t}"))
}

fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true)
}

// ── GET /{id}/archive ──────────────────────────────────────────────────────

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
    _headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    _decision_ext: Option<Extension<AuthDecision>>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = identity_from_ext(identity_ext);
    check_access(&schema, &identity, op::READ)?;
    let qualified = archive_qualified(&schema)?;

    let sql = format!(
        "SELECT (to_jsonb(t) - '__fts') AS row FROM {qualified} t \
         WHERE id = $1::uuid"
    );
    let row: Option<Value> = sqlx::query_scalar(&sql)
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(format!("archive get_one: {e}")))?;

    let mut row = row.ok_or(ApiError::NotFound)?;
    schema.field_filter.strip_for_read(&mut row, &identity.roles);
    schema.masking.apply_for_read(&mut row, &identity.roles);
    Ok(Json(row))
}

// ── POST /archive/query ────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveQueryRequest {
    /// Only return rows whose `archived_at >= archived_after` (RFC 3339).
    pub archived_after: Option<String>,
    /// Cap on rows returned. Clamped 1..=1000; default 100.
    pub limit: Option<i64>,
    /// Skip the first `offset` rows. Default 0.
    pub offset: Option<i64>,
}

pub async fn query(
    State(state): State<AppState>,
    Path((org, app, domain, object, version)): Path<(String, String, String, String, String)>,
    _headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    _decision_ext: Option<Extension<AuthDecision>>,
    Json(body): Json<ArchiveQueryRequest>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = identity_from_ext(identity_ext);
    check_access(&schema, &identity, op::READ)?;
    let qualified = archive_qualified(&schema)?;

    let limit = body.limit.unwrap_or(100).clamp(1, MAX_PAGE);
    let offset = body.offset.unwrap_or(0).max(0);

    let (where_clause, bind_after) = match body.archived_after.as_ref() {
        Some(_) => (" WHERE archived_at >= $1::timestamptz", true),
        None => ("", false),
    };
    let limit_param = if bind_after { "$2" } else { "$1" };
    let offset_param = if bind_after { "$3" } else { "$2" };

    let sql = format!(
        "SELECT (to_jsonb(t) - '__fts') AS row FROM {qualified} t{where_clause} \
         ORDER BY archived_at DESC, id LIMIT {limit_param} OFFSET {offset_param}"
    );

    let mut q = sqlx::query_scalar::<_, Value>(&sql);
    if let Some(after) = &body.archived_after {
        q = q.bind(after);
    }
    q = q.bind(limit).bind(offset);

    let rows: Vec<Value> = q
        .fetch_all(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(format!("archive query: {e}")))?;

    let rows: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            schema.field_filter.strip_for_read(&mut r, &identity.roles);
            schema.masking.apply_for_read(&mut r, &identity.roles);
            r
        })
        .collect();

    Ok(Json(json!({
        "items": rows,
        "limit": limit,
        "offset": offset,
    })))
}

// ── POST /{id}/unarchive ───────────────────────────────────────────────────

pub async fn unarchive(
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
    _decision_ext: Option<Extension<AuthDecision>>,
) -> Result<Response, ApiError> {
    let _ = &headers;
    let schema = resolve_schema(&state, (org, app, domain, object, version))?;
    let identity = identity_from_ext(identity_ext);
    check_access(&schema, &identity, op::UPDATE)?;
    let archive_qualified = archive_qualified(&schema)?;
    let hot_qualified = schema.pg_qualified.clone();

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(format!("unarchive begin: {e}")))?;

    // Step 1 — clear the marker on the hot row if it still exists.
    let update_sql = format!(
        "UPDATE {hot_qualified} SET archived_at = NULL, archive_ref = NULL \
         WHERE id = $1::uuid AND archived_at IS NOT NULL \
         RETURNING id"
    );
    let restored: Option<(sqlx::types::Uuid,)> = sqlx::query_as(&update_sql)
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(format!("unarchive update: {e}")))?;

    if restored.is_none() {
        // Either the hot row never existed under this id, or it was already
        // purged. Either way the archive copy is now the last surviving
        // record — refuse the unarchive to surface the lifecycle.
        return Ok((
            StatusCode::GONE,
            Json(json!({
                "code": "ARCHIVE_HOT_ROW_PURGED",
                "message": "the hot row for this id has been purged; \
                            restore-from-archive is not supported via this endpoint",
                "id": id,
            })),
        )
            .into_response());
    }

    // Step 2 — drop the archive copy.
    let del_sql = format!("DELETE FROM {archive_qualified} WHERE id = $1::uuid");
    sqlx::query(&del_sql)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(format!("unarchive delete: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(format!("unarchive commit: {e}")))?;

    Ok((StatusCode::OK, Json(json!({ "id": id, "unarchived": true }))).into_response())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_ident_accepts_valid() {
        assert!(is_safe_ident("acme_sc_proc_archive"));
        assert!(is_safe_ident("purchase_order_v1"));
    }

    #[test]
    fn is_safe_ident_rejects_unsafe() {
        assert!(!is_safe_ident(""));
        assert!(!is_safe_ident("1bad"));
        assert!(!is_safe_ident("a;drop"));
        assert!(!is_safe_ident(&"x".repeat(64)));
    }
}
