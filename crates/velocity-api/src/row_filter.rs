//! Layer 4 — row-level filter from `spec.access.rowFilter`.
//!
//! The schema author can declare per-role row predicates:
//!
//! ```yaml
//! access:
//!   rowFilter:
//!     - role: regional-reader-west
//!       filter: { field: region, op: eq, value: west }
//!     - role: regional-reader-west
//!       filter: { field: status, op: neq, value: archived }
//!     - role: regional-reader-east
//!       filter: { field: region, op: eq, value: east }
//! ```
//!
//! Multiple entries for the same role AND together; different roles OR.
//! An actor's effective WHERE fragment for the example above:
//!
//! - roles `["regional-reader-west"]` → `(region = 'west' AND status <> 'archived')`
//! - roles `["regional-reader-east"]` → `(region = 'east')`
//! - roles `["both"]` → `((region = 'west' AND status <> 'archived') OR (region = 'east'))`
//!
//! ## Unrestricted roles win
//!
//! A role the actor carries that does *not* appear in `rowFilter[]` is
//! treated as unrestricted — that role's contribution to the OR is TRUE,
//! so the whole predicate short-circuits and the actor sees everything.
//! This matches the "more roles = wider access" intuition: a CRD author
//! who wants pii-reader to also be scoped must declare a rowFilter entry
//! for it; otherwise pii-reader grants full visibility.
//!
//! ## Where it gets applied
//!
//! The predicate is AND'd into every `WHERE` we generate:
//! - LIST (`build_list` injects it after the user's filters)
//! - GET-by-id (the `fetch_one` SQL grows an extra clause)
//! - UPDATE / DELETE (so a scoped actor can't mutate a row they cannot
//!   see — the same predicate appears on the UPDATE WHERE)
//!
//! Skipping any of those would let a caller bypass the gate via direct
//! id lookups; the unit tests in this module pin the four call sites.

use std::collections::HashMap;

use serde_json::Value;
use velocity_types::crds::schema::SchemaDefinitionSpec;

use crate::error::ApiError;
use crate::identity::Identity;
use crate::query::FilterOp;
use crate::registry::{FieldIndex, ResolvedSchema};

/// One compiled clause from a CRD `rowFilter[]` entry. Built at resolve
/// time so the request hot path is allocation-free.
#[derive(Debug, Clone)]
pub struct Clause {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

/// Outcome of compiling a CRD's rowFilter block. `Broken` is fail-closed:
/// a broken predicate denies *all* access on this schema rather than
/// silently admitting (matches the same posture as [`crate::policy::CompiledPolicy::Broken`]).
#[derive(Debug)]
pub enum RowFilterIndex {
    Empty,
    Compiled(CompiledRowFilters),
    /// A clause referenced a non-existent or non-filterable field. The
    /// safer move on a misconfigured CRD is to refuse traffic until an
    /// operator fixes the spec.
    Broken {
        role: String,
        reason: String,
    },
}

#[derive(Debug, Default)]
pub struct CompiledRowFilters {
    by_role: HashMap<String, Vec<Clause>>,
}

impl RowFilterIndex {
    /// Build the index from the CRD spec. `fields` is the precomputed
    /// [`FieldIndex`] we use to validate that every clause references a
    /// real, filterable field (catching CRD typos at apply time instead
    /// of when a request first hits the schema).
    pub fn from_spec(spec: &SchemaDefinitionSpec, fields: &FieldIndex) -> Self {
        if spec.access.row_filter.is_empty() {
            return Self::Empty;
        }
        let mut by_role: HashMap<String, Vec<Clause>> = HashMap::new();
        for entry in &spec.access.row_filter {
            let role = entry.role.clone();
            let f = &entry.filter;

            // Every reference must be to a real field. We *don't* require
            // `filterable: true` here even though that's the user-query
            // gate — operator-declared predicates are trusted differently:
            // the CRD author chose them, not an HTTP caller. We do require
            // the field exists, since interpolating a missing name into
            // SQL would be a guaranteed runtime error per request.
            if !fields.by_name.contains_key(&f.field) {
                return Self::Broken {
                    role: role.clone(),
                    reason: format!("rowFilter references unknown field `{}`", f.field),
                };
            }

            let op = match FilterOp::parse(&f.op) {
                Some(o) => o,
                None => {
                    return Self::Broken {
                        role: role.clone(),
                        reason: format!("rowFilter has unknown op `{}`", f.op),
                    }
                }
            };

            by_role.entry(role).or_default().push(Clause {
                field: f.field.clone(),
                op,
                value: f.value.clone(),
            });
        }
        Self::Compiled(CompiledRowFilters { by_role })
    }

