//! Serves the velocity-portal SPA from inside velocity-api.
//!
//! The portal is a React/Vite bundle produced by `cd portal && npm run
//! build`. The multi-stage Dockerfile copies `portal/dist/` into
//! `crates/velocity-api/static/` before the Rust compile, and
//! [`rust_embed`] bundles every file into the binary at compile time —
//! no volume mounts, no separate nginx container.
//!
//! Two routes are exposed:
//!
//! * `GET /config.json` — small JSON document read by the SPA at boot
//!   for runtime knobs (default auth strategy, Grafana URL, environment
//!   banner). Backed by env vars on the api Deployment so the same
//!   binary works across environments.
//!
//! * Everything else not matched by the API router falls through to
//!   [`serve`], which looks the path up in the embedded asset set and
//!   either ships the file or — for unknown paths — ships `index.html`
//!   so the SPA's client-side router can take over (deep links work).
//!
//! When the static/ directory is empty (the cargo-only dev path), the
//! fallback returns 404 instead of the usual SPA fallback. That keeps
//! `cargo run -p velocity-api` from accidentally pretending it has a
//! UI to serve.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;
use serde::Serialize;

/// Compile-time-embedded portal bundle. The Dockerfile drops the
/// `portal/dist/` contents into this folder before the Rust build; the
/// `interpolate-folder-path` feature on `rust-embed` resolves the path
/// relative to the crate's manifest at compile time.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/static/"]
struct PortalAssets;

/// SPA entry document. Vite emits `index.html` at the root of `dist/`,
/// so the embedded asset name is the same.
const SPA_INDEX: &str = "index.html";

/// Mount the portal routes onto a fresh router. Returned as `Router<()>`
/// so it composes cleanly with `.fallback_service` on the main router
/// (which is `Router<PlatformState>` after axum 0.8's State plumbing).
pub fn router() -> Router {
    Router::new().route("/config.json", get(config_json)).fallback(serve)
}

/// Runtime portal knobs read by the SPA at boot. Anything user-specific
/// belongs in the cluster's identity provider, not in this document.
#[derive(Debug, Serialize)]
struct PortalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    default_auth_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grafana_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
}

impl PortalConfig {
    fn from_env() -> Self {
        Self {
            default_auth_strategy: env_nonempty("VELOCITY_API_PORTAL_DEFAULT_AUTH_STRATEGY"),
            grafana_url: env_nonempty("VELOCITY_API_PORTAL_GRAFANA_URL"),
            environment: env_nonempty("VELOCITY_API_PORTAL_ENVIRONMENT"),
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

async fn config_json() -> Response {
    // Tiny payload, regenerated per request. The cost is negligible vs.
    // caching + invalidating on env change.
    let body = serde_json::to_vec(&PortalConfig::from_env())
        .unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Fallback handler. Tries the asset name, then `index.html` for SPA
/// deep-link routes. Cache headers: `index.html` is never cached
/// (so a redeploy is picked up next reload), all other assets use a
/// long max-age — Vite fingerprints filenames so cache busting is free.
async fn serve(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');

    if let Some(asset) = lookup(path) {
        return asset.respond_with(path, AssetKind::Direct);
    }

    // SPA route fallback — only when index.html itself is embedded.
    // The empty-static-dir case returns a clean 404 instead of pretending
    // there's a UI to serve.
    if let Some(index) = lookup(SPA_INDEX) {
        return index.respond_with(SPA_INDEX, AssetKind::SpaFallback);
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Internal helper so [`serve`] can branch on whether the asset is the
/// resolved path or the SPA fallback (different caching policy).
#[derive(Clone, Copy)]
enum AssetKind {
    Direct,
    SpaFallback,
}

struct Embedded {
    data: std::borrow::Cow<'static, [u8]>,
}

impl Embedded {
    fn respond_with(self, name: &str, kind: AssetKind) -> Response {
        let mime = mime_guess::from_path(name).first_or_octet_stream();
        let cache = match kind {
            AssetKind::Direct if name == SPA_INDEX => "no-store",
            AssetKind::SpaFallback => "no-store",
            AssetKind::Direct => "public, max-age=31536000, immutable",
        };
        let mut resp = Response::new(Body::from(self.data.into_owned()));
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
            headers.insert(header::CONTENT_TYPE, v);
        }
        if let Ok(v) = HeaderValue::from_str(cache) {
            headers.insert(header::CACHE_CONTROL, v);
        }
        resp
    }
}

fn lookup(path: &str) -> Option<Embedded> {
    // rust-embed's get is a compile-time hash lookup keyed on the
    // file's path relative to the embed folder root — no filesystem
    // access, no traversal risk to defend against. A missing asset
    // just returns None.
    let file = PortalAssets::get(path)?;
    Some(Embedded { data: file.data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_asset_lookup_returns_none() {
        // With no SPA built, the embedded set is essentially empty; this
        // confirms the type plumbing rather than the assets themselves.
        assert!(lookup("does-not-exist.html").is_none());
    }

    #[test]
    fn portal_config_omits_empty_fields() {
        // No env set → empty JSON object. The SPA tolerates absent
        // fields, so we serialize as `{}` rather than null keys.
        let cfg = PortalConfig {
            default_auth_strategy: None,
            grafana_url: None,
            environment: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn portal_config_serialises_set_fields() {
        let cfg = PortalConfig {
            default_auth_strategy: Some("velocity/portal-oidc".into()),
            grafana_url: Some("https://grafana.example.com".into()),
            environment: Some("dev".into()),
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["default_auth_strategy"], "velocity/portal-oidc");
        assert_eq!(v["grafana_url"], "https://grafana.example.com");
        assert_eq!(v["environment"], "dev");
    }

    #[test]
    fn env_nonempty_trims_and_drops_blank() {
        // Use a key unlikely to be set in any env that runs tests.
        let key = "VELOCITY_API_TEST_NONEMPTY_KEY_DO_NOT_SET";
        assert!(env_nonempty(key).is_none());
        std::env::set_var(key, "   ");
        assert!(env_nonempty(key).is_none(), "whitespace-only → None");
        std::env::set_var(key, "  hello  ");
        assert_eq!(env_nonempty(key).as_deref(), Some("hello"));
        std::env::remove_var(key);
    }
}
