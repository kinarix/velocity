//! Payload validation — runs before any SQL is built or executed.
//!
//! Two passes:
//!
//! 1. [`validate_fields`] — synchronous, schema-driven type / required /
//!    enum / range / pattern checks against each declared field.
//! 2. [`validate_rules`] — asynchronous, runs the schema's
//!    `validations[]` (CEL programs + simple compare rules). CEL programs
//!    are pre-compiled at registry-resolve time and each evaluation runs
//!    inside `tokio::time::timeout` per ADR — CEL safety (default 10 ms).
//!
//! Both return `ApiError::BadRequest` with a human-readable message naming
//! the offending field / rule. The handler aborts before touching the DB.
//!
//! Required-field semantics differ between create and update:
//!   * `create` — every `required: true` field must be present and non-null.
//!   * `update` — partial; only the fields the caller supplied are checked.

use std::sync::Arc;
use std::time::Duration;

use cel_interpreter::{Context as CelContext, Program};
use regex::Regex;
use serde_json::{Map, Value};
use velocity_types::crds::schema::{FieldKind, FieldSpec, ValidationKind, ValidationRule};

use crate::error::ApiError;
use crate::registry::ResolvedSchema;

/// Hard cap on per-rule CEL execution. The CRD lets a rule request a lower
/// `maxExecutionMs` but never a higher one — ADR — CEL safety.
pub const CEL_MAX_MS: u64 = 10;

