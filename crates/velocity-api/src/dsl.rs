//! POST /query DSL — Phase 5a.
//!
//! Wire shape (JSON):
//! ```jsonc
//! {
//!   "where":  { "kind": "and", "children": [ ... ] },
//!   "sort":   [ { "field": "created_at", "desc": true } ],
//!   "select": [ "id", "po_number", "status" ],
//!   "include":[ "supplier_code" ],
//!   "limit":  100,
//!   "cursor": "<opaque>"
//! }
//! ```
//!
//! Every identifier reaching SQL is validated against
//! [`ResolvedSchema.fields`] (or the system-column allowlist) BEFORE
//! emission. Values are always `$N`-bound. Cursors are HMAC-signed so a
//! tampered cursor is rejected before SQL is built.
//!
//! Scope vs the simpler [`crate::query`] list builder:
//!
//! | feature              | `query::ListQuery`   | `dsl::QueryDsl`           |
//! |----------------------|----------------------|---------------------------|
//! | nested AND/OR/NOT    | no                   | yes                       |
//! | operators            | eq/neq/lt/lte/gt/gte | + in/like/contains/null/  |
//! |                      |                      |   between                 |
//! | sort                 | yes                  | yes                       |
//! | select               | * only               | column projection         |
//! | include (cross-schema)| no                  | yes (via `FieldSpec.ref`) |
//! | cursor pagination    | stub                 | HMAC-signed keyset        |
//! | route                | GET ?…               | POST /query               |
//!
//! Anything the DSL doesn't understand is rejected — never silently
//! dropped — so a typo in a field name surfaces as 400 instead of being
//! interpreted as "no filter".

use std::collections::HashSet;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use velocity_types::crds::schema::FieldKind;

use crate::error::ApiError;
use crate::identity::Identity;
use crate::query::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
use crate::rbac::{check_access, op};
use crate::registry::{registry_key, ResolvedSchema, SchemaRegistry};

type HmacSha256 = Hmac<Sha256>;

/// Hard caps — kept small so a malicious payload can't blow up
/// reconcile cost.
pub const MAX_WHERE_DEPTH: usize = 6;
pub const MAX_WHERE_NODES: usize = 64;
pub const MAX_INCLUDES: usize = 5;
pub const MAX_SORT_FIELDS: usize = 4;
pub const MAX_SELECT_FIELDS: usize = 64;

/// System columns that pass the field allowlist for `select` / `sort`
/// even though they aren't user-declared. `id` and timestamps double as
/// natural sort keys for cursor pagination.
const SYSTEM_READ_COLUMNS: &[&str] =
    &["id", "created_at", "updated_at", "version", "created_by", "updated_by"];

fn is_system_read_column(name: &str) -> bool {
    SYSTEM_READ_COLUMNS.contains(&name)
}

/// Postgres type cast for a field's column. Used by the cursor keyset
/// compiler so a tuple comparison like `(t.id, t.created_at) > ($1, $2)`
/// doesn't fail with `operator does not exist: uuid > text` — bound
/// params are text/jsonb by default and Postgres won't infer the cast.
fn pg_cast(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::String | FieldKind::Enum | FieldKind::Ref => "::text",
        FieldKind::Integer => "::bigint",
        FieldKind::Number => "::numeric",
        FieldKind::Boolean => "::boolean",
        FieldKind::Date => "::date",
        FieldKind::Datetime => "::timestamptz",
        FieldKind::Uuid => "::uuid",
        FieldKind::Json => "::jsonb",
    }
}

/// Cast for a system column. `id` is always uuid; timestamp columns
/// timestamptz; version is integer.
fn pg_cast_for_system(name: &str) -> &'static str {
    match name {
        "id" => "::uuid",
        "created_at" | "updated_at" => "::timestamptz",
        "version" => "::bigint",
        _ => "::text",
    }
}