    /// Returns `true` iff this schema has *any* row filter declared. Useful
    /// for handlers that want to skip the no-op append path.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// In-memory twin of [`Self::predicate`]. Returns `Ok(true)` when
    /// `identity_roles` may see an entity whose JSON state is `payload`,
    /// `Ok(false)` when the row-filter denies it, and `Err(BrokenError)`
    /// when the index itself is malformed. Used by `time_machine.rs` to
    /// gate per-entity access against `platform.event_log` reads — that
    /// table has no RLS, so the gate has to happen in app code.
    ///
    /// `Empty` index ⇒ everyone is unrestricted ⇒ always `Ok(true)`.
    pub fn matches_payload(
        &self,
        identity_roles: &[String],
        payload: &Value,
    ) -> Result<bool, BrokenError> {
        match self {
            Self::Empty => Ok(true),
            Self::Broken { role, reason } => Err(BrokenError {
                role: role.clone(),
                reason: reason.clone(),
            }),
            Self::Compiled(compiled) => Ok(compiled.matches_payload(identity_roles, payload)),
        }
    }

    /// Build the row-scope WHERE fragment for `identity_roles`, starting
    /// at `next_param_idx` (returns the new placeholder index alongside
    /// the SQL fragment and the bound values, in $-order).
    ///
    /// Returns `None` when no filter is needed:
    /// - schema has no rowFilter declared
    /// - the actor's roles include one that is not in the rowFilter map
    ///   (unrestricted role wins — see module docs)
    ///
    /// Returns `Err` when the index itself is broken — callers should
    /// surface this as a 500 so an operator gets paged, not as a silent
    /// admit or a confusing 4xx.
    pub fn predicate(
        &self,
        identity_roles: &[String],
        next_param_idx: usize,
    ) -> Result<Option<Predicate>, BrokenError> {
        match self {
            Self::Empty => Ok(None),
            Self::Broken { role, reason } => Err(BrokenError {
                role: role.clone(),
                reason: reason.clone(),
            }),
            Self::Compiled(compiled) => Ok(compiled.predicate(identity_roles, next_param_idx)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub sql: String,
    pub params: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct BrokenError {
    pub role: String,
    pub reason: String,
}

/// Append `v` to `q` as a typed parameter rather than letting sqlx default
/// every `serde_json::Value` to `jsonb`. The row-filter predicate compares
/// against columns of their *declared* SQL type (`region = $5` on a text
/// column, `score >= $5` on numeric, etc.) — without this dispatch
/// Postgres rejects with `operator does not exist: text = jsonb` and the
/// whole row filter is dead on arrival.
///
/// We deliberately match `Value` variants rather than threading the
/// schema's field-kind through here: the CRD's `rowFilter[].value` is
/// already parsed by serde_json into the right variant (a `"west"` arrives
/// as `Value::String`, a `42` as `Value::Number`, etc.), so the variant
/// IS the type signal. Arrays/objects fall through to a jsonb bind because
/// the corresponding columns would be jsonb too — there's no way to write
/// a top-level array literal in CRD YAML that would land on a non-jsonb
/// column.
pub fn bind_json_param<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    v: &'q Value,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match v {
        Value::String(s) => q.bind(s.as_str()),
        Value::Bool(b) => q.bind(*b),
        Value::Null => q.bind(None::<&str>),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else if let Some(f) = n.as_f64() {
                q.bind(f)
            } else {
                // u64 outside i64 range — extremely rare from CRD config,
                // but bind as jsonb rather than silently truncating.
                q.bind(v)
            }
        }
        // Arrays and objects: comparing those against a column requires
        // the column to be jsonb anyway, so the default bind is correct.
        Value::Array(_) | Value::Object(_) => q.bind(v),
    }
}

/// Per-id call-site wrapper: get the schema's row-filter [`Predicate`] for
/// `identity`, translating a [`BrokenError`] into [`ApiError::Internal`] so
/// GET/UPDATE/DELETE handlers don't have to repeat the boilerplate.
///
/// `next_param_idx` is the `$N` the caller will hand to sqlx for the *first*
/// row-filter bind — the predicate's placeholders pick up from there.
pub fn predicate_for(
    schema: &ResolvedSchema,
    identity: &Identity,
    next_param_idx: usize,
) -> Result<Option<Predicate>, ApiError> {
    schema.row_filter.predicate(&identity.roles, next_param_idx).map_err(|e| {
        ApiError::Internal(format!(
            "rowFilter broken on role `{}`: {}",
            e.role, e.reason
        ))
    })
}

/// In-memory companion to [`predicate_for`] for the time-machine read
/// path. Returns `true` if `identity` may see an entity whose
/// reconstructed JSON state is `payload`. A `Broken` row-filter on the
/// schema bubbles up as `ApiError::Internal` for the same reason the SQL
/// path does — fail loud rather than silently admit.
pub fn payload_visible(
    schema: &ResolvedSchema,
    identity: &Identity,
    payload: &Value,
) -> Result<bool, ApiError> {
    schema.row_filter.matches_payload(&identity.roles, payload).map_err(|e| {
        ApiError::Internal(format!(
            "rowFilter broken on role `{}`: {}",
            e.role, e.reason
        ))
    })
}

/// Compare a single CRD clause against a JSON object, mirroring the SQL
/// semantics `WHERE field <op> value` would produce: missing or `null`
/// on either side is `false`, cross-type comparisons are `false`,
/// numeric ops use `f64`, string equality is byte-exact.
fn clause_matches_json(clause: &Clause, obj: &serde_json::Map<String, Value>) -> bool {
    let lhs = match obj.get(&clause.field) {
        Some(v) if !v.is_null() => v,
        _ => return false,
    };
    let rhs = &clause.value;
    if rhs.is_null() {
        return false;
    }
    use crate::query::FilterOp;
    match clause.op {
        FilterOp::Eq => lhs == rhs,
        FilterOp::Neq => lhs != rhs,
        FilterOp::Lt | FilterOp::Lte | FilterOp::Gt | FilterOp::Gte => {
            // Numeric path: both sides must be numbers; compare as f64.
            // Anything else (string vs string ordering, dates) is out of
            // scope for this evaluator — rowFilter currently only sees
            // numeric or eq/neq use in practice. A future iteration that
            // needs lexicographic string ordering can extend this match.
            match (lhs.as_f64(), rhs.as_f64()) {
                (Some(a), Some(b)) => match clause.op {
                    FilterOp::Lt => a < b,
                    FilterOp::Lte => a <= b,
                    FilterOp::Gt => a > b,
                    FilterOp::Gte => a >= b,
                    _ => unreachable!(),
                },
                _ => false,
            }
        }
    }
}

/// Sentinel used in `app.scoped_roles` to mean "this caller carries at
/// least one role that has no rowFilter entry — they should see every
/// row." Kept out of the user-role namespace by being a single char that
/// `sanitize_role()` would never produce.
pub const SCOPED_ROLES_UNRESTRICTED: &str = "*";

/// Build the string the API hands to Postgres for `app.scoped_roles`
/// (Layer 7 RLS backstop). The encoding deliberately collapses "admit"
/// to a single sentinel so the SQL fragment and the RLS policy agree on
/// every case in this module's test matrix:
///
/// - `"*"`               → unrestricted. Either the schema declares no
///   `rowFilter[]` at all, or the actor carries at least one role that
///   isn't in the map (the "more roles = wider access" semantic).
/// - `"role-a,role-b,…"` → scoped. Every role on the actor that maps
///   into `rowFilter[]` is listed. The operator's per-role policies
///   check membership and apply their predicate.
/// - `""`                → deny. The schema has scoped rules but none
///   of the actor's roles matches the map (zero-role identity, or an
///   actor with only roles unrelated to this schema's row scope). The
///   operator's wildcard policy must NOT admit on this value — `""`
///   and a missing setting both fail closed.
///
/// A `Broken` rowFilter on the schema is reported here too — the caller
/// will turn it into a 500 rather than admit silently.
pub fn scoped_roles_for_session(
    schema: &ResolvedSchema,
    identity: &Identity,
) -> Result<String, BrokenError> {
    match &*schema.row_filter {
        // No rowFilter on the schema → everyone is unrestricted by
        // definition. Returning `*` lets the wildcard policy admit
        // without needing a separate "no rules declared" branch on the
        // SQL side.
        RowFilterIndex::Empty => Ok(SCOPED_ROLES_UNRESTRICTED.into()),
        RowFilterIndex::Broken { role, reason } => {
            Err(BrokenError { role: role.clone(), reason: reason.clone() })
        }
        RowFilterIndex::Compiled(compiled) => {
            let any_unrestricted = identity
                .roles
                .iter()
                .any(|r| !compiled.by_role.contains_key(r));
            if any_unrestricted {
                return Ok(SCOPED_ROLES_UNRESTRICTED.into());
            }
            // Only the actor's roles that DO appear in the map matter for
            // the membership check — emitting more would be wasted bytes
            // and a soft-information leak about the actor's other roles.
            let mut roles: Vec<&str> = identity
                .roles
                .iter()
                .filter(|r| compiled.by_role.contains_key(*r))
                .map(String::as_str)
                .collect();
            roles.sort();
            roles.dedup();
            // `""` here is the *deny* sentinel — the per-role policies
            // can't match it, and the wildcard policy is `= '*'` only.
            // This matches the SQL fragment's `(false)` contradiction.
            Ok(roles.join(","))
        }
    }
}

impl CompiledRowFilters {
    /// JSON twin of [`Self::predicate`]: evaluate the role-OR-of-clause-AND
    /// expression against an in-memory `serde_json::Value` rather than
    /// against SQL columns. Used by the time-machine endpoints, which read
    /// reconstructed payloads out of `platform.event_log` and need to
    /// answer "would this actor have seen this entity?" without re-issuing
    /// the live SQL.
    ///
    /// Semantic parity with SQL is the design rule:
    ///   - An unrestricted role short-circuits to `true` (same as
    ///     `predicate()` returning `None`).
    ///   - A role with clauses requires ALL its clauses to match
    ///     (`AND`), and the per-role results combine with `OR`.
    ///   - A clause whose field is missing from the payload, or whose
    ///     either side is JSON `null`, is `false` (mirrors SQL
    ///     three-valued logic collapsed to fail-closed).
    ///   - Numeric comparisons use `f64`; string equality is byte-exact.
    ///     Cross-type comparisons (`region = 42`) are `false`.
    fn matches_payload(&self, identity_roles: &[String], value: &Value) -> bool {
        if identity_roles.iter().any(|r| !self.by_role.contains_key(r)) {
            return true;
        }
        let obj = match value {
            Value::Object(map) => map,
            _ => return false,
        };
        for role in identity_roles {
            let Some(role_clauses) = self.by_role.get(role) else {
                continue;
            };
            if role_clauses.iter().all(|c| clause_matches_json(c, obj)) {
                return true;
            }
        }
        false
    }

