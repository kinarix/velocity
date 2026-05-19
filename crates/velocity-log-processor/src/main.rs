//! velocity-log-processor binary.
//!
//! Wires the env-driven `ProcessorConfig` to:
//! - a periodic policy reloader that re-reads the YAML bundle on disk
//!   and atomically swaps it into the server's `AppState`,
//! - the axum receiver from `server::serve`.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use velocity_log_processor::destination::build_all;
use velocity_log_processor::policy::LogPolicyBundle;
use velocity_log_processor::server::{serve, AppState, PolicySnapshot, Stats};
use velocity_log_processor::ProcessorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().init();
    let cfg = ProcessorConfig::from_env()?;
    tracing::info!(component = "velocity-log-processor",
                   bind = %cfg.bind_addr,
                   policy = %cfg.policy_path.display(),
                   reload_secs = cfg.policy_reload_secs,
                   "starting");

    let initial = load_snapshot(&cfg.policy_path).await?;
    let current = Arc::new(arc_swap::ArcSwap::from(Arc::new(initial)));
    let ready = Arc::new(AtomicBool::new(true));

    let state = AppState {
        current: current.clone(),
        token: Arc::new(cfg.ingest_token.clone()),
        ready,
        stats: Arc::new(Stats::default()),
    };

    // Background reloader. Polling beats inotify here: we don't need
    // sub-second reload latency, polling has zero platform surface,
    // and the ConfigMap mount latency is its own multi-second window.
    let reload_path = cfg.policy_path.clone();
    let reload_secs = cfg.policy_reload_secs;
    tokio::spawn(async move {
        let mut last: Option<LogPolicyBundle> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(reload_secs)).await;
            match load_snapshot(&reload_path).await {
                Ok(snap) => {
                    if last.as_ref() != Some(&snap.bundle) {
                        tracing::info!(
                            filters = snap.bundle.filters.len(),
                            destinations = snap.destinations.len(),
                            "policy bundle reloaded"
                        );
                        last = Some(snap.bundle.clone());
                        current.store(Arc::new(snap));
                    }
                }
                Err(e) => tracing::warn!(error = %e, "policy reload failed; keeping previous bundle"),
            }
        }
    });

    let addr: std::net::SocketAddr = cfg.bind_addr.parse()?;
    serve(addr, state).await
}

async fn load_snapshot(path: &Path) -> Result<PolicySnapshot> {
    let mut bundle = LogPolicyBundle::load_or_empty(path).await?;
    bundle.sort_filters();
    let destinations = build_all(&bundle.destinations);
    // Ensure at least one destination — stdout fallback — so a fresh
    // processor with no policy still tees instead of silently
    // black-holing kept lines.
    let destinations = if destinations.is_empty() {
        build_all(&[velocity_log_processor::LogRoutingDestSpec {
            name: "stdout".into(),
            kind: "stdout".into(),
            config: Default::default(),
        }])
    } else {
        destinations
    };
    Ok(PolicySnapshot { bundle, destinations })
}