// ─── Wire types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryDsl {
    #[serde(default, rename = "where")]
    pub where_node: Option<WhereNode>,
    #[serde(default)]
    pub sort: Vec<SortField>,
    #[serde(default)]
    pub select: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WhereNode {
    And { children: Vec<WhereNode> },
    Or { children: Vec<WhereNode> },
    Not { child: Box<WhereNode> },
    Cmp {
        field: String,
        op: DslOp,
        #[serde(default)]
        value: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DslOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    /// `value` is an array; expands to `field IN ($1, $2, ...)`.
    In,
    /// `value` is an array; expands to `field NOT IN ($1, $2, ...)`.
    NotIn,
    /// `value` is a string; case-insensitive substring via `position()`
    /// — no LIKE escapes needed because the value is bound as-is.
    Contains,
    /// `value` is a string; explicit LIKE pattern (`%`, `_` honoured).
    Like,
    /// `value` is ignored; `field IS NULL`.
    IsNull,
    /// `value` is ignored; `field IS NOT NULL`.
    IsNotNull,
    /// `value` is a 2-element array; `field BETWEEN $a AND $b`.
    Between,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SortField {
    pub field: String,
    #[serde(default)]
    pub desc: bool,
}

// ─── Cursor (HMAC-signed keyset) ────────────────────────────────────────────

/// Wire envelope encoded inside the cursor. Pinned to the `(path,
/// sort)` shape of the request that produced it — a cursor minted for
/// one query can't be reused on a different schema or different sort
/// signature (which would silently change page order).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorEnvelope {
    /// `registry_key` of the producing schema.
    schema: String,
    /// Stable signature of the sort: `<field>:<asc|desc>` joined by `,`.
    sort_sig: String,
    /// Last row's sort-field values in declared order.
    sort_values: Vec<Value>,
    /// Last row's `id` (always the tiebreaker).
    last_id: String,
}

/// HMAC-SHA256 cursor signer. Configured from
/// `VELOCITY_API_CURSOR_SIGNING_KEY` (≥32 bytes). When unset, the API
/// still serves pages but `next_cursor` is always `null` and a
/// cursor-bearing request returns 400.
#[derive(Debug, Clone)]
pub struct CursorSigner {
    key: Arc<Vec<u8>>,
}

impl CursorSigner {
    /// Construct from a raw key. Returns `Err` if the key is shorter
    /// than 32 bytes — anything smaller is trivially brute-forceable
    /// and the failure must be loud so a misconfigured env doesn't
    /// silently weaken pagination integrity.
    pub fn new(key: Vec<u8>) -> Result<Self, &'static str> {
        if key.len() < 32 {
            return Err("cursor signing key must be at least 32 bytes");
        }
        Ok(Self { key: Arc::new(key) })
    }

    fn encode(&self, env: &CursorEnvelope) -> Result<String, ApiError> {
        let payload =
            serde_json::to_vec(env).map_err(|e| ApiError::Internal(format!("cursor: {e}")))?;
        let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| ApiError::Internal("cursor hmac init".into()))?;
        mac.update(payload_b64.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(format!("{payload_b64}.{sig}"))
    }

    fn decode(&self, cursor: &str) -> Result<CursorEnvelope, ApiError> {
        let (payload_b64, sig_b64) = cursor
            .split_once('.')
            .ok_or_else(|| ApiError::BadRequest("cursor: malformed".into()))?;
        if payload_b64.is_empty() || sig_b64.is_empty() {
            return Err(ApiError::BadRequest("cursor: empty segment".into()));
        }
        let provided = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| ApiError::BadRequest("cursor: bad sig b64".into()))?;
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| ApiError::Internal("cursor hmac init".into()))?;
        mac.update(payload_b64.as_bytes());
        mac.verify_slice(&provided)
            .map_err(|_| ApiError::BadRequest("cursor: bad signature".into()))?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| ApiError::BadRequest("cursor: bad payload b64".into()))?;
        serde_json::from_slice(&payload)
            .map_err(|_| ApiError::BadRequest("cursor: bad payload json".into()))
    }
}

// ─── Compiled output ────────────────────────────────────────────────────────

/// SQL + bound params + everything the handler needs to mint the
/// `next_cursor`. We page by `limit + 1` and let the handler drop the
/// last row + emit a cursor if that overflow row materialised.
#[derive(Debug)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<Value>,
    pub limit: u32,
    /// Plus-one fetch size — handler reads at most this many rows; if
    /// it gets `limit + 1`, more pages exist.
    pub fetch_limit: u32,
    /// Identifies the cursor shape. Handler combines with last row's
    /// values to mint the next cursor.
    pub cursor_sort_sig: String,
    pub cursor_sort_fields: Vec<String>,
    pub schema_key: String,
}

// ─── Compiler ───────────────────────────────────────────────────────────────