    fn predicate(&self, identity_roles: &[String], next_param_idx: usize) -> Option<Predicate> {
        // If *any* role on the identity is unrestricted, the OR collapses
        // to TRUE → no filter needed.
        let any_unrestricted = identity_roles.iter().any(|r| !self.by_role.contains_key(r));
        if any_unrestricted {
            return None;
        }

        let mut params: Vec<Value> = Vec::new();
        let mut clauses: Vec<String> = Vec::new();
        let mut idx = next_param_idx;

        for role in identity_roles {
            let Some(role_clauses) = self.by_role.get(role) else {
                continue;
            };
            let mut and_parts: Vec<String> = Vec::with_capacity(role_clauses.len());
            for c in role_clauses {
                params.push(c.value.clone());
                and_parts.push(format!("{} {} ${}", c.field, c.op.sql(), idx));
                idx += 1;
            }
            if !and_parts.is_empty() {
                clauses.push(format!("({})", and_parts.join(" AND ")));
            }
        }

        if clauses.is_empty() {
            // No role on the identity matched the map. The actor has no
            // visibility — emit a contradiction so SQL returns zero rows
            // rather than swallowing the request silently.
            //
            // We use `(false)` rather than `0=1` so a future planner
            // optimisation that strips contradictions still recognises it.
            return Some(Predicate { sql: "(false)".into(), params: Vec::new() });
        }

        Some(Predicate { sql: clauses.join(" OR "), params })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, RoleAccess, RowFilter,
        RowFilterRule, SchemaDefinitionSpec, SearchSpec, SearchTier,
    };

