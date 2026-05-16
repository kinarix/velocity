//! AdmissionReview handler — routes by `request.kind.kind` to the matching validator.

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use kube::core::DynamicObject;
use serde_json::Value;
use tracing::Instrument;

use crate::validators::{self, ValidationFailure};

/// `POST /validate` handler.
pub async fn validate(
    Json(review): Json<AdmissionReview<DynamicObject>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let span = tracing::info_span!(
        "admission",
        kind = tracing::field::Empty,
        namespace = tracing::field::Empty,
        name = tracing::field::Empty,
        uid = tracing::field::Empty,
    );

    let req: AdmissionRequest<DynamicObject> =
        review.try_into().map_err(|e: kube::core::admission::ConvertAdmissionReviewError| {
            (StatusCode::BAD_REQUEST, format!("invalid AdmissionReview: {e}"))
        })?;

    span.record("kind", req.kind.kind.as_str());
    span.record("namespace", req.namespace.as_deref().unwrap_or(""));
    span.record("name", req.name.as_str());
    span.record("uid", req.uid.as_str());

    async move {
        let response = match decide(&req) {
            Ok(()) => {
                tracing::info!("admit");
                AdmissionResponse::from(&req)
            }
            Err(failure) => {
                tracing::warn!(reason = %failure.0, "deny");
                AdmissionResponse::from(&req).deny(failure.0)
            }
        };
        let review: AdmissionReview<DynamicObject> = response.into_review();
        Ok::<_, (StatusCode, String)>(Json(review))
    }
    .instrument(span)
    .await
}

fn decide(req: &AdmissionRequest<DynamicObject>) -> Result<(), ValidationFailure> {
    let Some(obj) = req.object.as_ref() else {
        // DELETE has no object — admit (nothing to validate).
        return Ok(());
    };
    let ns = req.namespace.as_deref().unwrap_or("");
    let value: Value = serde_json::to_value(obj)
        .map_err(|e| ValidationFailure(format!("failed to serialize object: {e}")))?;

    match req.kind.kind.as_str() {
        "Domain" => validators::validate_domain(&value, ns),
        "Application" => validators::validate_application(&value, ns),
        "SchemaDefinition" => validators::validate_schema_definition(&value, ns),
        // Organisation lives in `platform` — no namespace rule yet.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        axum::Router::new().route("/validate", axum::routing::post(validate))
    }

    fn review(kind: &str, namespace: &str, obj: Value) -> Value {
        json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-1",
                "kind": { "group": "velocity.sh", "version": "v1", "kind": kind },
                "resource": { "group": "velocity.sh", "version": "v1", "resource": format!("{}s", kind.to_lowercase()) },
                "name": "test",
                "namespace": namespace,
                "operation": "CREATE",
                "userInfo": { "username": "tester" },
                "object": obj,
                "dryRun": false,
            }
        })
    }

    async fn run(body: Value) -> Value {
        let app = app();
        let req = Request::builder()
            .method("POST")
            .uri("/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn allows_valid_domain() {
        let body = review(
            "Domain",
            "acme-supply-chain",
            json!({
                "apiVersion": "velocity.sh/v1",
                "kind": "Domain",
                "metadata": {
                    "name": "procurement",
                    "labels": { "velocity.sh/org": "acme" },
                },
                "spec": { "app": "supply-chain", "displayName": "Procurement",
                          "access": { "defaultRole": "r", "adminRole": "a" } }
            }),
        );
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], true);
    }

    #[tokio::test]
    async fn denies_namespace_mismatch() {
        let body = review(
            "Domain",
            "acme-supply",
            json!({
                "metadata": { "labels": { "velocity.sh/org": "acme" } },
                "spec": { "app": "supply-chain" }
            }),
        );
        let resp = run(body).await;
        assert_eq!(resp["response"]["allowed"], false);
        let msg = resp["response"]["status"]["message"].as_str().unwrap();
        assert!(msg.contains("acme-supply-chain"));
    }
}