/// Build SQL for the DSL. Validates every identifier against
/// `ResolvedSchema.fields`; emits parameterised SQL only. The
/// `registry` is consulted only for `include` resolution — cross-schema
/// RBAC is enforced before any join SQL is emitted.
pub fn build(
    schema: &ResolvedSchema,
    dsl: &QueryDsl,
    identity: &Identity,
    registry: &SchemaRegistry,
    cursor_signer: Option<&CursorSigner>,
) -> Result<CompiledQuery, ApiError> {
    // ── Validate select ──
    if dsl.select.len() > MAX_SELECT_FIELDS {
        return Err(ApiError::BadRequest(format!(
            "select: at most {MAX_SELECT_FIELDS} fields"
        )));
    }
    for f in &dsl.select {
        if !schema.fields.by_name.contains_key(f) && !is_system_read_column(f) {
            return Err(ApiError::UnknownField(f.clone()));
        }
    }

    // ── Validate sort ──
    if dsl.sort.len() > MAX_SORT_FIELDS {
        return Err(ApiError::BadRequest(format!(
            "sort: at most {MAX_SORT_FIELDS} fields"
        )));
    }
    for s in &dsl.sort {
        if !schema.fields.by_name.contains_key(&s.field) && !is_system_read_column(&s.field) {
            return Err(ApiError::UnknownField(s.field.clone()));
        }
        if !is_system_read_column(&s.field) && !schema.fields.sortable.contains(&s.field) {
            return Err(ApiError::NotSortable(s.field.clone()));
        }
    }
    // Multi-sort with mixed direction breaks tuple-comparison cursor
    // semantics. Reject early with a clear error so we never mint a
    // cursor we can't decode back into a correct WHERE clause.
    let mut sort_directions = dsl.sort.iter().map(|s| s.desc);
    let cursor_eligible = match sort_directions.next() {
        None => true,
        Some(first) => sort_directions.all(|d| d == first),
    };
    if !cursor_eligible && dsl.cursor.is_some() {
        return Err(ApiError::BadRequest(
            "cursor: pagination requires uniform sort direction".into(),
        ));
    }

    // ── Validate include ──
    if dsl.include.len() > MAX_INCLUDES {
        return Err(ApiError::BadRequest(format!(
            "include: at most {MAX_INCLUDES} entries"
        )));
    }
    let mut includes: Vec<IncludeJoin> = Vec::with_capacity(dsl.include.len());
    for inc in &dsl.include {
        let join = resolve_include(schema, inc, identity, registry)?;
        includes.push(join);
    }

    // ── Build SELECT list ──
    //
    // We always emit `to_jsonb(...) AS __row` for the main row so the
    // handler has a single, well-known column to read. For explicit
    // projections we wrap `to_jsonb(jsonb_build_object(...))` so the
    // shape is identical regardless of `select` presence — the field
    // filter / masking layer can then strip uniformly.
    let projection = if dsl.select.is_empty() {
        "to_jsonb(t.*) AS __row".to_string()
    } else {
        // Always include `id` so cursor pagination has a tiebreaker
        // and Layer-5 strip can key off it. Build via jsonb_build_object
        // for a stable column shape.
        let mut cols: Vec<String> = dsl.select.clone();
        if !cols.iter().any(|f| f == "id") {
            cols.insert(0, "id".to_string());
        }
        let pairs: Vec<String> =
            cols.iter().map(|f| format!("'{f}', t.{f}")).collect();
        format!("jsonb_build_object({}) AS __row", pairs.join(", "))
    };

    let include_select = includes
        .iter()
        .map(|j| format!(", row_to_json({}.*) AS \"__inc_{}\"", j.alias, j.field_name))
        .collect::<String>();

    // ── Build FROM + joins ──
    //
    // Cast both sides to text in the ON clause: refs are stored as
    // text on the source while target keys may be uuid (or some other
    // type). The cast avoids `operator does not exist: uuid = text`
    // without the compiler needing to know the target column's type.
    // Index usage on the join is a known v1 trade-off — fine for the
    // result sizes the DSL caps to (limit ≤ 1000).
    let mut from = format!("FROM {} t", schema.pg_qualified);
    for j in &includes {
        from.push_str(&format!(
            " LEFT JOIN {} {} ON {}.{}::text = t.{}::text",
            j.target_qualified, j.alias, j.alias, j.target_key, j.field_name
        ));
    }

    // ── Build WHERE ──
    let mut params: Vec<Value> = Vec::new();
    let mut where_parts: Vec<String> = Vec::new();
    where_parts.push("t.deleted_at IS NULL".to_string());

    if let Some(node) = &dsl.where_node {
        let mut depth = 0usize;
        let mut node_count = 0usize;
        let sql = compile_where(node, schema, &mut params, &mut depth, &mut node_count)?;
        if !sql.is_empty() {
            where_parts.push(sql);
        }
    }

    // Row-filter — same Layer-4 invariant as the list builder.
    match schema.row_filter.predicate(&identity.roles, params.len() + 1) {
        Ok(Some(pred)) => {
            where_parts.push(pred.sql);
            params.extend(pred.params);
        }
        Ok(None) => {}
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "rowFilter broken on role `{}`: {}",
                e.role, e.reason
            )));
        }
    }

    // Cursor predicate — keyset.
    let cursor_sort_fields: Vec<String> = if dsl.sort.is_empty() {
        vec!["id".to_string()]
    } else {
        dsl.sort.iter().map(|s| s.field.clone()).collect()
    };
    let cursor_sort_sig = build_sort_sig(&dsl.sort);
    let schema_key = registry_key(&schema.path);

    if let Some(cursor) = &dsl.cursor {
        let signer = cursor_signer.ok_or_else(|| {
            ApiError::BadRequest("cursor pagination is not configured on this server".into())
        })?;
        let env = signer.decode(cursor)?;
        if env.schema != schema_key {
            return Err(ApiError::BadRequest(
                "cursor: schema mismatch".into(),
            ));
        }
        if env.sort_sig != cursor_sort_sig {
            return Err(ApiError::BadRequest(
                "cursor: sort mismatch — re-issue from page 1".into(),
            ));
        }
        if env.sort_values.len() != cursor_sort_fields.len().saturating_sub(
            // `id` is appended below as tiebreaker if not in sort
            usize::from(!dsl.sort.iter().any(|s| s.field == "id")),
        ) && env.sort_values.len() != cursor_sort_fields.len()
        {
            return Err(ApiError::BadRequest("cursor: shape mismatch".into()));
        }

        let cmp = if dsl.sort.first().is_some_and(|s| s.desc) {
            "<"
        } else {
            ">"
        };
        // (a, b, id) > ($1::T, $2::T, $3::uuid)  — uniform direction
        // enforced earlier. Each placeholder is cast to the target
        // column's pg type so Postgres can resolve the tuple compare.
        let mut lhs_cols: Vec<String> =
            cursor_sort_fields.iter().map(|f| format!("t.{f}")).collect();
        let mut rhs_vals: Vec<String> = Vec::with_capacity(env.sort_values.len() + 1);
        for (i, v) in env.sort_values.iter().enumerate() {
            let field_name = cursor_sort_fields.get(i).map(String::as_str).unwrap_or("");
            let cast = if is_system_read_column(field_name) {
                pg_cast_for_system(field_name)
            } else if let Some(fs) = schema.fields.by_name.get(field_name) {
                pg_cast(fs.kind)
            } else {
                // Field was validated earlier; this branch is for
                // safety. Default to no cast.
                ""
            };
            params.push(v.clone());
            rhs_vals.push(format!("${}{}", params.len(), cast));
        }
        // Always tiebreak on id last.
        if !dsl.sort.iter().any(|s| s.field == "id") {
            lhs_cols.push("t.id".to_string());
        }
        params.push(Value::String(env.last_id));
        rhs_vals.push(format!("${}::uuid", params.len()));

        where_parts.push(format!(
            "({}) {} ({})",
            lhs_cols.join(", "),
            cmp,
            rhs_vals.join(", ")
        ));
    }

    // ── Build ORDER BY ──
    let order = if dsl.sort.is_empty() {
        " ORDER BY t.created_at DESC, t.id DESC".to_string()
    } else {
        let mut parts: Vec<String> = dsl
            .sort
            .iter()
            .map(|s| format!("t.{} {}", s.field, if s.desc { "DESC" } else { "ASC" }))
            .collect();
        // Append id tiebreaker matching the dominant direction.
        let dir = if dsl.sort.first().is_some_and(|s| s.desc) {
            "DESC"
        } else {
            "ASC"
        };
        if !dsl.sort.iter().any(|s| s.field == "id") {
            parts.push(format!("t.id {dir}"));
        }
        format!(" ORDER BY {}", parts.join(", "))
    };

    // ── Limit (plus-one fetch) ──
    let limit = dsl.limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let fetch_limit = limit.saturating_add(1).min(MAX_PAGE_SIZE + 1);

    let sql = format!(
        "SELECT {projection}{include_select} {from} WHERE {} {order} LIMIT {fetch_limit}",
        where_parts.join(" AND ")
    );

    Ok(CompiledQuery {
        sql,
        params,
        limit,
        fetch_limit,
        cursor_sort_sig,
        cursor_sort_fields,
        schema_key,
    })
}

