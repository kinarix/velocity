//! Admin CRD read/write endpoints backing the tree UI (Phase 12c).
//!
//! These live in `velocity-platform-api` — NOT the shared `velocity-api`
//! library — so the data-API binary never links CRD-write code (ADR-011).
//!
//! All Velocity CRDs are reached generically through `kube`'s
//! [`DynamicObject`] + an [`ApiResource`] built from a small kind→plural
//! registry, so adding a CRD kind is a one-line table entry. Writes are
//! server-side applies, which means the **validating webhook stays in the
//! path** exactly as for `kubectl`/CLI. Every endpoint is gated by either
//! the platform service Bearer token (service-to-service) or a valid
//! OIDC browser session cookie (Phase 12c, any authenticated session).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use kube::api::{Api, DeleteParams, DynamicObject, ListParams, Patch, PatchParams};
use kube::core::{ApiResource, GroupVersionKind};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use velocity_core::auth::{SessionStore, SESSION_COOKIE_NAME};

const GROUP: &str = "velocity.sh";
const VERSION: &str = "v1";
const FIELD_MANAGER: &str = "velocity-platform-api";

/// State for the admin endpoints: a kube client, the platform token, and
/// optionally the session store for browser-cookie-based auth (Phase 12c).
#[derive(Clone)]
pub(crate) struct AdminState {
    pub(crate) kube: kube::Client,
    pub(crate) token: Option<Arc<String>>,
    /// OIDC session store — when set, a valid `velocity_session` cookie is an
    /// accepted alternative to the Bearer service token (browser portal).
    pub(crate) sessions: Option<Arc<dyn SessionStore>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct NsQuery {
    /// Optional namespace filter for list; omitted → all namespaces.
    pub(crate) namespace: Option<String>,
}

/// Map a UI-facing kind to its CRD plural. All Velocity CRDs are namespaced.
fn plural_for(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "Organisation" => "organisations",
        "Application" => "applications",
        "Domain" => "domains",
        "SchemaDefinition" => "schemadefinitions",
        "AuthStrategy" => "authstrategies",
        "RoleBinding" => "rolebindings",
        "ApiKey" => "apikeys",
        "ArchivePolicy" => "archivepolicies",
        "LogFilterPolicy" => "logfilterpolicies",
        "LogRoutingPolicy" => "logroutingpolicies",
        "PurgeRequest" => "purgerequests",
        _ => return None,
    })
}

fn api_resource(kind: &str) -> Option<ApiResource> {
    let plural = plural_for(kind)?;
    let gvk = GroupVersionKind::gvk(GROUP, VERSION, kind);
    Some(ApiResource::from_gvk_with_plural(&gvk, plural))
}

fn err(code: StatusCode, msg_code: &str, msg: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "code": msg_code, "message": msg })))
}

/// Two-path auth gate:
/// 1. Service-to-service: `Authorization: Bearer <platform_audit_token>` (constant-time).
/// 2. Browser: `velocity_session` cookie → session store lookup (Phase 12c).
///
/// When no token is configured AND no session store is wired, all paths return 503.
async fn authorize(state: &AdminState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    // Path 1 — Bearer token (service-to-service, CI, CLI).
    if let Some(expected) = state.token.as_deref() {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        if presented.as_bytes().ct_eq(expected.as_bytes()).into() {
            return Ok(());
        }
        // Token is configured but didn't match. Fall through to session check
        // if the session store is available; otherwise reject immediately.
        if state.sessions.is_none() {
            return Err(err(StatusCode::UNAUTHORIZED, "ADMIN_UNAUTHORIZED", "invalid platform token"));
        }
    }

    // Path 2 — browser OIDC session cookie.
    if let Some(sessions) = &state.sessions {
        let cookie_uuid = headers
            .get(axum::http::header::COOKIE)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| {
                s.split(';').find_map(|part| {
                    part.trim()
                        .strip_prefix(SESSION_COOKIE_NAME)
                        .and_then(|rest| rest.strip_prefix('='))
                        .and_then(|val| val.parse::<uuid::Uuid>().ok())
                })
            });
        if let Some(uuid) = cookie_uuid {
            if sessions.lookup(uuid).await.is_ok() {
                return Ok(());
            }
        }
    }

    // Nothing worked — or nothing is configured at all.
    if state.token.is_none() && state.sessions.is_none() {
        Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "ADMIN_TOKEN_UNSET",
            "platform admin token is not configured",
        ))
    } else {
        Err(err(
            StatusCode::UNAUTHORIZED,
            "ADMIN_UNAUTHORIZED",
            "valid platform token or authenticated session required",
        ))
    }
}

fn dyn_api(state: &AdminState, ar: &ApiResource, ns: Option<&str>) -> Api<DynamicObject> {
    match ns {
        Some(ns) => Api::namespaced_with(state.kube.clone(), ns, ar),
        None => Api::all_with(state.kube.clone(), ar),
    }
}

/// GET /api/platform/objects/{kind} — list a CRD kind (optionally per ns).
pub(crate) async fn list(
    State(state): State<AdminState>,
    Path(kind): Path<String>,
    Query(q): Query<NsQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    authorize(&state, &headers).await?;
    let ar =
        api_resource(&kind).ok_or_else(|| err(StatusCode::NOT_FOUND, "UNKNOWN_KIND", "unknown CRD kind"))?;
    let api = dyn_api(&state, &ar, q.namespace.as_deref());
    let objs = api
        .list(&ListParams::default())
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, "KUBE_LIST_FAILED", &e.to_string()))?;
    let items: Vec<Value> = objs
        .items
        .into_iter()
        .filter_map(|o| serde_json::to_value(o).ok())
        .collect();
    Ok(Json(json!({ "kind": format!("{kind}List"), "apiVersion": "velocity.sh/v1", "items": items })))
}

