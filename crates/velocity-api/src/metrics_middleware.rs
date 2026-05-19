//! Axum middleware that records per-request metrics.
//!
//! Sits **inside** the auth layer: requests that fail authentication
//! never reach this middleware, so unauthenticated 401s do not get
//! charged against a schema label. Requests that authenticate and then
//! fail (404 on unknown route, 422 on validation, 5xx from the handler)
//! are charged with the appropriate `outcome` label.
//!
//! The schema label is derived from the URI path using the same parser
//! the auth middleware uses to pick a strategy — paths shaped like
//! `/api/{org}/{app}/{domain}/{object}/{version}[/…]` get the
//! `org/app/domain/object/version` label; anything else is bucketed
//! into the single [`label::SCHEMA_UNKNOWN`] series so platform routes
//! (`/api/platform/audit*`) and the root index don't burn cardinality.
//!
//! Operation is derived from method + path suffix without consulting
//! the registry, so the middleware stays cheap and synchronous on the
//! hot path.

use std::time::Instant;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::identity::Identity;
use crate::metrics::{label, operation_duration_seconds, operations_total};

/// Middleware function compatible with `axum::middleware::from_fn`.
pub async fn record(req: Request, next: Next) -> Response {
    let start = Instant::now();

    // Derive labels from the request before we hand it off — we can't
    // borrow the body after `next.run(req)` consumes the request.
    let schema_label = schema_label_from_uri(req.uri().path());
    let operation = operation_for(req.method(), req.uri().path());

    // Identity is inserted by the auth middleware that wraps us.
    // Missing → treat as anonymous (only happens on dev wiring that
    // skips the auth layer entirely — the constant-labelled fallback
    // keeps cardinality bounded).
    let (actor_type, strategy) = req
        .extensions()
        .get::<Identity>()
        .map(|i| (actor_type_for(i), i.strategy.clone()))
        .unwrap_or((label::actor_type::ANONYMOUS, String::new()));

    let response = next.run(req).await;
    let status = response.status();
    let outcome = outcome_for(status);

    operations_total()
        .with_label_values(&[
            schema_label.as_str(),
            operation,
            outcome,
            actor_type,
            strategy.as_str(),
        ])
        .inc();

    operation_duration_seconds()
        .with_label_values(&[operation, outcome])
        .observe(start.elapsed().as_secs_f64());

    // Note: `velocity_auth_attempts_total` lives in `crate::metrics`
    // but is NOT emitted here — by the time the request reaches this
    // middleware, the auth layer has already admitted it (or short-
    // circuited with a 401 that this layer never sees). The auth
    // counter belongs inside `auth::middleware::authenticate` so it
    // sees both admit and deny paths; that wiring is a follow-up.

    response
}

/// Returns the canonical schema label for a request URI, or
/// [`label::SCHEMA_UNKNOWN`] for any path that isn't shaped like a
/// per-schema API route.
///
/// Importantly: this does NOT consult the registry. A request to a
/// schema-shaped path whose CRD doesn't exist will be counted under
/// that path's label and resolve to a 404 — that's the right behaviour
/// for `denied`/`not_found` series.
fn schema_label_from_uri(uri_path: &str) -> String {
    let segments: Vec<&str> = uri_path.trim_start_matches('/').split('/').collect();
    if segments.len() < 6 || segments[0] != "api" {
        return label::SCHEMA_UNKNOWN.to_string();
    }
    // Reject platform-internal routes — `/api/platform/audit/...`
    // shares the `/api/{a}/{b}/{c}/{d}/{e}` shape but doesn't map to
    // a tenant schema.
    if segments[1] == "platform" {
        return label::SCHEMA_UNKNOWN.to_string();
    }
    format!("{}/{}/{}/{}/{}", segments[1], segments[2], segments[3], segments[4], segments[5])
}

