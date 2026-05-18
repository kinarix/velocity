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
    controllers::{application, domain, organisation, role_binding, schema_definition},
    drift_sweep, health, partition_manager, startup, tiering, Context, OperatorConfig,
    RedisNotify,
};
use velocity_types::crds::{Application, Domain, Organisation, RoleBinding, SchemaDefinition};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // rustls 0.23 requires an explicit crypto provider before any TLS code
    // runs. kube-rs (via rustls-tls) pulls rustls in but doesn't pick a
    // provider for us, so install aws-lc-rs here.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))?;

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

    // Wire the Redis revocation publisher if a URL was configured. We log
    // (don't fail) on connection error so a Redis outage at boot doesn't
    // prevent the operator from reconciling everything *else*; the
    // RoleBinding controller will surface the absence loudly per-event.
    let redis = match cfg.redis_url.as_ref() {
        Some(url) => match RedisNotify::connect(url, cfg.redis_revoked_key.clone()).await {
            Ok(r) => {
                tracing::info!(key = %cfg.redis_revoked_key, "redis revocation publisher connected");
                Some(r)
            }
            Err(e) => {
                tracing::error!(error = %e, "redis revocation publisher failed to connect — RoleBinding reconciles will be DB-only until restart");
                None
            }
        },
        None => {
            tracing::warn!(
                "VELOCITY_OPERATOR_REDIS_URL is unset — RoleBinding reconciles will not push revocations to Redis"
            );
            None
        }
    };

    let mut ctx_inner = Context::new(kube.clone(), pg, ready_tx);
    if let Some(r) = redis {
        ctx_inner = ctx_inner.with_redis(r);
    }
    let ctx = Arc::new(ctx_inner);

    // Health server.
    let health_addr = cfg.health_addr.clone();
    let health_ready_rx = ready_rx.clone();
    let health_handle =
        tokio::spawn(async move { health::serve(&health_addr, health_ready_rx).await });

    // Event-log partition manager (Phase 3.8). Runs forever, ticks
    // hourly; its only job is to make sure next month's
    // platform.event_log partition exists before the boundary so
    // mutations don't fail with "no partition of relation found" at
    // midnight on the 1st. Detached because controllers and the
    // partition loop have no shared state.
    let partition_pool = ctx.pg.clone();
    let _partition_handle = tokio::spawn(async move { partition_manager::run(partition_pool).await });

    // Hourly drift sweep (Phase 4.5). Compares declared SchemaDefinition
    // CRDs against `pg_class` and increments
    // `velocity_drift_detected_total{kind="orphan_table"}` per orphan
    // detected. Read-only — no auto-fix; humans run `velocity drift
    // quarantine` after triage.
    let (drift_shutdown_tx, drift_shutdown_rx) = tokio::sync::watch::channel(false);
    let drift_pool = ctx.pg.clone();
    let drift_client = ctx.kube.clone();
    let _drift_handle = tokio::spawn(async move {
        if let Err(e) = drift_sweep::run(drift_pool, drift_client, drift_shutdown_rx).await {
            tracing::error!(error = %e, "drift sweep exited");
        }
    });
    // Keep the tx alive for the lifetime of `main`; on process exit
    // its drop signals the sweep to wind down. Tagged `_` to silence
    // unused-var while making the lifetime explicit.
    let _drift_shutdown_tx = drift_shutdown_tx;

    // Hot → warm tier exporter (Phase 4.2). Skipped silently when
    // `VELOCITY_OPERATOR_WARM_STORAGE_URL` is unset — the platform
    // runs hot-only and partitions accumulate. We log loudly so a
    // forgotten config doesn't go unnoticed.
    let _tiering_handle = if let Some(url) = cfg.warm_storage_url.as_deref() {
        match tiering::object_store_url::build(url) {
            Ok(warm_store) => {
                tracing::info!(warm_storage_url = %url, "tiering exporter wired");
                // One-shot orphan scan before the exporter loop spins
                // up — surfaces drift without modifying state. Failure
                // here is logged but non-fatal (the regular tick still
                // converges).
                let scan_store = warm_store.clone();
                let scan_pool = ctx.pg.clone();
                tokio::spawn(async move {
                    if let Err(e) = tiering::orphan_recovery::scan(&scan_pool, scan_store).await {
                        tracing::warn!(error = %e, "orphan scan failed (non-fatal)");
                    }
                });
                let pool = ctx.pg.clone();
                Some(tokio::spawn(async move { tiering::exporter::run(pool, warm_store).await }))
            }
            Err(e) => {
                tracing::error!(error = %e, warm_storage_url = %url, "tiering exporter disabled — could not build object store");
                None
            }
        }
    } else {
        tracing::warn!(
            "VELOCITY_OPERATOR_WARM_STORAGE_URL is unset — tiering exporter disabled; hot partitions will accumulate"
        );
        None
    };

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
    let sd_api: Api<SchemaDefinition> = match &cfg.watch_namespace {
        Some(ns) => Api::namespaced(kube.clone(), ns),
        None => Api::all(kube.clone()),
    };
    let rb_api: Api<RoleBinding> = match &cfg.watch_namespace {
        Some(ns) => Api::namespaced(kube.clone(), ns),
        None => Api::all(kube.clone()),
    };

    let org_ctx = ctx.clone();
    let app_ctx = ctx.clone();
    let dom_ctx = ctx.clone();
    let sd_ctx = ctx.clone();
    let rb_ctx = ctx.clone();

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

    let dom_fut = Controller::new(domain_api, watcher_cfg.clone())
        .shutdown_on_signal()
        .run(domain::reconcile, domain::error_policy, dom_ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "domain controller stream error");
            }
        });

    let sd_fut = Controller::new(sd_api, watcher_cfg.clone())
        .shutdown_on_signal()
        .run(schema_definition::reconcile, schema_definition::error_policy, sd_ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "schemadefinition controller stream error");
            }
        });

    let rb_fut = Controller::new(rb_api, watcher_cfg)
        .shutdown_on_signal()
        .run(role_binding::reconcile, role_binding::error_policy, rb_ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "rolebinding controller stream error");
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
        _ = sd_fut  => tracing::warn!("schemadefinition controller exited"),
        _ = rb_fut  => tracing::warn!("rolebinding controller exited"),
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
