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
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Ok(Self {
            tls_addr: get("VELOCITY_WEBHOOK_TLS_ADDR")
                .unwrap_or_else(|| "0.0.0.0:8443".to_string()),
            health_addr: get("VELOCITY_WEBHOOK_HEALTH_ADDR")
                .unwrap_or_else(|| "0.0.0.0:8080".to_string()),
            tls_cert_path: get("VELOCITY_WEBHOOK_TLS_CERT"),
            tls_key_path: get("VELOCITY_WEBHOOK_TLS_KEY"),
            pretty_logs: parse_bool(get("VELOCITY_WEBHOOK_PRETTY_LOGS").as_deref(), false),
            multi_tenant_mode: parse_bool(
                get("VELOCITY_WEBHOOK_MULTI_TENANT_MODE").as_deref(),
                false,
            ),
        })
    }
}

fn parse_bool(v: Option<&str>, default: bool) -> bool {
    match v {
        Some(s) => s == "1" || s.eq_ignore_ascii_case("true"),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| map.get(k).map(|s| s.to_string())
    }

    #[test]
    fn defaults_are_safe_when_env_empty() {
        let env: HashMap<&str, &str> = HashMap::new();
        let cfg = WebhookConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.tls_addr, "0.0.0.0:8443");
        assert_eq!(cfg.health_addr, "0.0.0.0:8080");
        assert!(cfg.tls_cert_path.is_none());
        assert!(cfg.tls_key_path.is_none());
        assert!(!cfg.pretty_logs);
        assert!(!cfg.multi_tenant_mode);
    }

    #[test]
    fn explicit_env_values_propagate() {
        let mut env = HashMap::new();
        env.insert("VELOCITY_WEBHOOK_TLS_ADDR", "0.0.0.0:9443");
        env.insert("VELOCITY_WEBHOOK_HEALTH_ADDR", "0.0.0.0:9080");
        env.insert("VELOCITY_WEBHOOK_TLS_CERT", "/certs/tls.crt");
        env.insert("VELOCITY_WEBHOOK_TLS_KEY", "/certs/tls.key");
        env.insert("VELOCITY_WEBHOOK_PRETTY_LOGS", "1");
        env.insert("VELOCITY_WEBHOOK_MULTI_TENANT_MODE", "true");
        let cfg = WebhookConfig::from_env_with(lookup(&env)).unwrap();
        assert_eq!(cfg.tls_addr, "0.0.0.0:9443");
        assert_eq!(cfg.health_addr, "0.0.0.0:9080");
        assert_eq!(cfg.tls_cert_path.as_deref(), Some("/certs/tls.crt"));
        assert_eq!(cfg.tls_key_path.as_deref(), Some("/certs/tls.key"));
        assert!(cfg.pretty_logs);
        assert!(cfg.multi_tenant_mode);
    }

    #[test]
    fn parse_bool_handles_truthy_falsy_and_missing() {
        assert!(parse_bool(Some("1"), false));
        assert!(parse_bool(Some("true"), false));
        assert!(parse_bool(Some("TRUE"), false));
        assert!(!parse_bool(Some("0"), false));
        assert!(!parse_bool(Some("anything-else"), false));
        assert!(parse_bool(None, true), "missing should use default true");
        assert!(!parse_bool(None, false), "missing should use default false");
    }

    #[test]
    fn from_env_wrapper_invokes_std_env_var() {
        let _ = WebhookConfig::from_env();
    }
}