/// Derive a stable `operation` label from method + URL suffix without
/// the registry. The router exposes a small enumerated set; anything
/// outside it falls into [`label::operation::OTHER`] so unknown paths
/// don't grow the label set.
fn operation_for(method: &Method, uri_path: &str) -> &'static str {
    let trimmed = uri_path.trim_end_matches('/');
    let segments: Vec<&str> = trimmed.trim_start_matches('/').split('/').collect();

    // `/api/{org}/search` is the cross-schema search endpoint — 3
    // segments, POST.
    if segments.len() == 3
        && segments[0] == "api"
        && segments[2] == "search"
        && method == Method::POST
    {
        return label::operation::CROSS_SEARCH;
    }

    // Platform routes are bucketed as "other" — they're internal,
    // not part of the per-schema operation taxonomy.
    if segments.len() >= 2 && segments[0] == "api" && segments[1] == "platform" {
        return label::operation::OTHER;
    }

    // Domain-level snapshot route:
    // `POST /api/{org}/{app}/{domain}/history/snapshot` — 6 segments
    // where the last two are `history/snapshot`. Must be checked
    // before the per-schema 6-segment branch below, otherwise the
    // POST gets bucketed as `create`.
    if segments.len() == 6
        && segments[0] == "api"
        && segments[4] == "history"
        && segments[5] == "snapshot"
        && method == Method::POST
    {
        return label::operation::SNAPSHOT;
    }

    // Per-schema routes: /api/{org}/{app}/{domain}/{object}/{version}[/...]
    if segments.len() < 6 || segments[0] != "api" {
        return label::operation::OTHER;
    }

    match (method, segments.len(), segments.get(7).copied()) {
        // 6 segments: list / create
        (&Method::GET, 6, _) => label::operation::LIST,
        (&Method::POST, 6, _) => label::operation::CREATE,
        // 7 segments — either {id} or a verb (e.g. /query, /search).
        (&Method::POST, 7, _) => match segments[6] {
            "query" => label::operation::QUERY,
            "search" => label::operation::SEARCH,
            _ => label::operation::OTHER,
        },
        (&Method::GET, 7, _) => label::operation::READ,
        (&Method::PUT, 7, _) => label::operation::UPDATE,
        (&Method::DELETE, 7, _) => label::operation::DELETE,
        // 8 segments: /{id}/{verb} for history/diff/restore/replay,
        // or /history/snapshot at the domain level.
        (&Method::GET, 8, _) => match segments[7] {
            "history" => label::operation::HISTORY,
            "diff" => label::operation::DIFF,
            "replay" => label::operation::REPLAY,
            _ => label::operation::OTHER,
        },
        (&Method::POST, 8, _) => match segments[7] {
            "restore" => label::operation::RESTORE,
            "snapshot" => label::operation::SNAPSHOT,
            _ => label::operation::OTHER,
        },
        _ => label::operation::OTHER,
    }
}

fn outcome_for(status: StatusCode) -> &'static str {
    match status.as_u16() {
        200..=299 => label::outcome::SUCCESS,
        404 => label::outcome::NOT_FOUND,
        401 | 403 => label::outcome::DENIED,
        422 => label::outcome::VALIDATION_ERROR,
        400..=499 => label::outcome::VALIDATION_ERROR,
        _ => label::outcome::ERROR,
    }
}

