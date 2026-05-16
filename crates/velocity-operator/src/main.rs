use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use kube::api::Api;
use kube::runtime::watcher::Config as WatcherConfig;
use kube::runtime::Controller;
use kube::Client;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;
use velocity_operator::{
    controllers::{application, domain, organisation},
    health, startup, Context, OperatorConfig,
};
use velocity_types::crds::{Application, Domain, Organisation};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = OperatorConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        leader_election = cfg.leader_election,
        watch_namespace = cfg.watch_namespace.as_deref().unwrap_or("<all>"),
        "velocity-operator starting",
    );

    if cfg.leader_election {
        // Placeholder — real lease-based election lands when we move beyond
        // a single replica. The deployment manifest in charts/ currently
        // pins replicas=1, so this is safe to defer.
        tracing::warn!("leader_election=true is a no-op in Phase 0");
    }

    // Startup gates (ADR-007 + platform schema present).
    let pg = startup::pool_with_checks(&cfg).await?;

    // kube client.
    let kube = Client::try_default().await?;
    tracing::info!("kube client initialised");

    // Readiness signal — flipped once all controllers have seen their first
    // informer sync (we approximate this by setting `true` right after we
    // start spawning controllers; a stricter version waits for the first
    // Restart event from each watcher).
    let (ready_tx, ready_rx) = watch::channel(false);

    let ctx = Arc::new(Context::new(kube.clone(), pg, ready_tx));

    // Health server.
    let health_addr = cfg.health_addr.clone();
    let health_ready_rx = ready_rx.clone();
    let health_handle =
        tokio::spawn(async move { health::serve(&health_addr, health_ready_rx).await });

    let watcher_cfg = WatcherConfig::default();
    let org_api: Api<Organisation> = match &cfg.watch_namespace {
        Some(ns) => Api::namespaced(kube.clone(), ns),
        None => Api::all(kube.clone()),
    };
    let app_api: Api<Application> = match &cfg.watch_namespace {
        Some(ns) => Api::namespaced(kube.clone(), ns),
        None => Api::all(kube.clone()),
    };
    let domain_api: Api<Domain> = match &cfg.watch_namespace {
        Some(ns) => Api::namespaced(kube.clone(), ns),
        None => Api::all(kube.clone()),
    };

    let org_ctx = ctx.clone();
    let app_ctx = ctx.clone();
    let dom_ctx = ctx.clone();

    let org_fut = Controller::new(org_api, watcher_cfg.clone())
        .shutdown_on_signal()
        .run(organisation::reconcile, organisation::error_policy, org_ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "organisation controller stream error");
            }
        });

    let app_fut = Controller::new(app_api, watcher_cfg.clone())
        .shutdown_on_signal()
        .run(application::reconcile, application::error_policy, app_ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "application controller stream error");
            }
        });

    let dom_fut = Controller::new(domain_api, watcher_cfg)
        .shutdown_on_signal()
        .run(domain::reconcile, domain::error_policy, dom_ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "domain controller stream error");
            }
        });

    // Mark ready — controllers are running. (A future refinement: gate on
    // the first Restart event from each watcher.)
    let _ = ctx.ready_tx.send(true);
    tracing::info!("controllers running; readyz flipped to ready");

    tokio::select! {
        _ = org_fut => tracing::warn!("organisation controller exited"),
        _ = app_fut => tracing::warn!("application controller exited"),
        _ = dom_fut => tracing::warn!("domain controller exited"),
        r = health_handle => match r {
            Ok(Ok(())) => tracing::warn!("health server exited cleanly"),
            Ok(Err(e)) => tracing::error!(error = %e, "health server failed"),
            Err(e)     => tracing::error!(error = %e, "health server panicked"),
        },
    };

    Ok(())
}

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_operator=debug,kube=info"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}
