use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use velocity_api::auth::{
    authenticate, AuthState, PgApiKeyChecker, PgSessionStore, RedisRevocationChecker,
};
use velocity_api::auth_handlers::{
    AuthHandlersState, EnvClientSecretResolver,
};
use velocity_api::{
    auth_informer, health, informer, router, startup, ApiConfig, AppState, AuthRegistry,
    JwksCache, SchemaRegistry,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // rustls 0.23 requires an explicit crypto provider before any TLS code
    // runs. kube-rs (via rustls-tls) pulls rustls in but doesn't pick a
    // provider for us.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))?;

    let cfg = ApiConfig::from_env()?;
    init_tracing(cfg.pretty_logs);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind_addr,
        health = %cfg.health_addr,
        watch_namespace = cfg.watch_namespace.as_deref().unwrap_or("<all>"),
        "velocity-api starting",
    );

    // ADR-007 startup gate.
    let pool = startup::pool_with_checks(&cfg).await?;

    // Kube client + schema registry fed by an informer.
    let client = kube::Client::try_default().await?;
    let (registry, ready_rx) = SchemaRegistry::new();

    let informer_registry = registry.clone();
    let informer_client = client.clone();
    let informer_ns = cfg.watch_namespace.clone();
    let informer_handle = tokio::spawn(async move {
        if let Err(e) = informer::run(informer_registry, informer_client, informer_ns).await {
            tracing::error!(error = %e, "schema informer terminated");
        }
    });

    // Auth: registry of AuthStrategy CRDs (parallel to the schema
    // registry), shared JWKS cache, and the PgApiKeyChecker for ApiKey-kind
    // strategies. Composite strategies are resolved at request time so they
    // don't need anything extra here.
    let auth_registry = AuthRegistry::new();
    let jwks_cache = JwksCache::new();
    let api_key_checker = Arc::new(PgApiKeyChecker::new(pool.clone()));
    let session_store = Arc::new(PgSessionStore::new(pool.clone()));
    let mut auth_state =
        AuthState::new(registry.clone(), auth_registry.clone(), jwks_cache.clone())
            .with_api_keys(api_key_checker)
            .with_sessions(session_store.clone());

    // ADR-003 actor revocation backend. Optional at boot — operators must
    // opt-in by setting `VELOCITY_API_REDIS_URL`. If absent, every request
    // is admitted from the revocation perspective and the warning below is
    // the only signal that the fail-mode matrix cannot apply. If present,
    // the checker is wired before the auth middleware sees its first
    // request; per-strategy `revocation_fail_open` then governs behaviour
    // when Redis is unreachable.
    match cfg.redis_url.as_deref() {
        Some(url) => {
            let checker = RedisRevocationChecker::connect(
                url,
                velocity_api::auth::DEFAULT_REVOKED_SET_KEY,
            )
            .await?;
            auth_state = auth_state.with_revocation(Arc::new(checker));
            tracing::info!("revocation backend connected");
        }
        None => {
            tracing::warn!(
                "VELOCITY_API_REDIS_URL not set — revocation backend disabled; \
                 actors in the revoked set will be admitted (ADR-003 fail-closed default cannot apply)"
            );
        }
    }

    let auth_informer_registry = auth_registry.clone();
    let auth_informer_state = auth_state.clone();
    let auth_informer_client = client.clone();
    let auth_informer_ns = cfg.watch_namespace.clone();
    let auth_informer_handle = tokio::spawn(async move {
        if let Err(e) = auth_informer::run(
            auth_informer_registry,
            auth_informer_state,
            auth_informer_client,
            auth_informer_ns,
        )
        .await
        {
            tracing::error!(error = %e, "auth strategy informer terminated");
        }
    });

    // Health server (separate listener — readiness gates on the registry).
    let health_addr = cfg.health_addr.clone();
    let health_ready_rx = ready_rx.clone();
    let health_handle =
        tokio::spawn(async move { health::serve(&health_addr, health_ready_rx).await });

    // OIDC flow-cookie HMAC key. Required only when at least one
    // `AuthStrategy` of kind `Oidc` is in play, but we always read it at
    // startup so a missing key fails loudly rather than at first
    // /auth/login. Must be at least 32 bytes; truncated/short values are
    // rejected with a hard error.
    let flow_cookie_key = match std::env::var("VELOCITY_API_FLOW_COOKIE_KEY") {
        Ok(s) if s.len() >= 32 => Arc::new(s.into_bytes()),
        Ok(_) => {
            anyhow::bail!(
                "VELOCITY_API_FLOW_COOKIE_KEY must be at least 32 bytes — refusing to start"
            );
        }
        Err(_) => {
            tracing::warn!(
                "VELOCITY_API_FLOW_COOKIE_KEY not set — /auth/login will reject every request"
            );
            // Use a zero-length placeholder so non-OIDC deployments aren't
            // forced to set it. `encode_flow_cookie` returns an error on a
            // too-short HMAC key, which surfaces as 500 on /auth/login —
            // never silently admitting an unsigned cookie.
            Arc::new(Vec::new())
        }
    };

    let auth_handlers_state = AuthHandlersState {
        auth_registry: auth_registry.clone(),
        sessions: session_store.clone(),
        flow_cookie_key,
        // The middleware and the callback share these — `prime_strategy`
        // populates `claim_mappings`, and the JWKS cache holds keys
        // fetched per-issuer. Reusing the same `Arc`s means the callback
        // can verify an ID token without re-priming.
        jwks: jwks_cache.clone(),
        claim_mappings: auth_state.claim_mappings.clone(),
        // OIDC token + JWKS calls go through this client. A hung IdP
        // must not block a request indefinitely — set bounded timeouts.
        // 10s overall is generous enough for slow IdPs (Okta cold-start)
        // and still well below the upstream request deadline.
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(3))
            .build()
            .context("building OIDC http client")?,
        client_secret_resolver: Arc::new(EnvClientSecretResolver),
    };

    // Tiered event reader for time-machine reads (Phase 4.4).
    // Hot tier is always wired; warm tier requires a configured
    // warm-reader URL + service token (config layer pairs them so we
    // can rely on both-or-neither here).
    let hot_reader: std::sync::Arc<dyn velocity_api::tiering::EventReader> =
        std::sync::Arc::new(velocity_api::tiering::PostgresEventReader::new(pool.clone()));
    let warm_reader: Option<std::sync::Arc<dyn velocity_api::tiering::EventReader>> =
        match (cfg.warm_reader_url.as_deref(), cfg.warm_reader_service_token.as_deref()) {
            (Some(url), Some(token)) => {
                match velocity_api::tiering::WarmEventReader::new(
                    url,
                    token,
                    std::time::Duration::from_millis(cfg.warm_reader_timeout_ms),
                ) {
                    Ok(r) => {
                        tracing::info!(warm_reader_url = %url, "warm-tier reader wired");
                        Some(std::sync::Arc::new(r))
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "warm-tier reader could not be initialised — warm requests will return WARM_TIER_NOT_CONFIGURED");
                        None
                    }
                }
            }
            _ => {
                tracing::warn!("warm-tier reader not configured — time-machine reads older than the hot window will fail with WARM_TIER_NOT_CONFIGURED");
                None
            }
        };
    let tiered_reader = std::sync::Arc::new(velocity_api::tiering::TieredEventReader::new(
        hot_reader,
        warm_reader,
    ));
    let cold_jobs = velocity_api::tiering::cold_stub::ColdJobStore::new();

    // Public API. The base `new` constructs a hot-only state with
    // sensible defaults — keeps tests compiling — and `with_tiering`
    // injects the real warm-tier impl in production.
    let mut state = AppState::new(registry, pool).with_tiering(tiered_reader, cold_jobs);
    if let Some(key) = cfg.cursor_signing_key.clone() {
        match velocity_api::dsl::CursorSigner::new(key) {
            Ok(s) => {
                tracing::info!("query cursor signer configured");
                state = state.with_cursor_signer(std::sync::Arc::new(s));
            }
            Err(e) => anyhow::bail!("cursor signing key: {e}"),
        }
    } else {
        tracing::warn!(
            "VELOCITY_API_CURSOR_SIGNING_KEY is unset — POST /query will not mint cursors; cursor-bearing requests will 400"
        );
    }
    // Phase 5c — Typesense client + CDC loop. Both optional. When
    // unset, Tier-3 schemas still apply but outbox rows accumulate
    // and /search returns 503 — the failure is loud, not silent.
    let (cdc_shutdown_tx, cdc_shutdown_rx) = tokio::sync::watch::channel(false);
    let _cdc_handle = match (cfg.typesense_url.as_deref(), cfg.typesense_api_key.as_deref()) {
        (Some(url), Some(key)) => {
            let ts = std::sync::Arc::new(
                velocity_api::typesense::TypesenseClient::new(url, key)
                    .context("building typesense client")?,
            );
            match ts.health().await {
                Ok(true) => tracing::info!(url, "typesense reachable"),
                Ok(false) => tracing::warn!(url, "typesense health endpoint returned non-200"),
                Err(e) => tracing::warn!(url, error = %e, "typesense health check failed — CDC will retry"),
            }
            state = state.with_typesense(ts.clone());
            let cdc_pool = state.pool.clone();
            let cdc_registry = state.registry.clone();
            let cdc_ts = ts;
            Some(tokio::spawn(async move {
                velocity_api::cdc::run(cdc_pool, cdc_registry, cdc_ts, cdc_shutdown_rx).await;
            }))
        }
        _ => {
            tracing::warn!(
                "VELOCITY_API_TYPESENSE_URL/KEY unset — Tier-3 CDC disabled; /search returns 503"
            );
            None
        }
    };
    let _cdc_shutdown_tx = cdc_shutdown_tx;
    let app = router::build(state)
        .merge(router::build_auth(auth_handlers_state))
        .layer(axum::middleware::from_fn_with_state(auth_state, authenticate));
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "API server listening");

    // `into_make_service_with_connect_info::<SocketAddr>` is required so the
    // API-key IP allowlist can read `ConnectInfo<SocketAddr>` off the
    // request extensions. Without it the middleware would deny every
    // request that has an `ipAllowlist` set on its ApiKey CRD.
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );

    tokio::select! {
        r = serve => match r {
            Ok(()) => tracing::warn!("API server exited cleanly"),
            Err(e) => tracing::error!(error = %e, "API server failed"),
        },
        r = health_handle => match r {
            Ok(Ok(())) => tracing::warn!("health server exited cleanly"),
            Ok(Err(e)) => tracing::error!(error = %e, "health server failed"),
            Err(e)     => tracing::error!(error = %e, "health server panicked"),
        },
        r = informer_handle => match r {
            Ok(()) => tracing::warn!("schema informer task exited"),
            Err(e) => tracing::error!(error = %e, "schema informer task panicked"),
        },
        r = auth_informer_handle => match r {
            Ok(()) => tracing::warn!("auth informer task exited"),
            Err(e) => tracing::error!(error = %e, "auth informer task panicked"),
        },
    };

    Ok(())
}

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_api=debug,kube=info"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}
