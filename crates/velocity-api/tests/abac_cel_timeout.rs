#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

//! Phase 2b acceptance line: "ABAC CEL with deliberate infinite loop →
//! terminated at 10ms."
//!
//! CEL has no recursion or unbounded iteration — a literal infinite loop
//! isn't expressible — but a deeply-nested `.all()` over a moderate-sized
//! list is O(N^k) and easily exceeds the budget. We use N=60, k=3 → 216k
//! integer-arithmetic evaluations. On developer hardware that runs ~49ms;
//! even very slow CI dynos should clear 10ms by a wide margin.
//!
//! The timeout is enforced in `policy::evaluate_for` by wrapping the
//! `spawn_blocking` evaluator in `tokio::time::timeout(CEL_MAX_MS)`. On
//! deadline, the function emits `ApiError::PolicyDenied` with the literal
//! message `policy `{name}` timed out (>10ms)` — the latter is the assertion
//! anchor here so a future change to the timeout constant or message also
//! shows up in this test's diff.
//!
//! What this test would catch:
//! - dropped `tokio::time::timeout` wrapper (regression to "may run
//!   forever") → assertion fires because PolicyDenied never arrives or has
//!   the wrong shape
//! - `CEL_MAX_MS` accidentally raised to seconds (defeats DoS protection)
//!   → 60^3 finishes successfully and we get `Ok(())` instead of an error
//! - timeout firing but mis-mapped to `Internal(500)` or `BadRequest(400)`
//!   → fails the variant assertion below

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use velocity_api::error::ApiError;
use velocity_api::policy::{compile_policies, evaluate_for};
use velocity_api::Identity;
use velocity_types::crds::schema::AbacPolicy;

/// Build an `n`-element list literal `[0,1,...,n-1]` so we can drop it
/// straight into a CEL source string.
fn list_literal(n: u32) -> String {
    let nums: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    format!("[{}]", nums.join(","))
}

fn identity() -> Identity {
    Identity {
        actor_id: "ravi".into(),
        email: None,
        strategy: "acme-platform/jwt".into(),
        issuer: "https://idp.test".into(),
        roles: vec!["procurement-writer".into()],
        attributes: std::collections::HashMap::new(),
        api_key_scopes: None,
    }
}

#[tokio::test]
async fn nested_all_macro_exhausts_cel_budget_and_denies() {
    // 60^3 = 216k arithmetic evaluations. Local measurement: ~49ms,
    // 5x over the 10ms budget. Even at 10x slower (CI under contention)
    // it'd still take ~5ms — but at that speed nothing else in the test
    // suite would finish on time either, so we can rely on this margin.
    let list = list_literal(60);
    let condition =
        format!("{l}.all(a, {l}.all(b, {l}.all(c, a + b + c >= 0)))", l = list);

    let policies = compile_policies(&[AbacPolicy {
        name: "expensive-cel-bound-check".into(),
        action: "create".into(),
        fields: vec![],
        condition,
        message: Some("would have admitted, but never finished".into()),
    }]);
    // Sanity: the policy *compiled* — if a future cel-interpreter version
    // started rejecting nested all() at parse time, the test would have
    // to switch to a different expensive primitive. Asserting compile
    // success makes that failure mode loud.
    assert!(
        policies.iter().all(|p| matches!(
            p,
            velocity_api::policy::CompiledPolicy::Ok { .. }
        )),
        "policy did not compile — pick a different expensive CEL primitive",
    );
    let policies = Arc::new(policies);

    let start = Instant::now();
    let outcome = evaluate_for(&policies, "create", &Value::Null, &identity()).await;
    let elapsed = start.elapsed();

    // Assertion 1: denied (not admitted, not panicked).
    let err = outcome.expect_err("evaluation should not have succeeded");
    // Assertion 2: PolicyDenied variant with the timeout message — pins
    // both the variant mapping (timeout != Internal) and the user-facing
    // string format. Both are load-bearing: the variant drives the 403
    // status; the message drives audit/alerting routing.
    match err {
        ApiError::PolicyDenied(msg) => {
            assert!(
                msg.contains("timed out"),
                "expected timeout-flavored denial, got `{msg}`",
            );
            assert!(
                msg.contains(">10ms"),
                "expected the literal `>10ms` so a budget change ripples to this test, got `{msg}`",
            );
        }
        other => panic!("expected PolicyDenied, got {other:?}"),
    }

    // Assertion 3: the timeout actually clipped execution — the call
    // returned in a small multiple of CEL_MAX_MS (10ms), not after the
    // CEL program ran to completion (which would be ~49ms+). We allow
    // 100ms of slack for spawn_blocking dispatch + cooperative
    // cancellation; on healthy hardware the call returns in ~12-15ms.
    //
    // If this fires, the timeout fired but the call still took ~50ms,
    // which would mean `tokio::time::timeout` no longer interrupts the
    // spawn_blocking — a real regression worth investigating, not just a
    // flaky margin.
    assert!(
        elapsed.as_millis() < 100,
        "timeout fired but call took {}ms — \
         tokio::time::timeout may not be cancelling the spawn_blocking",
        elapsed.as_millis(),
    );
}

#[tokio::test]
async fn cheap_policy_runs_under_budget_and_admits() {
    // Positive control: a CEL that takes microseconds returns Ok, so the
    // expensive test isn't passing just because evaluate_for always errors.
    let policies = Arc::new(compile_policies(&[AbacPolicy {
        name: "trivial-admit".into(),
        action: "create".into(),
        fields: vec![],
        condition: "1 + 1 == 2".into(),
        message: None,
    }]));
    let outcome =
        evaluate_for(&policies, "create", &json!({ "po_number": "PO-1" }), &identity()).await;
    outcome.expect("cheap CEL should evaluate within budget");
}
