//! velocity-archive-worker — periodic batched archival of hot rows into
//! per-domain `*_archive` schemas. See `lib.rs` and `worker.rs` for the
//! mechanics; this binary just wires kube + sqlx + config and hands off
//! to [`worker::run`].

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use velocity_archive_worker::worker::{run, WorkerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().init();

    let pg_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (e.g. postgres://velocity_api@.../velocity)")?;
    let pool = PgPoolOptions::new()
        .max_connections(env_u32("ARCHIVE_PG_MAX_CONNS", 4))
        .connect(&pg_url)
        .await
        .context("connecting to Postgres")?;

    let kube = kube::Client::try_default()
        .await
        .context("building kube client (in-cluster or kubeconfig)")?;

    let s3_store = build_s3_store()?;

    let cfg = WorkerConfig {
        tick_interval: Duration::from_secs(env_u64("ARCHIVE_TICK_INTERVAL_SECS", 60)),
        min_run_interval: Duration::from_secs(env_u64("ARCHIVE_MIN_RUN_INTERVAL_SECS", 300)),
        default_batch_size: env_u64("ARCHIVE_DEFAULT_BATCH_SIZE", 500) as usize,
        default_max_duration: Duration::from_secs(env_u64(
            "ARCHIVE_DEFAULT_MAX_DURATION_SECS",
            600,
        )),
        watch_namespace: std::env::var("WATCH_NAMESPACE").ok().filter(|s| !s.is_empty()),
        s3_store,
    };

    tracing::info!(
        component = "velocity-archive-worker",
        tick_interval_secs = cfg.tick_interval.as_secs(),
        min_run_interval_secs = cfg.min_run_interval.as_secs(),
        default_batch_size = cfg.default_batch_size,
        watch_namespace = ?cfg.watch_namespace,
        "starting archive worker"
    );

    run(pool, kube, cfg).await
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Build an `Arc<dyn ObjectStore>` when `ARCHIVE_S3_BUCKET` is set;
/// `None` otherwise (s3-destined ArchivePolicies are skipped with a
/// warning). Credentials come from the standard AWS SDK chain.
fn build_s3_store() -> Result<Option<std::sync::Arc<dyn object_store::ObjectStore>>> {
    let Some(bucket) = std::env::var("ARCHIVE_S3_BUCKET").ok().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let mut builder = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(&bucket);
    if let Ok(region) = std::env::var("AWS_REGION") {
        builder = builder.with_region(region);
    }
    let store = builder
        .build()
        .context("building AmazonS3 object_store")?;
    Ok(Some(std::sync::Arc::new(store)))
}
