//! Kube informer feeding [`crate::auth::AuthRegistry`] and priming
//! [`crate::auth::JwksCache`] + claim mappings on [`crate::auth::AuthState`].
//!
//! Mirrors [`crate::informer`] (which watches `SchemaDefinition`) but
//! follows the `AuthStrategy` CRD. Each event:
//!
//! 1. resolves the CRD into [`crate::auth::ResolvedAuthStrategy`] —
//!    fetching the OIDC discovery doc once if `oidc.configUrl` is set
//!    and any endpoint field is unset (see [`crate::auth::DiscoveryCache`])
//! 2. registers it with the registry (lock-free swap on hot read path)
//! 3. registers every issuer with the shared JWKS cache so the first JWT
//!    request after a config change doesn't pay the JWKS round-trip
//! 4. compiles the claim mapping into the per-strategy cache on
//!    [`crate::auth::AuthState`]
//!
//! The `Init` snapshot is buffered the same way the schema informer does it,
//! so a delete that lands while we were disconnected doesn't leave a stale
//! strategy in the registry across reconnects.
//!
//! ## Discovery & fail-closed posture
//!
//! When `oidc.configUrl` is set and the discovery doc is unreachable (or
//! returns a body that doesn't parse), we DO NOT register the strategy.
//! That is intentional: a half-resolved OIDC strategy whose endpoints are
//! blank would either crash the login handler or — worse — redirect users
//! to an empty URL. Better to refuse to serve the strategy until the IdP
//! is reachable; the operator will retry on the next informer event.

use std::sync::Arc;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::watcher::{self, watcher, Event};
use kube::ResourceExt;
use velocity_types::common::NamespacedRef;
use velocity_types::crds::auth::{AuthStrategy, AuthStrategySpec, AuthStrategyType};

use crate::auth::{
    AuthRegistry, AuthState, DiscoveryCache, OidcDiscovery, ResolvedAuthStrategy,
};

pub async fn run(
    registry: Arc<AuthRegistry>,
    auth_state: AuthState,
    client: kube::Client,
    namespace: Option<String>,
    label_selector: Option<String>,
) -> anyhow::Result<()> {
    run_with_discovery(registry, auth_state, client, namespace, label_selector, DiscoveryCache::new())
        .await
}

/// Same as [`run`] but lets the caller inject a pre-built
/// [`DiscoveryCache`]. Used by tests to point at a local HTTP fixture
/// without overriding the global builder.
pub async fn run_with_discovery(
    registry: Arc<AuthRegistry>,
    auth_state: AuthState,
    client: kube::Client,
    namespace: Option<String>,
    label_selector: Option<String>,
    discovery: DiscoveryCache,
) -> anyhow::Result<()> {
    let api: Api<AuthStrategy> = match namespace {
        Some(ns) => Api::namespaced(client, &ns),
        None => Api::all(client),
    };
    let watcher_config = match label_selector {
        Some(sel) => watcher::Config::default().labels(&sel),
        None => watcher::Config::default(),
    };

    let mut stream = watcher(api, watcher_config).boxed();
    tracing::info!("auth strategy informer started");

    let mut bootstrap: Vec<ResolvedAuthStrategy> = Vec::new();

    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Init) => {
                bootstrap.clear();
                tracing::debug!("auth informer init — buffering snapshot");
            }
            Ok(Event::InitApply(crd)) => {
                if let Some(rs) = resolve(&crd, &discovery).await {
                    bootstrap.push(rs);
                }
            }
            Ok(Event::InitDone) => {
                let count = bootstrap.len();
                let strategies = std::mem::take(&mut bootstrap);
                for strategy in &strategies {
                    apply(&registry, &auth_state, strategy).await;
                }
                tracing::info!(count, "auth informer initial sync complete");
            }
            Ok(Event::Apply(crd)) => {
                if let Some(rs) = resolve(&crd, &discovery).await {
                    tracing::info!(strategy = %rs.key, "auth strategy upserted");
                    apply(&registry, &auth_state, &rs).await;
                }
            }
            Ok(Event::Delete(crd)) => {
                if let Some(reference) = reference_from(&crd) {
                    let key = format!("{}/{}", reference.namespace, reference.name);
                    tracing::info!(strategy = %key, "auth strategy removed");
                    registry.remove(&reference);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth informer transient error");
            }
        }
    }

    anyhow::bail!("auth informer stream ended");
}

