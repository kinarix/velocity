//! Shared server bootstrap for the velocity-api family of binaries
//! (Phase 12a / ADR-011 "Final service topology").
//!
//! `velocity-api` is a library; the runnable services —
//! `velocity-platform-api`, `velocity-data-api`, `velocity-search` — are thin
//! binary crates that each call [`bootstrap_common`] for the setup every tier
//! shares (Postgres pool, kube client, schema registry + informer, auth stack,
//! health server) and then assemble their own router on top.

use std::sync::Arc;

use anyhow::Result;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

use crate::auth::{AuthState, PgApiKeyChecker, PgSessionStore, RedisRevocationChecker};
use crate::auth_handlers::{AuthHandlersState, EnvClientSecretResolver};
use crate::{
    auth_informer, health, informer, startup, ApiConfig, AuthRegistry, JwksCache, SchemaRegistry,
};

/// rustls 0.23 requires an explicit crypto provider before any TLS code runs.
/// kube-rs (via rustls-tls) pulls rustls in but doesn't pick a provider.
pub fn install_crypto_provider() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))
}

/// JSON logs in-cluster; pretty logs when `pretty` (local dev).
pub fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,velocity_core=debug,kube=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if pretty {
        builder.init();
    } else {
        builder.json().init();
    }
}

/// Everything the three API tiers share, plus the background task handles to
/// select on. Each binary builds its own `AppState`/router from `registry` +
/// `pool` and adds tier-specific pieces (tiering, cursor signer, Typesense,
/// CDC, admin endpoints, static UI).
// kube::Client isn't Debug, and these are live handles/clients, not data —
// a Debug impl would add nothing.
#[allow(missing_debug_implementations)]
pub struct Common {
    pub pool: PgPool,
    pub client: kube::Client,
    pub registry: Arc<SchemaRegistry>,
    pub ready_rx: watch::Receiver<bool>,
    pub auth_state: AuthState,
    pub auth_handlers_state: AuthHandlersState,
    pub anonymous_auth: bool,
    pub informer_handle: JoinHandle<()>,
    pub auth_informer_handle: JoinHandle<()>,
    pub health_handle: JoinHandle<Result<()>>,
}

/// Shared startup: ADR-007 pool gate, kube client, schema registry fed by an
/// informer (namespace-scoped in data mode), the full auth stack (JWKS, API
/// keys, sessions, revocation, anonymous-mode bypass per Phase 12b), the auth
/// strategy informer, and the health/readiness server.
pub async fn bootstrap_common(cfg: &ApiConfig) -> Result<Common> {
    // ADR-007 startup gate.
    let pool = startup::pool_with_checks(cfg).await?;

    // Kube client + schema registry fed by an informer.
    let client = kube::Client::try_default().await?;
    let (registry, ready_rx) = SchemaRegistry::new();

    let informer_registry = registry.clone();
    let informer_client = client.clone();
    let informer_ns = cfg.watch_namespace.clone();
    let informer_sel = cfg.watch_label_selector.clone();
    let informer_handle = tokio::spawn(async move {
        if let Err(e) =
            informer::run(informer_registry, informer_client, informer_ns, informer_sel).await
        {
            tracing::error!(error = %e, "schema informer terminated");
        }
    });

    // Auth: AuthStrategy registry, shared JWKS cache, ApiKey checker, session
    // store. Composite strategies resolve at request time.
    let auth_registry = AuthRegistry::new();
    let jwks_cache = JwksCache::new();
    let api_key_checker = Arc::new(PgApiKeyChecker::new(pool.clone()));
    let session_store = Arc::new(PgSessionStore::new(pool.clone()));
    let mut auth_state =
        AuthState::new(registry.clone(), auth_registry.clone(), jwks_cache.clone())
            .with_api_keys(api_key_checker)
            .with_sessions(session_store.clone())
            .with_audit_pool(Arc::new(pool.clone()))
            .with_auth_mode(cfg.auth_mode);

    // Phase 12b: anonymous bypass is loud at startup + on the gauge.
    let anonymous_auth = cfg.auth_mode == crate::AuthMode::Anonymous;
    crate::metrics::auth_anonymous_mode().set(i64::from(anonymous_auth));
    if anonymous_auth {
        tracing::warn!(
            "╔═ AUTH BYPASS ═╗ VELOCITY_API_AUTH_MODE=anonymous — authentication is DISABLED \
             for all data-plane requests (actor=anonymous). Test mode only; never in production."
        );
    }

    // ADR-003 actor revocation backend (optional; fail-closed default cannot
    // apply when absent — logged).
    match cfg.redis_url.as_deref() {
        Some(url) => {
            let checker =
                RedisRevocationChecker::connect(url, crate::auth::DEFAULT_REVOKED_SET_KEY).await?;
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
    let auth_informer_sel = cfg.watch_label_selector.clone();
    let auth_informer_handle = tokio::spawn(async move {
        if let Err(e) = auth_informer::run(
            auth_informer_registry,
            auth_informer_state,
            auth_informer_client,
            auth_informer_ns,
            auth_informer_sel,
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
        tokio::spawn(async move { health::serve(&health_addr, health_ready_rx, anonymous_auth).await });

    // OIDC flow-cookie HMAC key — read at startup so a missing key fails loud.
    let flow_cookie_key = startup::parse_flow_cookie_key(|k| std::env::var(k).ok())?;
    let auth_handlers_state = AuthHandlersState {
        auth_registry: auth_registry.clone(),
        sessions: session_store.clone(),
        flow_cookie_key,
        jwks: jwks_cache.clone(),
        claim_mappings: auth_state.claim_mappings.clone(),
        http: startup::build_oidc_http_client()?,
        client_secret_resolver: Arc::new(EnvClientSecretResolver),
    };

    Ok(Common {
        pool,
        client,
        registry,
        ready_rx,
        auth_state,
        auth_handlers_state,
        anonymous_auth,
        informer_handle,
        auth_informer_handle,
        health_handle,
    })
}
