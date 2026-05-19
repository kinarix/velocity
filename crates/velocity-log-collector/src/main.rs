//! velocity-log-collector binary.

use anyhow::Result;
use velocity_log_collector::{Collector, CollectorConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().init();
    let cfg = CollectorConfig::from_env()?;
    tracing::info!(
        component = "velocity-log-collector",
        log_root = %cfg.log_root.display(),
        endpoint = %cfg.processor_endpoint,
        scan_secs = cfg.scan_interval.as_secs(),
        "starting"
    );
    let c = Collector::new(cfg)?;
    c.run().await
}