/// A validation rule that's been turned into a runnable form at resolve
/// time. `Err` means the rule failed to compile and any payload should be
/// rejected — the schema is in an unsafe state.
#[derive(Debug)]
pub enum CompiledRule {
    /// Compiled CEL program. Wrapped in `Arc` so we can move a cheap handle
    /// into `tokio::task::spawn_blocking` without requiring `Program: Clone`
    /// or paying a copy per request.
    Cel {
        program: Arc<Program>,
        message: String,
        max_ms: u64,
    },
    Compare {
        left: String,
        op: CompareOp,
        right: CompareOperand,
        message: String,
    },
    /// Compile / parse failure. The rule's intent is unknown; fail-closed.
    Broken {
        reason: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "==" | "=" => CompareOp::Eq,
            "!=" | "<>" => CompareOp::Ne,
            "<" => CompareOp::Lt,
            "<=" => CompareOp::Le,
            ">" => CompareOp::Gt,
            ">=" => CompareOp::Ge,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub enum CompareOperand {
    /// Refers to a field on the payload (`self.<name>` or bare `<name>`).
    Field(String),
    /// Literal JSON value parsed from the operand string.
    Literal(Value),
}

pub fn compile_rules(rules: &[ValidationRule]) -> Vec<CompiledRule> {
    rules.iter().map(compile_one).collect()
}

fn compile_one(rule: &ValidationRule) -> CompiledRule {
    let message = rule.message.clone().unwrap_or_else(|| "validation failed".into());
    match rule.kind {
        ValidationKind::Cel => {
            let source = match rule.rule.as_deref() {
                Some(s) => s,
                None => {
                    return CompiledRule::Broken {
                        reason: "CEL rule has no `rule` source".into(),
                        message,
                    }
                }
            };
            match Program::compile(source) {
                Ok(program) => CompiledRule::Cel {
                    program: Arc::new(program),
                    message,
                    max_ms: rule
                        .max_execution_ms
                        .map(|m| (m as u64).min(CEL_MAX_MS))
                        .unwrap_or(CEL_MAX_MS),
                },
                Err(e) => {
                    CompiledRule::Broken { reason: format!("CEL compile error: {e}"), message }
                }
            }
        }
        ValidationKind::Compare => {
            let (left, op, right) = match (
                rule.left.as_deref(),
                rule.operator.as_deref().and_then(CompareOp::parse),
                rule.right.as_deref(),
            ) {
                (Some(l), Some(op), Some(r)) => (l.to_string(), op, parse_operand(r)),
                _ => {
                    return CompiledRule::Broken {
                        reason: "compare rule missing left/operator/right".into(),
                        message,
                    }
                }
            };
            CompiledRule::Compare { left, op, right, message }
        }
    }
}

/// Operands accept either `self.<field>` / `<field>` or a JSON literal. We
/// only honour the prefix-stripped field reference form — anything else is
/// parsed as JSON and falls back to a string literal if that fails.
fn parse_operand(s: &str) -> CompareOperand {
    let trimmed = s.trim();
    if let Some(field) = trimmed.strip_prefix("self.") {
        return CompareOperand::Field(field.to_string());
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return CompareOperand::Literal(v);
    }
    CompareOperand::Literal(Value::String(trimmed.to_string()))
}

/// What flavor of write are we validating?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Create,
    Update,
}

/// Synchronous field checks. Returns the first offending issue — we surface
/// one error at a time rather than a list so the message stays specific.
pub fn validate_fields(
    schema: &ResolvedSchema,
    payload: &Map<String, Value>,
    mode: WriteMode,
) -> Result<(), ApiError> {
    if mode == WriteMode::Create {
        for f in schema.fields.ordered.iter() {
            if f.required {
                match payload.get(&f.name) {
                    None | Some(Value::Null) => {
                        return Err(ApiError::BadRequest(format!(
                            "field `{}` is required",
                            f.name
                        )));
                    }
                    _ => {}
                }
            }
        }
    }

    for (name, value) in payload.iter() {
        let Some(spec) = schema.fields.by_name.get(name) else {
            // Unknown fields are silently ignored at SQL build time (Phase 1).
            // Phase 2+ may strict-reject; for now we don't fail here.
            continue;
        };
        // null is allowed when the field isn't required; required nullness is
        // already caught above for create. For update we let the DB enforce.
        if matches!(value, Value::Null) {
            continue;
        }
        check_field_value(spec, value)?;
    }

    Ok(())
}

fn check_field_value(spec: &FieldSpec, value: &Value) -> Result<(), ApiError> {
    let name = &spec.name;
    match spec.kind {
        FieldKind::String | FieldKind::Enum | FieldKind::Ref => {
            let s =
                value.as_str().ok_or_else(|| bad(format!("field `{name}` must be a string")))?;
            if let Some(max) = spec.max_length {
                if s.chars().count() > max as usize {
                    return Err(bad(format!(
                        "field `{name}` exceeds max length {max} (got {})",
                        s.chars().count()
                    )));
                }
            }
            if matches!(spec.kind, FieldKind::Enum)
                && !spec.enum_values.is_empty()
                && !spec.enum_values.iter().any(|e| e == s)
            {
                return Err(bad(format!("field `{name}` must be one of {:?}", spec.enum_values)));
            }
            if let Some(pat) = spec.pattern.as_deref() {
                // The webhook already vets the regex at admission; we still
                // guard here in case the registry was seeded from outside.
                let re = Regex::new(pat).map_err(|e| {
                    ApiError::Internal(format!("field `{name}` has invalid regex pattern: {e}"))
                })?;
                if !re.is_match(s) {
                    return Err(bad(format!("field `{name}` does not match pattern")));
                }
            }
        }
        FieldKind::Integer => {
            let n = value
                .as_i64()
                .or_else(|| value.as_f64().and_then(|f| (f.fract() == 0.0).then_some(f as i64)))
                .ok_or_else(|| bad(format!("field `{name}` must be an integer")))?;
            check_range(spec, n as f64, name)?;
        }
        FieldKind::Number => {
            let n =
                value.as_f64().ok_or_else(|| bad(format!("field `{name}` must be a number")))?;
            check_range(spec, n, name)?;
        }
        FieldKind::Boolean => {
            if !value.is_boolean() {
                return Err(bad(format!("field `{name}` must be a boolean")));
            }
        }
        FieldKind::Date => {
            let s = value
                .as_str()
                .ok_or_else(|| bad(format!("field `{name}` must be a date string")))?;
            // Cheap shape check (YYYY-MM-DD) — Postgres will do the strict
            // parse downstream and surface a 400 via the error mapping.
            if s.len() != 10
                || s.as_bytes().get(4) != Some(&b'-')
                || s.as_bytes().get(7) != Some(&b'-')
            {
                return Err(bad(format!("field `{name}` must look like YYYY-MM-DD")));
            }
        }
        FieldKind::Datetime => {
            let s = value
                .as_str()
                .ok_or_else(|| bad(format!("field `{name}` must be a datetime string")))?;
            // Require a digit-rich, timezone-bearing form. RFC3339 strictness
            // is enforced by Postgres' timestamptz cast.
            if s.len() < 10 || !s.as_bytes().iter().any(|b| matches!(b, b'T' | b' ')) {
                return Err(bad(format!("field `{name}` must be an ISO-8601 datetime string")));
            }
        }
        FieldKind::Uuid => {
            let s = value
                .as_str()
                .ok_or_else(|| bad(format!("field `{name}` must be a uuid string")))?;
            if uuid::Uuid::parse_str(s).is_err() {
                return Err(bad(format!("field `{name}` is not a valid uuid")));
            }
        }
        FieldKind::Json => {
            // Any JSON value is acceptable.
        }
    }
    Ok(())
}

fn check_range(spec: &FieldSpec, n: f64, name: &str) -> Result<(), ApiError> {
    if let Some(min) = spec.min {
        if n < min {
            return Err(bad(format!("field `{name}` must be >= {min} (got {n})")));
        }
    }
    if let Some(max) = spec.max {
        if n > max {
            return Err(bad(format!("field `{name}` must be <= {max} (got {n})")));
        }
    }
    Ok(())
}

fn bad(msg: String) -> ApiError {
    ApiError::BadRequest(msg)
}

/// Run every compiled rule against the payload. CEL programs run inside
/// `tokio::time::timeout`. A rule that fails is converted into the rule's
/// configured message (or a generic "validation failed").
pub async fn validate_rules(
    rules: &Arc<Vec<CompiledRule>>,
    payload: &Value,
) -> Result<(), ApiError> {
    for rule in rules.iter() {
        match rule {
            CompiledRule::Cel { program, message, max_ms } => {
                let program = Arc::clone(program);
                let payload_owned = payload.clone();
                let max_ms = *max_ms;
                // Run CEL on the blocking pool so a runaway expression can't
                // wedge the executor. `timeout` polls the JoinHandle — the
                // blocking thread will finish on its own, but we stop
                // waiting after `max_ms`.
                let outcome = tokio::time::timeout(
                    Duration::from_millis(max_ms),
                    tokio::task::spawn_blocking(move || {
                        let mut ctx = CelContext::default();
                        ctx.add_variable("self", payload_owned)
                            .map_err(|e| format!("CEL context: {e}"))?;
                        program.execute(&ctx).map_err(|e| format!("{e}"))
                    }),
                )
                .await;
                match outcome {
                    Err(_) => {
                        return Err(ApiError::BadRequest(format!(
                            "validation `{message}` timed out (>{max_ms}ms)"
                        )));
                    }
                    Ok(Err(join_err)) => {
                        return Err(ApiError::Internal(format!(
                            "CEL evaluator panicked: {join_err}"
                        )));
                    }
                    Ok(Ok(Err(eval_err))) => {
                        return Err(ApiError::BadRequest(format!(
                            "validation `{message}` errored: {eval_err}"
                        )));
                    }
                    Ok(Ok(Ok(value))) => {
                        if !cel_truthy(&value) {
                            return Err(ApiError::BadRequest(message.clone()));
                        }
                    }
                }
            }
            CompiledRule::Compare { left, op, right, message } => {
                let lhs = payload.get(left).cloned().unwrap_or(Value::Null);
                let rhs = match right {
                    CompareOperand::Field(name) => {
                        payload.get(name).cloned().unwrap_or(Value::Null)
                    }
                    CompareOperand::Literal(v) => v.clone(),
                };
                if !compare(&lhs, *op, &rhs) {
                    return Err(ApiError::BadRequest(message.clone()));
                }
            }
            CompiledRule::Broken { reason, message } => {
                tracing::error!(
                    rule = %message,
                    reason = %reason,
                    "fail-closed: schema has an uncompilable validation rule"
                );
                return Err(ApiError::Internal(format!(
                    "schema validation rule is broken: {message} ({reason})"
                )));
            }
        }
    }
    Ok(())
}

fn cel_truthy(v: &cel_interpreter::Value) -> bool {
    use cel_interpreter::Value as V;
    match v {
        V::Bool(b) => *b,
        V::Null => false,
        V::Int(n) => *n != 0,
        V::UInt(n) => *n != 0,
        V::Float(f) => *f != 0.0,
        V::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn compare(lhs: &Value, op: CompareOp, rhs: &Value) -> bool {
    use CompareOp::*;
    match (lhs, rhs) {
        (Value::Number(a), Value::Number(b)) => {
            let (Some(a), Some(b)) = (a.as_f64(), b.as_f64()) else {
                return matches!(op, Eq) && lhs == rhs;
            };
            match op {
                Eq => a == b,
                Ne => a != b,
                Lt => a < b,
                Le => a <= b,
                Gt => a > b,
                Ge => a >= b,
            }
        }
        (Value::String(a), Value::String(b)) => match op {
            Eq => a == b,
            Ne => a != b,
            Lt => a < b,
            Le => a <= b,
            Gt => a > b,
            Ge => a >= b,
        },
        (Value::Bool(a), Value::Bool(b)) => match op {
            Eq => a == b,
            Ne => a != b,
            _ => false,
        },
        _ => match op {
            Eq => lhs == rhs,
            Ne => lhs != rhs,
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use velocity_types::common::SchemaPath;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
        SearchSpec, SearchTier, ValidationKind, ValidationRule,
    };

    fn field(name: &str, kind: FieldKind) -> FieldSpec {
        let mut f: FieldSpec =
            serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap();
        f.kind = kind;
        f
    }

    fn spec(fields: Vec<FieldSpec>, validations: Vec<ValidationRule>) -> SchemaDefinitionSpec {
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
            validations,
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        }
    }

    fn schema(s: SchemaDefinitionSpec) -> ResolvedSchema {
        ResolvedSchema::from_spec(
            SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1"),
            s,
        )
    }

    fn payload(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn required_missing_rejected_on_create() {
        let s = schema(spec(
            vec![{
                let mut f = field("po_number", FieldKind::String);
                f.required = true;
                f
            }],
            vec![],
        ));
        let err = validate_fields(&s, &payload(json!({})), WriteMode::Create).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(m) if m.contains("required")));
    }

    #[test]
    fn required_missing_accepted_on_update() {
        let s = schema(spec(
            vec![{
                let mut f = field("po_number", FieldKind::String);
                f.required = true;
                f
            }],
            vec![],
        ));
        validate_fields(&s, &payload(json!({})), WriteMode::Update).unwrap();
    }

    #[test]
    fn integer_range_enforced() {
        let s = schema(spec(
            vec![{
                let mut f = field("qty", FieldKind::Integer);
                f.min = Some(1.0);
                f.max = Some(100.0);
                f
            }],
            vec![],
        ));
        assert!(matches!(
            validate_fields(&s, &payload(json!({"qty": 0})), WriteMode::Create),
            Err(ApiError::BadRequest(_))
        ));
        assert!(matches!(
            validate_fields(&s, &payload(json!({"qty": 101})), WriteMode::Create),
            Err(ApiError::BadRequest(_))
        ));
        validate_fields(&s, &payload(json!({"qty": 50})), WriteMode::Create).unwrap();
    }

    #[test]
    fn enum_check_enforced() {
        let s = schema(spec(
            vec![{
                let mut f = field("status", FieldKind::Enum);
                f.enum_values = vec!["draft".into(), "approved".into()];
                f
            }],
            vec![],
        ));
        validate_fields(&s, &payload(json!({"status": "approved"})), WriteMode::Create).unwrap();
        assert!(matches!(
            validate_fields(&s, &payload(json!({"status": "rejected"})), WriteMode::Create),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn pattern_enforced() {
        let s = schema(spec(
            vec![{
                let mut f = field("po_number", FieldKind::String);
                f.pattern = Some(r"^PO-\d{4,}$".into());
                f
            }],
            vec![],
        ));
        validate_fields(&s, &payload(json!({"po_number": "PO-0001"})), WriteMode::Create).unwrap();
        assert!(matches!(
            validate_fields(&s, &payload(json!({"po_number": "X"})), WriteMode::Create),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn type_mismatch_rejected() {
        let s = schema(spec(vec![field("flag", FieldKind::Boolean)], vec![]));
        assert!(matches!(
            validate_fields(&s, &payload(json!({"flag": "yes"})), WriteMode::Create),
            Err(ApiError::BadRequest(_))
        ));
        validate_fields(&s, &payload(json!({"flag": true})), WriteMode::Create).unwrap();
    }

    #[tokio::test]
    async fn cel_rule_passes_and_fails() {
        let rules = vec![ValidationRule {
            kind: ValidationKind::Cel,
            left: None,
            operator: None,
            right: None,
            rule: Some("self.amount > 0".into()),
            message: Some("amount must be positive".into()),
            max_execution_ms: None,
        }];
        let compiled = Arc::new(compile_rules(&rules));
        validate_rules(&compiled, &json!({ "amount": 5 })).await.unwrap();
        let err = validate_rules(&compiled, &json!({ "amount": 0 })).await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(m) if m.contains("amount must be positive")));
    }

    #[tokio::test]
    async fn compare_rule_field_to_field() {
        let rules = vec![ValidationRule {
            kind: ValidationKind::Compare,
            left: Some("start_date".into()),
            operator: Some("<".into()),
            right: Some("self.end_date".into()),
            rule: None,
            message: Some("start_date must precede end_date".into()),
            max_execution_ms: None,
        }];
        let compiled = Arc::new(compile_rules(&rules));
        validate_rules(&compiled, &json!({ "start_date": "2025-01-01", "end_date": "2025-02-01" }))
            .await
            .unwrap();
        let err = validate_rules(
            &compiled,
            &json!({ "start_date": "2025-03-01", "end_date": "2025-02-01" }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(m) if m.contains("precede")));
    }

    #[tokio::test]
    async fn broken_rule_is_fail_closed() {
        // Compile a rule with garbage CEL — compile fails, runtime must
        // refuse rather than silently allow writes through.
        let rules = vec![ValidationRule {
            kind: ValidationKind::Cel,
            left: None,
            operator: None,
            right: None,
            rule: Some("this is not (valid CEL".into()),
            message: Some("bogus rule".into()),
            max_execution_ms: None,
        }];
        let compiled = Arc::new(compile_rules(&rules));
        let err = validate_rules(&compiled, &json!({})).await.unwrap_err();
        assert!(matches!(err, ApiError::Internal(_)));
    }
}
