//! Wire-up that the binary needs at startup, exposed as library
//! functions so they're unit-testable without a process.
//!
//! `main.rs` calls these in order — `build_object_store` → `build_session` →
//! `build_app_state`. Each is pure (no env reads, no global state) so tests
//! can drive them with synthetic configs and `file://`-backed object stores.

use std::sync::Arc;

use anyhow::{Context, Result};
use datafusion::execution::context::SessionContext;
use datafusion::execution::runtime_env::RuntimeEnv;
use object_store::ObjectStore;

use crate::config::WarmReaderConfig;
use crate::http;

/// Parse the storage URL and return the underlying ObjectStore plus
/// the path prefix the URL pointed at. We don't apply PrefixStore
/// here — callers decide which view they want.
pub fn build_object_store(
    url_str: &str,
) -> Result<(Arc<dyn ObjectStore>, object_store::path::Path)> {
    let url = url::Url::parse(url_str).with_context(|| format!("invalid storage URL: {url_str}"))?;
    let (store, prefix) = object_store::parse_url(&url)
        .with_context(|| format!("unsupported storage URL: {url_str}"))?;
    Ok((Arc::from(store), prefix))
}

/// Build the DataFusion `SessionContext` with the warm storage
/// registered. For `file://` URLs, DataFusion's built-in
/// `LocalFileSystem` handles things natively and the registration is
/// a no-op (still safe to call — it just overrides with the same
/// store). For `s3://` (and friends), the registration is what
/// teaches DataFusion how to authenticate.
///
/// We also disable `schema_force_view_types` so Parquet string columns
/// surface as plain `Utf8` arrays. DataFusion 53 defaults this to
/// true for performance, but it would force our downstream Arrow
/// downcasts to know about both `StringArray` and `StringViewArray`.
/// Since our project workflow doesn't benefit from view types (we
/// fully decode every row we return anyway), turn it off and keep
/// the downcast path single.
pub fn build_session(url_str: &str, raw_store: Arc<dyn ObjectStore>) -> Result<SessionContext> {
    let url = url::Url::parse(url_str).with_context(|| format!("invalid storage URL: {url_str}"))?;
    let runtime = Arc::new(RuntimeEnv::default());
    runtime.register_object_store(&url, raw_store);

    let mut cfg = datafusion::execution::config::SessionConfig::new();
    cfg.options_mut().execution.parquet.schema_force_view_types = false;

    let session = SessionContext::new_with_config_rt(cfg, runtime);
    Ok(session)
}

/// Wire the full HTTP `AppState` from a parsed `WarmReaderConfig`.
/// This is the pure portion of `main()` — no socket binds, no tracing
/// init, no select-loop. Tests construct an `AppState` end-to-end via
/// this single entry point.
pub fn build_app_state(cfg: &WarmReaderConfig) -> Result<http::AppState> {
    let (raw_store, prefix) = build_object_store(&cfg.storage_url)?;
    // Two store handles emerge from the same backend:
    //   1. `prefixed`: used for cheap HEAD existence checks in
    //      `datafusion_reader`. PrefixStore hides the bucket/prefix
    //      so paths in `object_layout` stay free of that boilerplate.
    //   2. `raw_store`: registered with DataFusion's runtime so
    //      `read_parquet` can fetch files by their full URL. We pass
    //      the URL WITH the prefix to DataFusion because that's what
    //      `read_parquet` resolves; the raw store underneath knows
    //      how to fetch the bucket-relative key.
    let prefixed: Arc<dyn ObjectStore> = if prefix.as_ref().is_empty() {
        raw_store.clone()
    } else {
        Arc::new(object_store::prefix::PrefixStore::new(
            Arc::clone(&raw_store),
            prefix.clone(),
        ))
    };

    let session = build_session(&cfg.storage_url, raw_store)?;

    Ok(http::AppState {
        session: Arc::new(session),
        store: prefixed,
        base_url: Arc::from(cfg.storage_url.as_str()),
        service_token: Arc::from(cfg.service_token.as_str()),
        // 12 months covers a year of warm history per request. The
        // warm-tier retention horizon is years, but per-request scan
        // depth is bounded so a pathological `until` years in the
        // past can't ask us to consult thousands of objects.
        max_months: 12,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::net::SocketAddr;

    fn cfg(url: String) -> WarmReaderConfig {
        WarmReaderConfig {
            storage_url: url,
            bind_addr: SocketAddr::from(([0, 0, 0, 0], 9090)),
            health_addr: SocketAddr::from(([0, 0, 0, 0], 9091)),
            service_token: "a-test-token-32-chars-min-xxxxxxx".into(),
            pretty_logs: false,
        }
    }

    #[test]
    fn build_object_store_handles_file_url_with_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().to_str().unwrap());
        let (_store, prefix) = build_object_store(&url).expect("file:// should resolve");
        assert!(!prefix.as_ref().is_empty(), "file URL path becomes the prefix");
    }

    #[test]
    fn build_object_store_handles_memory_url_with_no_prefix() {
        // `memory:///` has an empty prefix — exercises the bare-store
        // branch in `build_app_state` that skips PrefixStore wrapping.
        let (_store, prefix) = build_object_store("memory:///").expect("memory:// should resolve");
        assert!(prefix.as_ref().is_empty());
    }

    #[test]
    fn build_object_store_rejects_malformed_url() {
        let err = build_object_store("not a url").unwrap_err();
        assert!(format!("{err:#}").contains("invalid storage URL"));
    }

    #[test]
    fn build_object_store_rejects_unsupported_scheme() {
        let err = build_object_store("ftp://example.com/bucket").unwrap_err();
        assert!(format!("{err:#}").contains("unsupported storage URL"));
    }

    #[test]
    fn build_session_registers_url_with_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().to_str().unwrap());
        let (store, _prefix) = build_object_store(&url).unwrap();
        let session = build_session(&url, store).expect("session should build");
        // Smoke test: DataFusion option we explicitly toggled is set.
        let opts = session.copied_config().options().clone();
        assert!(!opts.execution.parquet.schema_force_view_types);
    }

    #[test]
    fn build_session_rejects_malformed_url() {
        let (store, _) = build_object_store("memory:///").unwrap();
        let err = match build_session("not a url", store) {
            Ok(_) => panic!("malformed URL should not produce a session"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("invalid storage URL"));
    }

    #[tokio::test]
    async fn build_app_state_wires_file_backed_store_with_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().to_str().unwrap());
        let state = build_app_state(&cfg(url.clone())).expect("state should build");
        assert_eq!(state.max_months, 12);
        assert_eq!(state.base_url.as_ref(), url.as_str());
        // service_token is propagated for the bearer-auth middleware.
        assert!(state.service_token.len() >= 16);
    }

    #[tokio::test]
    async fn build_app_state_wires_memory_backed_store_without_prefix() {
        // Triggers the `prefix.as_ref().is_empty()` branch — `prefixed`
        // becomes a clone of the raw store, no PrefixStore wrap.
        let state = build_app_state(&cfg("memory:///".into())).unwrap();
        assert_eq!(state.max_months, 12);
    }

    #[tokio::test]
    async fn build_app_state_propagates_storage_url_error() {
        let err = build_app_state(&cfg("not a url".into())).unwrap_err();
        assert!(format!("{err:#}").contains("invalid storage URL"));
    }
}