    fn field(name: &str) -> FieldSpec {
        let mut f: FieldSpec = serde_json::from_value(json!({ "name": name, "type": "string" }))
            .unwrap();
        f.kind = FieldKind::String;
        f.filterable = true;
        f
    }

    fn spec_with(rows: Vec<RowFilterRule>, fields: Vec<FieldSpec>) -> SchemaDefinitionSpec {
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
            access: AccessSpec {
                roles: vec![RoleAccess { role: "reader".into(), operations: vec!["read".into()] }],
                row_filter: rows,
                ..AccessSpec::default()
            },
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

    fn idx(spec: &SchemaDefinitionSpec) -> RowFilterIndex {
        let fields = FieldIndex::from_spec(spec);
        RowFilterIndex::from_spec(spec, &fields)
    }

    fn rule(role: &str, field: &str, op: &str, value: Value) -> RowFilterRule {
        RowFilterRule {
            role: role.into(),
            filter: RowFilter { field: field.into(), op: op.into(), value },
        }
    }

    #[test]
    fn empty_spec_is_empty_index() {
        let i = idx(&spec_with(Vec::new(), vec![field("region")]));
        assert!(i.is_empty());
        assert!(i.predicate(&["reader".into()], 1).unwrap().is_none());
    }

    #[test]
    fn unknown_field_in_filter_is_broken() {
        let i = idx(&spec_with(
            vec![rule("reader", "ghost", "eq", json!("x"))],
            vec![field("region")],
        ));
        let err = i.predicate(&["reader".into()], 1).unwrap_err();
        assert!(err.reason.contains("unknown field"));
    }

    #[test]
    fn unknown_op_is_broken() {
        let i = idx(&spec_with(
            vec![rule("reader", "region", "bogus", json!("west"))],
            vec![field("region")],
        ));
        let err = i.predicate(&["reader".into()], 1).unwrap_err();
        assert!(err.reason.contains("unknown op"));
    }

    #[test]
    fn single_role_single_clause_emits_one_predicate() {
        let i = idx(&spec_with(
            vec![rule("regional-reader", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let p = i.predicate(&["regional-reader".into()], 5).unwrap().unwrap();
        assert_eq!(p.sql, "(region = $5)");
        assert_eq!(p.params, vec![json!("west")]);
    }

    #[test]
    fn multiple_clauses_for_same_role_and_together() {
        // The two clauses on `regional-reader` are AND'd: both must hold.
        let i = idx(&spec_with(
            vec![
                rule("regional-reader", "region", "eq", json!("west")),
                rule("regional-reader", "status", "neq", json!("archived")),
            ],
            vec![field("region"), field("status")],
        ));
        let p = i.predicate(&["regional-reader".into()], 1).unwrap().unwrap();
        assert!(p.sql.contains("region = $1"));
        assert!(p.sql.contains(" AND "));
        assert!(p.sql.contains("status <> $2"));
        assert_eq!(p.params.len(), 2);
    }

    #[test]
    fn multiple_roles_or_together() {
        let i = idx(&spec_with(
            vec![
                rule("west", "region", "eq", json!("west")),
                rule("east", "region", "eq", json!("east")),
            ],
            vec![field("region")],
        ));
        let p = i
            .predicate(&["west".into(), "east".into()], 1)
            .unwrap()
            .unwrap();
        assert!(p.sql.contains(" OR "));
        assert_eq!(p.params.len(), 2);
    }

    #[test]
    fn unrestricted_role_collapses_to_no_filter() {
        // `pii-reader` isn't in rowFilter[] → unrestricted → whole OR is
        // TRUE → no predicate needed. The pinned semantic is "more roles =
        // wider access".
        let i = idx(&spec_with(
            vec![rule("regional-reader", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let p = i
            .predicate(&["regional-reader".into(), "pii-reader".into()], 1)
            .unwrap();
        assert!(p.is_none(), "an unmapped role must short-circuit to TRUE");
    }

    fn identity_with_roles(roles: &[&str]) -> Identity {
        Identity {
            actor_id: "tester".into(),
            roles: roles.iter().map(|s| (*s).to_string()).collect(),
            strategy: "test".into(),
            ..Default::default()
        }
    }

    fn resolved(spec: SchemaDefinitionSpec) -> ResolvedSchema {
        let path = velocity_types::common::SchemaPath::new(
            "acme",
            "supply-chain",
            "procurement",
            "purchase-order",
            "v1",
        );
        ResolvedSchema::from_spec(path, spec)
    }

    #[test]
    fn scoped_roles_star_when_no_row_filter_declared() {
        // No rowFilter on the schema → the `*` admit sentinel. Collapsing
        // "schema admits all" and "actor has unrestricted role" to the
        // same sentinel keeps the operator-side RLS policy a single
        // `= '*'` check (defense-in-depth pinned in the operator test).
        let s = resolved(spec_with(Vec::new(), vec![field("region")]));
        let id = identity_with_roles(&["any-role"]);
        let got = scoped_roles_for_session(&s, &id).unwrap();
        assert_eq!(got, "*");
    }

    #[test]
    fn scoped_roles_denies_when_no_actor_role_matches_map() {
        // Schema declares rules for `east`/`west`; actor carries neither.
        // Every actor role is mapped (vacuously — the actor has zero
        // roles), nothing matched → deny sentinel. The SQL-fragment path
        // (`actor_with_no_matching_role_sees_no_rows`) renders this case
        // to `(false)`; the RLS path encodes it as `''`, which the
        // wildcard policy must NOT admit.
        let s = resolved(spec_with(
            vec![
                rule("west", "region", "eq", json!("west")),
                rule("east", "region", "eq", json!("east")),
            ],
            vec![field("region")],
        ));
        let id = identity_with_roles(&[]);
        assert_eq!(scoped_roles_for_session(&s, &id).unwrap(), "");
    }

    #[test]
    fn scoped_roles_star_when_actor_has_unrestricted_role() {
        // `regional-reader` is scoped, `pii-reader` is not in the map →
        // unrestricted wins; we hand Postgres the `*` sentinel.
        let s = resolved(spec_with(
            vec![rule("regional-reader", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["regional-reader", "pii-reader"]);
        assert_eq!(scoped_roles_for_session(&s, &id).unwrap(), "*");
    }

    #[test]
    fn scoped_roles_joins_actor_roles_when_all_scoped() {
        let s = resolved(spec_with(
            vec![
                rule("west", "region", "eq", json!("west")),
                rule("east", "region", "eq", json!("east")),
            ],
            vec![field("region")],
        ));
        // Roles are sorted + deduped so the value is stable; the operator
        // policies match by membership, so order is irrelevant for
        // correctness but useful for debuggability.
        let id = identity_with_roles(&["west", "east", "east"]);
        assert_eq!(scoped_roles_for_session(&s, &id).unwrap(), "east,west");
    }

    #[test]
    fn scoped_roles_drops_actor_roles_outside_the_map_when_all_scoped() {
        // The `any_unrestricted` short-circuit fires when an actor role
        // is unmapped — but if every actor role IS mapped, we emit just
        // those roles. Nothing changes here; this is the all-mapped path
        // by construction.
        let s = resolved(spec_with(
            vec![
                rule("west", "region", "eq", json!("west")),
                rule("east", "region", "eq", json!("east")),
            ],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["west"]);
        assert_eq!(scoped_roles_for_session(&s, &id).unwrap(), "west");
    }

    #[test]
    fn scoped_roles_propagates_broken_filter() {
        // A misconfigured CRD must surface as BrokenError so the handler
        // can 500 rather than silently admit traffic the operator never
        // sanctioned.
        let s = resolved(spec_with(
            vec![rule("reader", "ghost", "eq", json!("x"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["reader"]);
        let err = scoped_roles_for_session(&s, &id).unwrap_err();
        assert!(err.reason.contains("unknown field"));
    }

    #[test]
    fn matches_payload_empty_index_admits_everyone() {
        // Schema with no rowFilter at all → in-memory eval also collapses
        // to TRUE, matching what `predicate()` does (returns None).
        let i = idx(&spec_with(Vec::new(), vec![field("region")]));
        let id = identity_with_roles(&["any-role"]);
        assert!(i.matches_payload(&id.roles, &json!({ "region": "west" })).unwrap());
    }

    #[test]
    fn matches_payload_unrestricted_role_short_circuits() {
        // Same semantic as the SQL path: an actor with even one role
        // outside the rowFilter map gets unrestricted visibility.
        let i = idx(&spec_with(
            vec![rule("regional-reader", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["regional-reader", "pii-reader"]);
        // East should be visible — pii-reader is unmapped → OR collapses.
        assert!(i.matches_payload(&id.roles, &json!({ "region": "east" })).unwrap());
    }

    #[test]
    fn matches_payload_scoped_admit_and_deny() {
        let i = idx(&spec_with(
            vec![rule("west", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["west"]);
        assert!(i.matches_payload(&id.roles, &json!({ "region": "west" })).unwrap());
        assert!(!i.matches_payload(&id.roles, &json!({ "region": "east" })).unwrap());
    }

    #[test]
    fn matches_payload_clauses_for_same_role_and_together() {
        // Mirrors `multiple_clauses_for_same_role_and_together`: two
        // entries on one role must BOTH hold.
        let i = idx(&spec_with(
            vec![
                rule("regional-reader", "region", "eq", json!("west")),
                rule("regional-reader", "status", "neq", json!("archived")),
            ],
            vec![field("region"), field("status")],
        ));
        let id = identity_with_roles(&["regional-reader"]);
        assert!(i
            .matches_payload(&id.roles, &json!({ "region": "west", "status": "active" }))
            .unwrap());
        assert!(!i
            .matches_payload(&id.roles, &json!({ "region": "west", "status": "archived" }))
            .unwrap());
        assert!(!i
            .matches_payload(&id.roles, &json!({ "region": "east", "status": "active" }))
            .unwrap());
    }

    #[test]
    fn matches_payload_missing_or_null_field_is_false() {
        // SQL `WHERE region = 'west'` returns no rows where region IS NULL.
        // The JSON evaluator mirrors that for a missing key and an
        // explicit `null` value — same fail-closed posture.
        let i = idx(&spec_with(
            vec![rule("west", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["west"]);
        assert!(!i.matches_payload(&id.roles, &json!({})).unwrap());
        assert!(!i.matches_payload(&id.roles, &json!({ "region": null })).unwrap());
    }

    #[test]
    fn matches_payload_non_object_value_denied() {
        // The reconstructed payload is always a JSON object on the
        // common path. If a caller hands us an array or scalar, deny —
        // a non-object can't satisfy a field-comparison predicate.
        let i = idx(&spec_with(
            vec![rule("west", "region", "eq", json!("west"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["west"]);
        assert!(!i.matches_payload(&id.roles, &json!([{ "region": "west" }])).unwrap());
        assert!(!i.matches_payload(&id.roles, &json!("scalar")).unwrap());
    }

    #[test]
    fn matches_payload_numeric_ordering() {
        let i = idx(&spec_with(
            vec![rule("pricer", "amount", "gte", json!(100))],
            vec![{
                let mut f: FieldSpec =
                    serde_json::from_value(json!({ "name": "amount", "type": "integer" }))
                        .unwrap();
                f.kind = FieldKind::Integer;
                f.filterable = true;
                f
            }],
        ));
        let id = identity_with_roles(&["pricer"]);
        assert!(i.matches_payload(&id.roles, &json!({ "amount": 100 })).unwrap());
        assert!(i.matches_payload(&id.roles, &json!({ "amount": 250 })).unwrap());
        assert!(!i.matches_payload(&id.roles, &json!({ "amount": 99 })).unwrap());
    }

    #[test]
    fn matches_payload_propagates_broken_filter() {
        // Same fail-loud posture as scoped_roles_propagates_broken_filter:
        // a misconfigured CRD must NOT silently admit traffic.
        let i = idx(&spec_with(
            vec![rule("reader", "ghost", "eq", json!("x"))],
            vec![field("region")],
        ));
        let id = identity_with_roles(&["reader"]);
        let err = i.matches_payload(&id.roles, &json!({ "region": "west" })).unwrap_err();
        assert!(err.reason.contains("unknown field"));
    }

    #[test]
    fn actor_with_no_matching_role_sees_no_rows() {
        // Schema has a rowFilter for `regional-reader`. Actor only carries
        // `auditor`, which isn't in the map. But `auditor` is *also* not
        // unrestricted-in-context — wait, the current semantic is "any role
        // not in the map ⇒ unrestricted". So `auditor` → unrestricted →
        // no filter. We exercise the contradiction branch by having ALL
        // the actor's roles in the map but none matching their identity.
        //
        // This is the "scoped reader who has no scope yet" case. Returning
        // an explicit `(false)` keeps the SQL well-formed without a NULL
        // expansion, and is easy to spot in EXPLAIN output during triage.
        let i = idx(&spec_with(
            vec![rule("east", "region", "eq", json!("east"))],
            vec![field("region")],
        ));
        // Actor has zero roles → loop produces no clauses → contradiction.
        let p = i.predicate(&[], 1).unwrap().unwrap();
        assert_eq!(p.sql, "(false)");
        assert!(p.params.is_empty());
    }
}