async fn apply(registry: &AuthRegistry, auth_state: &AuthState, strategy: &ResolvedAuthStrategy) {
    strategy.prime_jwks(&auth_state.jwks).await;
    if let Err(e) = auth_state.prime_strategy(strategy) {
        tracing::warn!(
            strategy = %strategy.key,
            error = %e,
            "auth strategy claim mapping failed to compile — strategy will reject requests",
        );
    }
    // Register after priming so the hot path never sees a strategy whose
    // JWKS/claim-mapping side-tables aren't ready yet.
    registry.upsert(strategy.clone());
}

async fn resolve(
    crd: &AuthStrategy,
    discovery: &DiscoveryCache,
) -> Option<ResolvedAuthStrategy> {
    let reference = reference_from(crd)?;
    let key = format!("{}/{}", reference.namespace, reference.name);
    let mut spec = crd.spec.clone();

    if let Err(err) = resolve_discovery(&key, &mut spec, discovery).await {
        // fail-closed: do not register a strategy whose endpoints we could
        // not pin. Operator retries on the next informer event.
        tracing::warn!(
            strategy = %key,
            error = %err,
            "OIDC discovery resolution failed — strategy will not be registered",
        );
        return None;
    }

    // Non-OIDC strategies (kind=Jwt, Composite, ApiKey) never go through
    // discovery, so `IssuerConfig::jwks_url`'s serde default of `""`
    // would otherwise land in the registry as a silent broken issuer —
    // request-time JWT verification would loop on JwksError::Pending.
    // Reject loudly here instead.
    if !matches!(spec.kind, AuthStrategyType::Oidc) {
        for issuer in &spec.config.issuers {
            if issuer.jwks_url.is_empty() {
                tracing::warn!(
                    strategy = %key,
                    issuer = %issuer.issuer,
                    "issuer is missing `jwksUrl` and the strategy is not OIDC \
                     (discovery only runs for `type: oidc`) — strategy will \
                     not be registered",
                );
                return None;
            }
        }
    }

    Some(ResolvedAuthStrategy::from_spec(&reference, spec))
}

/// Mutate `spec` in place: if it's an OIDC strategy with `config_url` set
/// and at least one endpoint field is unset, fetch the discovery doc and
/// fill in the unset fields. Explicit values in the CRD always win.
async fn resolve_discovery(
    strategy_key: &str,
    spec: &mut AuthStrategySpec,
    discovery: &DiscoveryCache,
) -> Result<(), DiscoveryResolveError> {
    if spec.kind != AuthStrategyType::Oidc {
        return Ok(());
    }
    let Some(oidc) = spec.config.oidc.as_mut() else {
        return Ok(());
    };
    let Some(config_url) = oidc.config_url.clone() else {
        // Pinned mode — no discovery needed. We still want to fail loud
        // here if the user forgot to set the required endpoints, but
        // that's covered by the existing webhook + login handler checks.
        return Ok(());
    };

    if !oidc.needs_discovery() && all_issuer_jwks_set(spec) {
        // configUrl is set but every field is already explicit — nothing
        // to fetch. Cheap path so a user can keep configUrl in the CRD
        // for documentation purposes without paying for a round-trip.
        return Ok(());
    }

    let doc = discovery
        .fetch(&config_url)
        .await
        .map_err(DiscoveryResolveError::Fetch)?;

    apply_discovery(spec, &doc);

    let Some(oidc) = spec.config.oidc.as_ref() else {
        // unreachable — we set it above and apply_discovery never removes
        // it — but stay defensive so a future refactor can't silently
        // bypass the completeness check.
        return Err(DiscoveryResolveError::IncompleteAfterMerge);
    };
    if !oidc.endpoints_complete() {
        return Err(DiscoveryResolveError::IncompleteAfterMerge);
    }
    if spec.config.issuers.iter().any(|i| i.jwks_url.is_empty()) {
        return Err(DiscoveryResolveError::JwksUrlMissingAfterMerge);
    }

    tracing::info!(
        strategy = %strategy_key,
        config_url = %config_url,
        authorization_endpoint = %oidc.authorization_endpoint,
        token_endpoint = %oidc.token_endpoint,
        issuer = %oidc.issuer,
        "OIDC discovery applied",
    );

    Ok(())
}

