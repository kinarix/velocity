use std::sync::Arc;

use anyhow::{Context, Result};
use datafusion::execution::context::SessionContext;
use datafusion::execution::runtime_env::RuntimeEnv;
use object_store::ObjectStore;
use tracing_subscriber::EnvFilter;
use velocity_warm_reader::{http, WarmReaderConfig};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = WarmReaderConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind_addr = %cfg.bind_addr,
        health_addr = %cfg.health_addr,
        storage_url = %cfg.storage_url,
        "velocity-warm-reader starting",
    );

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
            wrap_clone(&raw_store),
            prefix.clone(),
        ))
    };

    let session = build_session(&cfg.storage_url, raw_store)?;

    let state = http::AppState {
        session: Arc::new(session),
        store: prefixed,
        base_url: Arc::from(cfg.storage_url.as_str()),
        service_token: Arc::from(cfg.service_token.as_str()),
        // 12 months covers a year of warm history per request. The
        // warm-tier retention horizon is years, but per-request scan
        // depth is bounded so a pathological `until` years in the
        // past can't ask us to consult thousands of objects.
        max_months: 12,
    };

    let data_router = http::router(state);
    let health_router = http::health_router();

    let data_listener = tokio::net::TcpListener::bind(cfg.bind_addr)
        .await
        .with_context(|| format!("failed to bind data socket {}", cfg.bind_addr))?;
    let health_listener = tokio::net::TcpListener::bind(cfg.health_addr)
        .await
        .with_context(|| format!("failed to bind health socket {}", cfg.health_addr))?;

    tracing::info!("listeners up; serving warm-tier reads");

    let data_fut = async move { axum::serve(data_listener, data_router).await };
    let health_fut = async move { axum::serve(health_listener, health_router).await };

    tokio::select! {
        r = data_fut    => r.context("data listener exited")?,
        r = health_fut  => r.context("health listener exited")?,
    }
    Ok(())
}

/// Parse the storage URL and return the underlying ObjectStore plus
/// the path prefix the URL pointed at. We don't apply PrefixStore
/// here — callers decide which view they want.
fn build_object_store(
    url_str: &str,
) -> Result<(Arc<dyn ObjectStore>, object_store::path::Path)> {
    let url = url::Url::parse(url_str).with_context(|| format!("invalid storage URL: {url_str}"))?;
    let (store, prefix) =
        object_store::parse_url(&url).with_context(|| format!("unsupported storage URL: {url_str}"))?;
    Ok((Arc::from(store), prefix))
}

/// `Arc<dyn ObjectStore>` doesn't impl `Clone` on the trait object
/// itself, but the underlying `Arc` does. `Arc::clone` works fine —
/// this helper just hides the type-inference dance when threading
/// the same store into two construction paths.
fn wrap_clone(store: &Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
    Arc::clone(store)
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
fn build_session(url_str: &str, raw_store: Arc<dyn ObjectStore>) -> Result<SessionContext> {
    let url = url::Url::parse(url_str).with_context(|| format!("invalid storage URL: {url_str}"))?;
    let runtime = Arc::new(RuntimeEnv::default());
    runtime.register_object_store(&url, raw_store);

    let mut cfg = datafusion::execution::config::SessionConfig::new();
    cfg.options_mut().execution.parquet.schema_force_view_types = false;

    let session = SessionContext::new_with_config_rt(cfg, runtime);
    Ok(session)
}

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_warm_reader=debug"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}
