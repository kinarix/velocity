//! Tier-3 search HTTP handlers (relocated from `velocity-api` in the
//! Phase-12a crate-isolation cleanup).
//!
//! Two routes, both Tier-3 / Typesense only — Tier-1/2 callers use the
//! data-API's `POST /query` (with `q` for Tier-2 FTS) instead:
//!
//! - `POST /api/{org}/{app}/{domain}/{object}/{version}/search` — single
//!   schema collection.
//! - `POST /api/{org}/search` — per-org cross-domain collection, filtered
//!   to the schemas the caller has read access on.
//!
//! The shared request helpers (`resolve_schema`, `identity_from_ext`,
//! `audit_if_denied`, …) live in `velocity_core::handler_util`; auth/RBAC,
//! the audit writer, and `ResolvedSchema` all come from the shared core.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::{Extension, Json};
use serde_json::{json, Value};
use velocity_core::audit::{self, action, outcome};
use velocity_core::auth::AuthDecision;
use velocity_core::error::ApiError;
use velocity_core::handler_util::{
    audit_if_denied, identity_from_ext, request_id_from_headers, resolve_schema, SchemaPathParts,
};
use velocity_core::identity::Identity;
use velocity_core::rbac::{check_access, op};
use velocity_types::crds::schema::SearchTier;
use velocity_typesense::SearchParams;

use crate::cdc::{cross_collection_name, schema_collection_name};
use crate::state::SearchState;

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
    State(state): State<SearchState>,
    Path(parts): Path<SchemaPathParts>,
    headers: HeaderMap,
    identity_ext: Option<Extension<Identity>>,
    decision_ext: Option<Extension<AuthDecision>>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<Value>, ApiError> {
    let schema = resolve_schema(&state.registry, parts)?;
    let identity = identity_from_ext(identity_ext);
    let decision = decision_ext.map(|Extension(d)| d);
    let request_id = request_id_from_headers(&headers);
    audit_if_denied(
        &state.pool,
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
        let parts: Vec<String> =
            schema.fields.ordered.iter().filter(|f| f.searchable).map(|f| f.name.clone()).collect();
        if parts.is_empty() {
            "id".to_string() // last-resort: search by id so the call doesn't 400
        } else {
            parts.join(",")
        }
    });

    let params = SearchParams {
        q: req.q,
        query_by,
        filter_by: req.filter_by,
        sort_by: req.sort_by,
        per_page: req.per_page,
        page: req.page,
    };

    let coll = schema_collection_name(&schema);
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
    State(state): State<SearchState>,
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
        if check_access(schema, &identity, op::READ).is_ok() {
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
        allowed_schemas.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(",")
    );
    // Compose with any user-supplied filter_by (AND'd).
    let filter_by = match req.filter_by {
        Some(extra) => Some(format!("{schema_filter} && {extra}")),
        None => Some(schema_filter),
    };

    let params = SearchParams {
        q: req.q,
        query_by: req.query_by.unwrap_or_else(|| "__body,title".into()),
        filter_by,
        sort_by: req.sort_by,
        per_page: req.per_page,
        page: req.page,
    };

    let coll = cross_collection_name(&org);
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