/// Mint a `next_cursor` value from the last row of a results page.
/// Caller passes the SECOND-to-last row if it received `limit + 1`
/// rows (the trailing row signals "there's another page" but is not
/// returned to the client).
pub fn mint_cursor(
    signer: &CursorSigner,
    schema_key: &str,
    sort_sig: &str,
    sort_fields: &[String],
    last_row: &Value,
) -> Result<String, ApiError> {
    let obj = last_row
        .as_object()
        .ok_or_else(|| ApiError::Internal("cursor: row is not an object".into()))?;
    let mut vals: Vec<Value> = Vec::with_capacity(sort_fields.len());
    let mut last_id: Option<String> = None;
    for f in sort_fields {
        let v = obj.get(f).cloned().unwrap_or(Value::Null);
        if f == "id" {
            last_id = v.as_str().map(str::to_string);
        }
        vals.push(v);
    }
    if last_id.is_none() {
        last_id = obj.get("id").and_then(|v| v.as_str()).map(str::to_string);
    }
    let last_id = last_id.ok_or_else(|| {
        ApiError::Internal("cursor: row missing string id for tiebreaker".into())
    })?;
    let env = CursorEnvelope {
        schema: schema_key.to_string(),
        sort_sig: sort_sig.to_string(),
        sort_values: vals,
        last_id,
    };
    signer.encode(&env)
}

fn build_sort_sig(sort: &[SortField]) -> String {
    if sort.is_empty() {
        return "_default".to_string();
    }
    sort.iter()
        .map(|s| format!("{}:{}", s.field, if s.desc { "desc" } else { "asc" }))
        .collect::<Vec<_>>()
        .join(",")
}

// ─── WHERE compiler ─────────────────────────────────────────────────────────