/// GET /api/platform/objects/{kind}/{namespace}/{name} — fetch one object.
pub(crate) async fn get_one(
    State(state): State<AdminState>,
    Path((kind, namespace, name)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    authorize(&state, &headers).await?;
    let ar =
        api_resource(&kind).ok_or_else(|| err(StatusCode::NOT_FOUND, "UNKNOWN_KIND", "unknown CRD kind"))?;
    let obj = dyn_api(&state, &ar, Some(&namespace))
        .get(&name)
        .await
        .map_err(|e| err(StatusCode::NOT_FOUND, "KUBE_GET_FAILED", &e.to_string()))?;
    Ok(Json(serde_json::to_value(obj).unwrap_or_else(|_| json!({}))))
}

/// PUT /api/platform/objects/{kind}/{namespace}/{name} — server-side apply
/// (create or update). The validating webhook runs as for any apply.
pub(crate) async fn apply(
    State(state): State<AdminState>,
    Path((kind, namespace, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    authorize(&state, &headers).await?;
    let ar =
        api_resource(&kind).ok_or_else(|| err(StatusCode::NOT_FOUND, "UNKNOWN_KIND", "unknown CRD kind"))?;
    let obj: DynamicObject = serde_json::from_value(body)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "INVALID_MANIFEST", &e.to_string()))?;
    let applied = dyn_api(&state, &ar, Some(&namespace))
        .patch(&name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(&obj))
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, "KUBE_APPLY_FAILED", &e.to_string()))?;
    Ok(Json(serde_json::to_value(applied).unwrap_or_else(|_| json!({}))))
}

/// DELETE /api/platform/objects/{kind}/{namespace}/{name}.
pub(crate) async fn delete(
    State(state): State<AdminState>,
    Path((kind, namespace, name)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    authorize(&state, &headers).await?;
    let ar =
        api_resource(&kind).ok_or_else(|| err(StatusCode::NOT_FOUND, "UNKNOWN_KIND", "unknown CRD kind"))?;
    dyn_api(&state, &ar, Some(&namespace))
        .delete(&name, &DeleteParams::default())
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, "KUBE_DELETE_FAILED", &e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Routes mounted under the platform listener. Self-authenticate via the
/// platform token (the `/api/platform/*` prefix is skipped by the per-schema
/// auth middleware), so they're safe to compose with it.
pub(crate) fn router(state: AdminState) -> axum::Router {
    use axum::routing::{delete as del, get};
    axum::Router::new()
        .route("/api/platform/objects/{kind}", get(list))
        .route("/api/platform/objects/{kind}/{namespace}/{name}", get(get_one).put(apply).delete(del(delete)))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::{HeaderMap, HeaderValue};
    use velocity_core::auth::{MockSessionStore, SessionRecord, SESSION_COOKIE_NAME};

    use super::*;

    fn make_session(id: uuid::Uuid) -> SessionRecord {
        SessionRecord {
            id,
            actor_id: "alice".into(),
            issuer: "https://idp.example.com".into(),
            id_token_claims: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        }
    }

    #[test]
    fn known_kinds_resolve_to_plurals() {
        assert_eq!(plural_for("SchemaDefinition"), Some("schemadefinitions"));
        assert_eq!(plural_for("Domain"), Some("domains"));
        assert_eq!(plural_for("LogRoutingPolicy"), Some("logroutingpolicies"));
        assert!(plural_for("Nonsense").is_none());
    }

    #[test]
    fn api_resource_built_for_known_kind() {
        let ar = api_resource("SchemaDefinition").unwrap();
        assert_eq!(ar.group, "velocity.sh");
        assert_eq!(ar.version, "v1");
        assert_eq!(ar.plural, "schemadefinitions");
    }

    fn no_kube() -> kube::Client {
        // kube-rs uses rustls, which needs a CryptoProvider installed before
        // client construction. install_default() is idempotent (ignores Err).
        rustls::crypto::aws_lc_rs::default_provider().install_default().ok();
        kube::Client::try_from(
            kube::Config::new("http://localhost:6443".parse().expect("valid url")),
        )
        .expect("dummy kube client")
    }

    #[tokio::test]
    async fn bearer_token_accepted() {
        let state = AdminState {
            kube: no_kube(),
            token: Some(Arc::new("secret".into())),
            sessions: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(authorize(&state, &headers).await.is_ok());
    }

    #[tokio::test]
    async fn wrong_bearer_rejected_no_sessions() {
        let state = AdminState {
            kube: no_kube(),
            token: Some(Arc::new("secret".into())),
            sessions: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        let result = authorize(&state, &headers).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_session_cookie_accepted() {
        let session_id = uuid::Uuid::new_v4();
        let store = MockSessionStore::new();
        store.insert(make_session(session_id));

        let state = AdminState {
            kube: no_kube(),
            token: None,
            sessions: Some(Arc::new(store)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={session_id}"))
                .expect("valid header value"),
        );
        assert!(authorize(&state, &headers).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_session_cookie_rejected() {
        let store = MockSessionStore::new(); // empty — lookup will return Expired
        let state = AdminState {
            kube: no_kube(),
            token: None,
            sessions: Some(Arc::new(store)),
        };
        let mut headers = HeaderMap::new();
        let bad_uuid = uuid::Uuid::new_v4();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={bad_uuid}"))
                .expect("valid header value"),
        );
        let result = authorize(&state, &headers).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn no_token_and_no_sessions_returns_503() {
        let state = AdminState { kube: no_kube(), token: None, sessions: None };
        let headers = HeaderMap::new();
        let result = authorize(&state, &headers).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