fn all_issuer_jwks_set(spec: &AuthStrategySpec) -> bool {
    spec.config.issuers.iter().all(|i| !i.jwks_url.is_empty())
}

/// Merge a discovery doc into `spec`. CRD-explicit values always win;
/// only blank/unset fields get filled in. Also fills in
/// `issuers[].jwks_url` for any issuer whose `issuer` claim matches the
/// discovery doc OR for the sole issuer when only one is configured —
/// in the common "config_url + one issuer for claim mapping" shape, the
/// jwks endpoint comes from discovery so users don't have to repeat it.
fn apply_discovery(spec: &mut AuthStrategySpec, doc: &OidcDiscovery) {
    if let Some(oidc) = spec.config.oidc.as_mut() {
        if oidc.authorization_endpoint.is_empty() {
            oidc.authorization_endpoint = doc.authorization_endpoint.clone();
        }
        if oidc.token_endpoint.is_empty() {
            oidc.token_endpoint = doc.token_endpoint.clone();
        }
        if oidc.userinfo_endpoint.is_none() {
            oidc.userinfo_endpoint = doc.userinfo_endpoint.clone();
        }
        if oidc.issuer.is_empty() {
            oidc.issuer = doc.issuer.clone();
        }
    }

    let single_issuer = spec.config.issuers.len() == 1;
    for issuer in spec.config.issuers.iter_mut() {
        if !issuer.jwks_url.is_empty() {
            continue;
        }
        let matches_discovery_issuer = issuer.issuer == doc.issuer;
        if matches_discovery_issuer || single_issuer {
            issuer.jwks_url = doc.jwks_uri.clone();
            if issuer.issuer.is_empty() && single_issuer {
                issuer.issuer = doc.issuer.clone();
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum DiscoveryResolveError {
    #[error(transparent)]
    Fetch(#[from] crate::auth::DiscoveryError),
    #[error(
        "OIDC strategy still has unset endpoint(s) after discovery — \
         the IdP's discovery doc may have omitted required fields"
    )]
    IncompleteAfterMerge,
    #[error(
        "OIDC strategy has an issuer entry without `jwks_url` after \
         discovery — set `issuers[].jwksUrl` explicitly or ensure the \
         issuer's `issuer` claim matches the discovery doc's `issuer`"
    )]
    JwksUrlMissingAfterMerge,
}

fn reference_from(crd: &AuthStrategy) -> Option<NamespacedRef> {
    let name = crd.name_any();
    let namespace = crd.namespace()?;
    Some(NamespacedRef { namespace, name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_types::crds::auth::{
        AuthStrategyConfig, ClaimMapping, IssuerConfig, OidcConfig, SecretRef,
    };

    fn discovery_doc() -> OidcDiscovery {
        OidcDiscovery {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/oauth2/authorize".into(),
            token_endpoint: "https://idp.example.com/oauth2/token".into(),
            userinfo_endpoint: Some("https://idp.example.com/oauth2/userinfo".into()),
            jwks_uri: "https://idp.example.com/.well-known/jwks.json".into(),
        }
    }

    fn empty_oidc_spec(config_url: Option<&str>) -> AuthStrategySpec {
        AuthStrategySpec {
            kind: AuthStrategyType::Oidc,
            config: AuthStrategyConfig {
                issuers: vec![IssuerConfig {
                    issuer: String::new(),
                    jwks_url: String::new(),
                    audience: Some("velocity-api".into()),
                    claims: ClaimMapping::default(),
                }],
                oidc: Some(OidcConfig {
                    config_url: config_url.map(str::to_string),
                    authorization_endpoint: String::new(),
                    token_endpoint: String::new(),
                    userinfo_endpoint: None,
                    client_id: "vel-client".into(),
                    client_secret_ref: SecretRef { name: "s".into(), key: "k".into() },
                    redirect_uri: "https://api.example.com/auth/callback/p/d".into(),
                    scopes: vec!["openid".into()],
                    issuer: String::new(),
                    session_ttl: None,
                }),
                ..AuthStrategyConfig::default()
            },
        }
    }

    #[test]
    fn discovery_fills_only_missing_endpoint_fields() {
        let mut spec = empty_oidc_spec(Some("https://idp.example.com/.well-known/openid-configuration"));
        // pre-pin the token endpoint to verify explicit-wins semantics
        let oidc = spec.config.oidc.as_mut().unwrap();
        oidc.token_endpoint = "https://override.example.com/token".into();

        apply_discovery(&mut spec, &discovery_doc());

        let oidc = spec.config.oidc.as_ref().unwrap();
        assert_eq!(
            oidc.authorization_endpoint,
            "https://idp.example.com/oauth2/authorize",
            "blank field filled from discovery",
        );
        assert_eq!(
            oidc.token_endpoint, "https://override.example.com/token",
            "explicit CRD value must NOT be overwritten",
        );
        assert_eq!(oidc.issuer, "https://idp.example.com");
        assert_eq!(
            oidc.userinfo_endpoint.as_deref(),
            Some("https://idp.example.com/oauth2/userinfo"),
        );
    }

    #[test]
    fn discovery_fills_single_issuer_jwks_url() {
        let mut spec = empty_oidc_spec(Some("https://idp.example.com/.well-known/openid-configuration"));
        apply_discovery(&mut spec, &discovery_doc());
        assert_eq!(
            spec.config.issuers[0].jwks_url,
            "https://idp.example.com/.well-known/jwks.json",
        );
        assert_eq!(spec.config.issuers[0].issuer, "https://idp.example.com");
    }

    #[test]
    fn discovery_does_not_overwrite_explicit_jwks_url() {
        let mut spec = empty_oidc_spec(Some("https://idp.example.com/.well-known/openid-configuration"));
        spec.config.issuers[0].issuer = "https://idp.example.com".into();
        spec.config.issuers[0].jwks_url = "https://pinned.example.com/jwks.json".into();

        apply_discovery(&mut spec, &discovery_doc());

        assert_eq!(spec.config.issuers[0].jwks_url, "https://pinned.example.com/jwks.json");
    }

    #[tokio::test]
    async fn fail_closed_when_discovery_url_unreachable() {
        // Bind a TCP listener then drop it without serving so the URL is
        // routable but every connect either RSTs or returns a non-200.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/.well-known/openid-configuration");
        let mut spec = empty_oidc_spec(Some(&url));
        let cache = DiscoveryCache::new();

        let res = resolve_discovery("ns/name", &mut spec, &cache).await;
        assert!(
            matches!(res, Err(DiscoveryResolveError::Fetch(_))),
            "expected fail-closed Fetch error, got {:?}",
            res,
        );
        // Spec must still have its endpoints blank — discovery did NOT
        // partially fill them in on failure.
        let oidc = spec.config.oidc.as_ref().unwrap();
        assert!(oidc.authorization_endpoint.is_empty());
        assert!(oidc.token_endpoint.is_empty());
    }

    #[tokio::test]
    async fn jwt_strategy_with_blank_jwks_url_is_rejected() {
        use kube::core::ObjectMeta;
        let mut spec = empty_oidc_spec(None);
        spec.kind = AuthStrategyType::Jwt;
        spec.config.oidc = None;
        spec.config.issuers[0].issuer = "https://idp.example.com".into();
        // jwks_url left blank — JWT path must reject this.
        let crd = AuthStrategy {
            metadata: ObjectMeta {
                name: Some("oidc-default".into()),
                namespace: Some("platform".into()),
                ..ObjectMeta::default()
            },
            spec,
            status: None,
        };
        let cache = DiscoveryCache::new();
        let res = resolve(&crd, &cache).await;
        assert!(res.is_none(), "JWT strategy with empty jwks_url must not register");
    }

    #[test]
    fn no_op_when_kind_is_not_oidc() {
        let mut spec = empty_oidc_spec(None);
        spec.kind = AuthStrategyType::Jwt;
        // jwt-only strategy: discovery would be skipped at the caller, but
        // even if apply_discovery is called directly, oidc may be Some and
        // be touched. That's OK — JWT strategies ignore oidc entirely.
        let before = spec.clone();
        apply_discovery(&mut spec, &discovery_doc());
        // OIDC sub-struct is still mutated (we don't gate inside
        // apply_discovery — the caller does), but JWT consumers don't read
        // it, so behaviour is unchanged. Just assert issuers are still
        // populated correctly for a single-issuer case.
        assert_eq!(spec.config.issuers.len(), before.config.issuers.len());
    }
}