fn compile_where(
    node: &WhereNode,
    schema: &ResolvedSchema,
    params: &mut Vec<Value>,
    depth: &mut usize,
    node_count: &mut usize,
) -> Result<String, ApiError> {
    *node_count += 1;
    if *node_count > MAX_WHERE_NODES {
        return Err(ApiError::BadRequest(format!(
            "where: at most {MAX_WHERE_NODES} nodes"
        )));
    }
    *depth += 1;
    if *depth > MAX_WHERE_DEPTH {
        return Err(ApiError::BadRequest(format!(
            "where: nesting deeper than {MAX_WHERE_DEPTH}"
        )));
    }

    let out = match node {
        WhereNode::And { children } => {
            if children.is_empty() {
                "TRUE".to_string()
            } else {
                let mut parts = Vec::with_capacity(children.len());
                for c in children {
                    parts.push(compile_where(c, schema, params, depth, node_count)?);
                }
                format!("({})", parts.join(" AND "))
            }
        }
        WhereNode::Or { children } => {
            if children.is_empty() {
                "FALSE".to_string()
            } else {
                let mut parts = Vec::with_capacity(children.len());
                for c in children {
                    parts.push(compile_where(c, schema, params, depth, node_count)?);
                }
                format!("({})", parts.join(" OR "))
            }
        }
        WhereNode::Not { child } => {
            let inner = compile_where(child, schema, params, depth, node_count)?;
            format!("(NOT ({inner}))")
        }
        WhereNode::Cmp { field, op, value } => compile_cmp(schema, field, *op, value, params)?,
    };

    *depth -= 1;
    Ok(out)
}

fn compile_cmp(
    schema: &ResolvedSchema,
    field: &str,
    op: DslOp,
    value: &Value,
    params: &mut Vec<Value>,
) -> Result<String, ApiError> {
    // `id` and timestamp system columns are always filterable.
    let is_system = is_system_read_column(field);
    if !is_system && !schema.fields.by_name.contains_key(field) {
        return Err(ApiError::UnknownField(field.to_string()));
    }
    if !is_system && !schema.fields.filterable.contains(field) {
        return Err(ApiError::NotFilterable(field.to_string()));
    }

    Ok(match op {
        DslOp::Eq | DslOp::Neq | DslOp::Lt | DslOp::Lte | DslOp::Gt | DslOp::Gte => {
            let sym = match op {
                DslOp::Eq => "=",
                DslOp::Neq => "<>",
                DslOp::Lt => "<",
                DslOp::Lte => "<=",
                DslOp::Gt => ">",
                DslOp::Gte => ">=",
                _ => unreachable!(),
            };
            params.push(value.clone());
            format!("t.{field} {sym} ${}", params.len())
        }
        DslOp::In | DslOp::NotIn => {
            let arr = value.as_array().ok_or_else(|| {
                ApiError::BadRequest(format!("`{field}` {op:?}: value must be an array"))
            })?;
            if arr.is_empty() {
                // IN ()   → never matches; NOT IN () → always matches.
                return Ok(if op == DslOp::In { "FALSE".into() } else { "TRUE".into() });
            }
            if arr.len() > 1000 {
                return Err(ApiError::BadRequest(format!(
                    "`{field}` {op:?}: array length capped at 1000"
                )));
            }
            let mut placeholders = Vec::with_capacity(arr.len());
            for v in arr {
                params.push(v.clone());
                placeholders.push(format!("${}", params.len()));
            }
            let kw = if op == DslOp::In { "IN" } else { "NOT IN" };
            format!("t.{field} {kw} ({})", placeholders.join(", "))
        }
        DslOp::Contains => {
            let _ = value
                .as_str()
                .ok_or_else(|| ApiError::BadRequest("`contains`: value must be a string".into()))?;
            params.push(value.clone());
            // position() returns 0 when not found — safe regardless of
            // characters in the user value; no LIKE-escape needed.
            format!(
                "position(lower(${}) in lower(t.{field}::text)) > 0",
                params.len()
            )
        }
        DslOp::Like => {
            let _ = value
                .as_str()
                .ok_or_else(|| ApiError::BadRequest("`like`: value must be a string".into()))?;
            params.push(value.clone());
            format!("t.{field} ILIKE ${}", params.len())
        }
        DslOp::IsNull => format!("t.{field} IS NULL"),
        DslOp::IsNotNull => format!("t.{field} IS NOT NULL"),
        DslOp::Between => {
            let arr = value.as_array().ok_or_else(|| {
                ApiError::BadRequest("`between`: value must be a 2-element array".into())
            })?;
            if arr.len() != 2 {
                return Err(ApiError::BadRequest(
                    "`between`: value must be a 2-element array".into(),
                ));
            }
            params.push(arr[0].clone());
            let lo = params.len();
            params.push(arr[1].clone());
            let hi = params.len();
            format!("t.{field} BETWEEN ${lo} AND ${hi}")
        }
    })
}

// ─── Include resolution ─────────────────────────────────────────────────────

#[derive(Debug)]
struct IncludeJoin {
    /// The local field name (becomes the result alias too).
    field_name: String,
    /// `pg_schema.pg_table` of the target schema.
    target_qualified: String,
    /// SQL alias for the joined table — distinct per include so two
    /// includes pointing at the same target don't collide.
    alias: String,
    /// Column on the target table the FK resolves to. Comes from
    /// `FieldSpec.ref.key` (defaults to `id`).
    target_key: String,
}

