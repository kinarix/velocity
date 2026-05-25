//! Data-plane startup wiring extracted from the shared bootstrap.
//!
//! `build_tiered_reader` wires the hot + optional warm + cold-stub reader
//! tower so handlers don't need to know about tier selection. The ADR-007
//! pool gate and the OIDC/auth wiring live in the shared core
//! (`velocity_core::startup` / `velocity_core::server::bootstrap_common`).

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use velocity_core::ApiConfig;

use crate::tiering::{
    cold_stub::ColdJobStore, EventReader, PostgresEventReader, TieredEventReader, WarmEventReader,
};

/// Build the tiered event-reader tower (Phase 4.4): hot (Postgres),
/// optional warm (HTTP to `velocity-warm-reader`), plus the cold-stub
/// store. Returns the reader + the cold-job store so the state wire-up has
/// both pieces in one call.
///
/// Warm tier requires both URL + service token; the config layer pairs
/// them, so one-without-the-other never reaches this function.
pub fn build_tiered_reader(
    cfg: &ApiConfig,
    pool: PgPool,
) -> (Arc<TieredEventReader>, Arc<ColdJobStore>) {
    let hot: Arc<dyn EventReader> = Arc::new(PostgresEventReader::new(pool));
    let warm: Option<Arc<dyn EventReader>> =
        match (cfg.warm_reader_url.as_deref(), cfg.warm_reader_service_token.as_deref()) {
            (Some(url), Some(token)) => {
                match WarmEventReader::new(
                    url,
                    token,
                    Duration::from_millis(cfg.warm_reader_timeout_ms),
                ) {
                    Ok(r) => {
                        tracing::info!(warm_reader_url = %url, "warm-tier reader wired");
                        Some(Arc::new(r))
                    }
                    Err(e) => {
                        tracing::error!(
                            error = ?e,
                            "warm-tier reader could not be initialised — warm requests will return WARM_TIER_NOT_CONFIGURED"
                        );
                        None
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "warm-tier reader not configured — time-machine reads older than the hot window will fail with WARM_TIER_NOT_CONFIGURED"
                );
                None
            }
        };
    (Arc::new(TieredEventReader::new(hot, warm)), ColdJobStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> ApiConfig {
        ApiConfig {
            pg_url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            bind_addr: "0.0.0.0:8080".into(),
            health_addr: "0.0.0.0:8081".into(),
            watch_namespace: None,
            watch_label_selector: None,
            pg_pool_max: 1,
            pretty_logs: false,
            redis_url: None,
            warm_reader_url: None,
            warm_reader_service_token: None,
            warm_reader_timeout_ms: 15_000,
            cursor_signing_key: None,
            typesense_url: None,
            typesense_api_key: None,
            platform_audit_token: None,
            auth_mode: velocity_core::config::AuthMode::Enforced,
            api_mode: velocity_core::config::ApiMode::Platform,
        }
    }

    fn lazy_pool() -> PgPool {
        use sqlx::pool::PoolOptions;
        use sqlx::postgres::PgConnectOptions;
        use std::str::FromStr;
        let opts = PgConnectOptions::from_str("postgres://stub:stub@127.0.0.1:1/stub").unwrap();
        PoolOptions::new().acquire_timeout(Duration::from_millis(200)).connect_lazy_with(opts)
    }

    #[tokio::test]
    async fn build_tiered_reader_without_warm_config_returns_hot_only() {
        let cfg = base_cfg();
        let (_reader, cold) = build_tiered_reader(&cfg, lazy_pool());
        assert!(cold.get(uuid::Uuid::new_v4()).is_none());
    }

    #[tokio::test]
    async fn build_tiered_reader_with_warm_config_attaches_warm_reader() {
        let mut cfg = base_cfg();
        cfg.warm_reader_url = Some("http://warm-reader.test:9090".into());
        cfg.warm_reader_service_token = Some("a-test-token".into());
        cfg.warm_reader_timeout_ms = 5_000;
        let (_reader, _cold) = build_tiered_reader(&cfg, lazy_pool());
    }

    #[tokio::test]
    async fn build_tiered_reader_with_partial_warm_config_skips_warm() {
        let mut cfg = base_cfg();
        cfg.warm_reader_url = Some("http://warm".into());
        let (_reader, _cold) = build_tiered_reader(&cfg, lazy_pool());
    }
}
