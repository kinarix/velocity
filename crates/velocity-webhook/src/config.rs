//! Env-driven webhook configuration.

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// `host:port` for the TLS admission endpoint.
    pub tls_addr: String,
    /// `host:port` for the plain-HTTP health endpoint.
    pub health_addr: String,
    /// PEM-encoded TLS certificate path (cert-manager mounts this).
    pub tls_cert_path: Option<String>,
    /// PEM-encoded TLS private key path.
    pub tls_key_path: Option<String>,
    /// Pretty logs (dev) vs JSON (prod).
    pub pretty_logs: bool,
    /// Multi-tenant cluster (ADR-010). When `true`, the webhook rejects any
    /// `SchemaDefinition` whose fields reference a target schema in a
    /// different org — a cross-tenant data path the admission gate must
    /// shut down before it reaches the operator. Single-tenant clusters
    /// leave this off.
    pub multi_tenant_mode: bool,
}

impl WebhookConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            tls_addr: std::env::var("VELOCITY_WEBHOOK_TLS_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8443".to_string()),
            health_addr: std::env::var("VELOCITY_WEBHOOK_HEALTH_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            tls_cert_path: std::env::var("VELOCITY_WEBHOOK_TLS_CERT").ok(),
            tls_key_path: std::env::var("VELOCITY_WEBHOOK_TLS_KEY").ok(),
            pretty_logs: parse_bool_env("VELOCITY_WEBHOOK_PRETTY_LOGS", false),
            multi_tenant_mode: parse_bool_env("VELOCITY_WEBHOOK_MULTI_TENANT_MODE", false),
        })
    }
}

fn parse_bool_env(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(default)
}