fn resolve_include(
    schema: &ResolvedSchema,
    include_name: &str,
    identity: &Identity,
    registry: &SchemaRegistry,
) -> Result<IncludeJoin, ApiError> {
    let field = schema
        .fields
        .by_name
        .get(include_name)
        .ok_or_else(|| ApiError::UnknownField(include_name.to_string()))?;

    // Must be a ref-typed field (or any field with a `ref` attached).
    let target_ref = field.r#ref.as_ref().ok_or_else(|| {
        ApiError::BadRequest(format!("`{include_name}` is not a reference field"))
    })?;

    // Resolve the target schema. `ObjectRef` is already a full
    // 5-segment coordinate — no inference needed.
    let target_path = velocity_types::common::SchemaPath::new(
        &target_ref.org,
        &target_ref.app,
        &target_ref.domain,
        &target_ref.object,
        &target_ref.version,
    );
    let target = registry.resolve(&target_path).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "include `{include_name}`: target schema `{}` not found",
            registry_key(&target_path)
        ))
    })?;

    // CROSS-SCHEMA RBAC. The identity must hold read on the target — a
    // user with read on purchase_order but not supplier cannot pull
    // supplier rows in via include. Default deny.
    check_access(&target, identity, op::READ).map_err(|_| {
        // Surface as a distinct code so callers can distinguish "I'm
        // not allowed on the target" from "I'm not allowed here".
        ApiError::CrossSchemaAccessDenied(include_name.to_string())
    })?;

    // Validate the target key the ref resolves on. Default is `id`;
    // otherwise it must be a real field on the target schema. Reject
    // wild values rather than emitting SQL that references a missing
    // column.
    let target_key = if target_ref.key == "id" {
        "id".to_string()
    } else if target.fields.by_name.contains_key(&target_ref.key) {
        target_ref.key.clone()
    } else {
        return Err(ApiError::BadRequest(format!(
            "include `{include_name}`: target has no field `{}`",
            target_ref.key
        )));
    };

    let alias = format!("__inc_{}", sanitize_alias(include_name));
    Ok(IncludeJoin {
        field_name: include_name.to_string(),
        target_qualified: target.pg_qualified.clone(),
        alias,
        target_key,
    })
}