fn actor_type_for(identity: &Identity) -> &'static str {
    if identity.is_anonymous() {
        label::actor_type::ANONYMOUS
    } else if identity.api_key_scopes.is_some() {
        label::actor_type::API_KEY
    } else {
        label::actor_type::HUMAN
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn schema_label_extracts_five_segments() {
        let s = schema_label_from_uri("/api/acme/supply-chain/procurement/po/v1");
        assert_eq!(s, "acme/supply-chain/procurement/po/v1");
    }

    #[test]
    fn schema_label_works_for_id_suffix_routes() {
        let s = schema_label_from_uri("/api/acme/sc/proc/po/v1/abc-123");
        assert_eq!(s, "acme/sc/proc/po/v1");
    }

    #[test]
    fn schema_label_unknown_for_non_api_paths() {
        assert_eq!(schema_label_from_uri("/healthz"), label::SCHEMA_UNKNOWN);
        assert_eq!(schema_label_from_uri("/metrics"), label::SCHEMA_UNKNOWN);
        assert_eq!(schema_label_from_uri("/"), label::SCHEMA_UNKNOWN);
        assert_eq!(schema_label_from_uri("/api"), label::SCHEMA_UNKNOWN);
        assert_eq!(schema_label_from_uri("/api/acme/supply-chain"), label::SCHEMA_UNKNOWN);
    }

    #[test]
    fn schema_label_unknown_for_platform_routes() {
        assert_eq!(schema_label_from_uri("/api/platform/audit"), label::SCHEMA_UNKNOWN);
        assert_eq!(schema_label_from_uri("/api/platform/audit/verify"), label::SCHEMA_UNKNOWN);
    }

    #[test]
    fn operation_maps_list_create() {
        assert_eq!(operation_for(&Method::GET, "/api/acme/sc/proc/po/v1"), label::operation::LIST);
        assert_eq!(
            operation_for(&Method::POST, "/api/acme/sc/proc/po/v1"),
            label::operation::CREATE
        );
    }

    #[test]
    fn operation_maps_id_routes() {
        assert_eq!(
            operation_for(&Method::GET, "/api/acme/sc/proc/po/v1/abc"),
            label::operation::READ
        );
        assert_eq!(
            operation_for(&Method::PUT, "/api/acme/sc/proc/po/v1/abc"),
            label::operation::UPDATE
        );
        assert_eq!(
            operation_for(&Method::DELETE, "/api/acme/sc/proc/po/v1/abc"),
            label::operation::DELETE
        );
    }

    #[test]
    fn operation_maps_query_and_search() {
        assert_eq!(
            operation_for(&Method::POST, "/api/acme/sc/proc/po/v1/query"),
            label::operation::QUERY
        );
        assert_eq!(
            operation_for(&Method::POST, "/api/acme/sc/proc/po/v1/search"),
            label::operation::SEARCH
        );
        assert_eq!(
            operation_for(&Method::POST, "/api/acme/search"),
            label::operation::CROSS_SEARCH
        );
    }

    #[test]
    fn operation_maps_time_machine_routes() {
        assert_eq!(
            operation_for(&Method::GET, "/api/acme/sc/proc/po/v1/abc/history"),
            label::operation::HISTORY
        );
        assert_eq!(
            operation_for(&Method::GET, "/api/acme/sc/proc/po/v1/abc/diff"),
            label::operation::DIFF
        );
        assert_eq!(
            operation_for(&Method::POST, "/api/acme/sc/proc/po/v1/abc/restore"),
            label::operation::RESTORE
        );
        assert_eq!(
            operation_for(&Method::GET, "/api/acme/sc/proc/po/v1/abc/replay"),
            label::operation::REPLAY
        );
        assert_eq!(
            operation_for(&Method::POST, "/api/acme/sc/proc/history/snapshot"),
            label::operation::SNAPSHOT
        );
    }

    #[test]
    fn operation_other_for_unknown_paths() {
        assert_eq!(operation_for(&Method::GET, "/healthz"), label::operation::OTHER);
        assert_eq!(operation_for(&Method::GET, "/api/platform/audit"), label::operation::OTHER);
    }

    #[test]
    fn outcome_buckets_by_status() {
        assert_eq!(outcome_for(StatusCode::OK), label::outcome::SUCCESS);
        assert_eq!(outcome_for(StatusCode::CREATED), label::outcome::SUCCESS);
        assert_eq!(outcome_for(StatusCode::NOT_FOUND), label::outcome::NOT_FOUND);
        assert_eq!(outcome_for(StatusCode::UNAUTHORIZED), label::outcome::DENIED);
        assert_eq!(outcome_for(StatusCode::FORBIDDEN), label::outcome::DENIED);
        assert_eq!(outcome_for(StatusCode::UNPROCESSABLE_ENTITY), label::outcome::VALIDATION_ERROR);
        assert_eq!(outcome_for(StatusCode::BAD_REQUEST), label::outcome::VALIDATION_ERROR);
        assert_eq!(outcome_for(StatusCode::INTERNAL_SERVER_ERROR), label::outcome::ERROR);
    }

    /// End-to-end: the middleware actually wraps a handler and
    /// increments `velocity_operations_total` + observes a duration
    /// when a request flows through.
    #[tokio::test]
    async fn middleware_increments_counters_for_real_request() {
        use axum::{routing::get, Router};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        // Capture the counter's prior count so this test is robust
        // against other tests in the same process bumping it.
        let counter = crate::metrics::operations_total().with_label_values(&[
            "acme/sc/proc/po/v1",
            label::operation::LIST,
            label::outcome::SUCCESS,
            label::actor_type::ANONYMOUS,
            "",
        ]);
        let before = counter.get();

        let app: Router = Router::new()
            .route("/api/{org}/{app}/{domain}/{object}/{version}", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(record));

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/api/acme/sc/proc/po/v1")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.into_body().collect().await.unwrap();

        let after = counter.get();
        assert_eq!(after, before + 1, "operations counter must tick once");

        // Confirm /metrics text exposition shows our labels.
        let body = crate::metrics::gather();
        assert!(body.contains("velocity_operations_total"));
        assert!(body.contains("schema=\"acme/sc/proc/po/v1\""));
    }

    #[test]
    fn actor_type_from_identity() {
        assert_eq!(actor_type_for(&Identity::anonymous()), label::actor_type::ANONYMOUS);

        let mut user = Identity::anonymous();
        user.actor_id = "alice".into();
        user.strategy = "ns/jwt".into();
        assert_eq!(actor_type_for(&user), label::actor_type::HUMAN);

        let mut key_user = Identity::anonymous();
        key_user.actor_id = "svc".into();
        key_user.strategy = "ns/key".into();
        key_user.api_key_scopes = Some(Vec::new());
        assert_eq!(actor_type_for(&key_user), label::actor_type::API_KEY);
    }
}