fn sanitize_alias(s: &str) -> String {
    // Field names are already CRD-validated (alphanumeric + `_`), but
    // an extra belt-and-braces strip costs nothing.
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

// ─── Field-kind helper (exposed for FTS in 5b) ──────────────────────────────

/// Returns the set of `searchable: true` field names. Lifted out so the
/// FTS handler in 5b can reuse the same view. Phase 5a doesn't call
/// this — but keeping it next to the DSL avoids a second source of
/// truth for "what fields are searchable".
pub fn searchable_field_names(schema: &ResolvedSchema) -> HashSet<String> {
    schema
        .fields
        .ordered
        .iter()
        .filter(|f| f.searchable && matches!(f.kind, FieldKind::String | FieldKind::Enum))
        .map(|f| f.name.clone())
        .collect()
}

// ─── Response helpers ───────────────────────────────────────────────────────

/// Wire shape returned by POST /query.
pub fn build_response(items: Vec<Value>, next_cursor: Option<String>) -> Value {
    json!({
        "items": items,
        "next_cursor": next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::common::SchemaPath;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
        SearchSpec, SearchTier,
    };

    fn id() -> Identity {
        Identity::anonymous()
    }

    fn field(name: &str, kind: FieldKind, filterable: bool, sortable: bool) -> FieldSpec {
        let mut f: FieldSpec =
            serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
        f.kind = kind;
        f.filterable = filterable;
        f.sortable = sortable;
        f
    }

    fn ref_field(name: &str, target_object: &str) -> FieldSpec {
        let mut f = field(name, FieldKind::Ref, true, false);
        f.r#ref = Some(velocity_types::common::ObjectRef {
            org: "acme".into(),
            app: "supply-chain".into(),
            domain: "procurement".into(),
            object: target_object.to_string(),
            version: "v1".into(),
            key: "id".into(),
        });
        f
    }

    fn schema(fields: Vec<FieldSpec>) -> ResolvedSchema {
        let spec = SchemaDefinitionSpec {
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
        };
        ResolvedSchema::from_spec(
            SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1"),
            spec,
        )
    }

    fn fresh_registry() -> Arc<SchemaRegistry> {
        let (reg, _rx) = SchemaRegistry::new();
        reg
    }

    fn key() -> Vec<u8> {
        (b"velocity-cursor-test-key-32-bytes!!!").to_vec()
    }

    #[test]
    fn cursor_signer_rejects_short_key() {
        let err = CursorSigner::new(b"too-short".to_vec()).unwrap_err();
        assert!(err.contains("32 bytes"));
    }

    #[test]
    fn cursor_signer_roundtrips() {
        let s = CursorSigner::new(key()).unwrap();
        let env = CursorEnvelope {
            schema: "a/b/c/d/v1".into(),
            sort_sig: "created_at:desc".into(),
            sort_values: vec![json!("2026-01-01")],
            last_id: "01HF...".into(),
        };
        let tok = s.encode(&env).unwrap();
        let back = s.decode(&tok).unwrap();
        assert_eq!(back.schema, env.schema);
        assert_eq!(back.last_id, env.last_id);
    }

    #[test]
    fn cursor_signer_rejects_tampering() {
        let s = CursorSigner::new(key()).unwrap();
        let env = CursorEnvelope {
            schema: "a/b/c/d/v1".into(),
            sort_sig: "created_at:desc".into(),
            sort_values: vec![json!("2026-01-01")],
            last_id: "01HF...".into(),
        };
        let tok = s.encode(&env).unwrap();
        // Flip one bit of the payload segment.
        let (payload, sig) = tok.split_once('.').unwrap();
        let mut tampered_bytes = URL_SAFE_NO_PAD.decode(payload).unwrap();
        tampered_bytes[0] ^= 0x01;
        let tampered = format!("{}.{}", URL_SAFE_NO_PAD.encode(tampered_bytes), sig);
        let err = s.decode(&tampered).unwrap_err();
        assert!(format!("{err:?}").contains("signature") || format!("{err:?}").contains("json"));
    }

    #[test]
    fn simple_eq_emits_parameterised_sql() {
        let s = schema(vec![field("po_number", FieldKind::String, true, true)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "po_number".into(),
                op: DslOp::Eq,
                value: json!("PO-1"),
            }),
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("t.po_number = $1"));
        assert_eq!(c.params, vec![json!("PO-1")]);
    }

    #[test]
    fn nested_and_or_renders() {
        let s = schema(vec![
            field("status", FieldKind::String, true, false),
            field("amount", FieldKind::Number, true, false),
        ]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::And {
                children: vec![
                    WhereNode::Cmp {
                        field: "status".into(),
                        op: DslOp::Eq,
                        value: json!("approved"),
                    },
                    WhereNode::Or {
                        children: vec![
                            WhereNode::Cmp {
                                field: "amount".into(),
                                op: DslOp::Gt,
                                value: json!(1000),
                            },
                            WhereNode::Cmp {
                                field: "amount".into(),
                                op: DslOp::IsNull,
                                value: Value::Null,
                            },
                        ],
                    },
                ],
            }),
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("AND"));
        assert!(c.sql.contains("OR"));
        assert!(c.sql.contains("IS NULL"));
        assert!(c.sql.contains("amount > $2"));
    }

    #[test]
    fn unknown_field_in_where_rejected() {
        let s = schema(vec![field("po_number", FieldKind::String, true, true)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "ghost".into(),
                op: DslOp::Eq,
                value: json!("x"),
            }),
            ..Default::default()
        };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::UnknownField(_)));
    }

    #[test]
    fn non_filterable_rejected() {
        let s = schema(vec![field("notes", FieldKind::String, false, false)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "notes".into(),
                op: DslOp::Eq,
                value: json!("x"),
            }),
            ..Default::default()
        };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::NotFilterable(_)));
    }

    #[test]
    fn in_expands_to_placeholders() {
        let s = schema(vec![field("status", FieldKind::String, true, false)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "status".into(),
                op: DslOp::In,
                value: json!(["draft", "approved", "shipped"]),
            }),
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("t.status IN ($1, $2, $3)"));
        assert_eq!(c.params.len(), 3);
    }

    #[test]
    fn in_empty_array_short_circuits() {
        let s = schema(vec![field("status", FieldKind::String, true, false)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "status".into(),
                op: DslOp::In,
                value: json!([]),
            }),
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("FALSE"));
        assert!(c.params.is_empty());
    }

    #[test]
    fn between_takes_two_values() {
        let s = schema(vec![field("amount", FieldKind::Number, true, false)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "amount".into(),
                op: DslOp::Between,
                value: json!([100, 500]),
            }),
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("BETWEEN $1 AND $2"));
        assert_eq!(c.params, vec![json!(100), json!(500)]);
    }

    #[test]
    fn between_rejects_wrong_arity() {
        let s = schema(vec![field("amount", FieldKind::Number, true, false)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "amount".into(),
                op: DslOp::Between,
                value: json!([100]),
            }),
            ..Default::default()
        };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn contains_uses_position_not_like() {
        let s = schema(vec![field("notes", FieldKind::String, true, false)]);
        let dsl = QueryDsl {
            where_node: Some(WhereNode::Cmp {
                field: "notes".into(),
                op: DslOp::Contains,
                value: json!("100%"), // would break naive LIKE %x%
            }),
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("position(lower($1)"));
        // The pattern goes into a bind param, not the SQL string.
        assert!(!c.sql.contains("100%"));
    }

    #[test]
    fn nesting_depth_capped() {
        let s = schema(vec![field("x", FieldKind::String, true, false)]);
        // Build MAX_WHERE_DEPTH + 1 levels of AND.
        let mut node = WhereNode::Cmp {
            field: "x".into(),
            op: DslOp::Eq,
            value: json!("v"),
        };
        for _ in 0..(MAX_WHERE_DEPTH + 1) {
            node = WhereNode::And { children: vec![node] };
        }
        let dsl = QueryDsl { where_node: Some(node), ..Default::default() };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn sort_default_appended_with_id_tiebreaker() {
        let s = schema(vec![field("status", FieldKind::String, true, true)]);
        let dsl = QueryDsl {
            sort: vec![SortField { field: "status".into(), desc: false }],
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        assert!(c.sql.contains("ORDER BY t.status ASC, t.id ASC"));
    }

    #[test]
    fn mixed_direction_blocks_cursor() {
        let s = schema(vec![
            field("created_at", FieldKind::Datetime, true, true),
            field("amount", FieldKind::Number, true, true),
        ]);
        let signer = CursorSigner::new(key()).unwrap();
        // Mint a valid-looking cursor first (uniform direction).
        let env = CursorEnvelope {
            schema: registry_key(&SchemaPath::new(
                "acme",
                "supply-chain",
                "procurement",
                "purchase-order",
                "v1",
            )),
            sort_sig: "created_at:desc,amount:asc".into(),
            sort_values: vec![json!("2026-01-01"), json!(100)],
            last_id: "x".into(),
        };
        let tok = signer.encode(&env).unwrap();
        let dsl = QueryDsl {
            sort: vec![
                SortField { field: "created_at".into(), desc: true },
                SortField { field: "amount".into(), desc: false },
            ],
            cursor: Some(tok),
            ..Default::default()
        };
        let err = build(&s, &dsl, &id(), &fresh_registry(), Some(&signer)).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn include_unknown_field_rejected() {
        let s = schema(vec![]);
        let dsl = QueryDsl { include: vec!["supplier".into()], ..Default::default() };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::UnknownField(_)));
    }

    #[test]
    fn include_on_non_ref_field_rejected() {
        let s = schema(vec![field("status", FieldKind::String, true, false)]);
        let dsl = QueryDsl { include: vec!["status".into()], ..Default::default() };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn include_target_schema_missing_rejected() {
        let s = schema(vec![ref_field("supplier_code", "supplier")]);
        let dsl = QueryDsl { include: vec!["supplier_code".into()], ..Default::default() };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn include_target_present_emits_left_join() {
        // Wire up a registry that has both schemas.
        let s = schema(vec![ref_field("supplier_code", "supplier")]);

        // Target: a `supplier` schema in the same org/app/domain.
        let target_path =
            SchemaPath::new("acme", "supply-chain", "procurement", "supplier", "v1");
        let target_spec = SchemaDefinitionSpec {
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
        };
        let target = ResolvedSchema::from_spec(target_path, target_spec);

        let (registry, _rx) = SchemaRegistry::new();
        registry.replace_all(vec![target]);

        let dsl = QueryDsl { include: vec!["supplier_code".into()], ..Default::default() };
        let c = build(&s, &dsl, &id(), &registry, None).unwrap();
        assert!(c.sql.contains("LEFT JOIN"));
        assert!(c.sql.contains("supplier_v1"));
        assert!(c.sql.contains("row_to_json(__inc_supplier_code"));
    }

    #[test]
    fn select_appends_id_for_cursor() {
        let s = schema(vec![field("po_number", FieldKind::String, true, true)]);
        let dsl = QueryDsl {
            select: vec!["po_number".into()],
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap();
        // Projection wraps into jsonb_build_object; `id` is prepended
        // so cursor minting always has a tiebreaker.
        assert!(c.sql.contains("jsonb_build_object('id', t.id, 'po_number', t.po_number)"));
    }

    #[test]
    fn cursor_required_when_set_but_signer_absent() {
        let s = schema(vec![]);
        let dsl = QueryDsl {
            cursor: Some("any.thing".into()),
            ..Default::default()
        };
        let err = build(&s, &dsl, &id(), &fresh_registry(), None).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn mint_cursor_round_trips_with_compiled_keyset() {
        let s = schema(vec![field("created_at", FieldKind::Datetime, true, true)]);
        let signer = CursorSigner::new(key()).unwrap();
        let dsl = QueryDsl {
            sort: vec![SortField { field: "created_at".into(), desc: true }],
            ..Default::default()
        };
        let c = build(&s, &dsl, &id(), &fresh_registry(), Some(&signer)).unwrap();
        let row = json!({
            "id": "01HF...",
            "created_at": "2026-01-01T00:00:00Z",
        });
        let tok = mint_cursor(
            &signer,
            &c.schema_key,
            &c.cursor_sort_sig,
            &c.cursor_sort_fields,
            &row,
        )
        .unwrap();

        // Round-trip: now use the cursor on the next request.
        let dsl2 = QueryDsl {
            sort: vec![SortField { field: "created_at".into(), desc: true }],
            cursor: Some(tok),
            ..Default::default()
        };
        let c2 = build(&s, &dsl2, &id(), &fresh_registry(), Some(&signer)).unwrap();
        // The compiled SQL must reference the keyset comparison.
        assert!(c2.sql.contains("(t.created_at, t.id) <"));
        // And each placeholder must carry its column-type cast so the
        // tuple compare doesn't fail with "operator does not exist".
        assert!(c2.sql.contains("::timestamptz"));
        assert!(c2.sql.contains("::uuid"));
    }

}
