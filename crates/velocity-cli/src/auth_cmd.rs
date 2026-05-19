//! `velocity grant` / `velocity revoke` / `velocity api-key ...`.
//!
//! All three are thin CRD wrappers. We don't reach into the data
//! plane — the operator owns the actual RBAC effects via its
//! RoleBinding / ApiKey controllers. The CLI just applies / deletes
//! the CRD with the right shape and a sensible name when the operator
//! didn't supply one.
//!
//! For `api-key create`, the controller generates a plaintext key,
//! stores `sha256(plaintext)` in `status.key_hash`, and writes the
//! plaintext into a Secret referenced by `status.secret_ref`. The CLI
//! polls until the phase flips to `Ready` and then prints the secret
//! name so the operator can `kubectl get secret ... -o jsonpath` to
//! retrieve the plaintext once. Plaintext never goes through the CLI.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use clap::{Args, Subcommand};
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::{Api, Discovery};
use serde_json::{json, Value};
use velocity_types::crds::auth::{ApiKey, ApiKeySpec, RoleBinding, RoleBindingSpec, ScopeSpec};

use crate::kube_helpers::{build_client, find_resource};

const FIELD_MANAGER: &str = "velocity-cli";

// ---------------------------------------------------------------------
// grant
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct GrantArgs {
    /// Namespace to create the RoleBinding in (must match the
    /// `<org>-<app>-<domain>` of the schemas the roles cover).
    pub namespace: String,
    /// Subject identifier (JWT `sub`, OIDC subject, API-key actor).
    #[arg(long)]
    pub actor: String,
    /// One or more role names. Repeat the flag for multiple roles.
    #[arg(long, required = true)]
    pub role: Vec<String>,
    /// Optional schema:operation scope. Format: `<schema>:<op>[,<op>]`,
    /// e.g. `purchase-order:read,update`. Repeatable.
    #[arg(long)]
    pub scope: Vec<String>,
    /// RFC 3339 expiry. Without this the binding has no automatic
    /// expiry — the operator must revoke it explicitly.
    #[arg(long)]
    pub expires: Option<String>,
    /// Binding name. Defaults to `<actor>-<random6>` — explicit names
    /// are recommended for human review (`velocity get rb`).
    #[arg(long)]
    pub name: Option<String>,
}

pub(crate) async fn grant(args: GrantArgs, kubeconfig: &Option<String>) -> Result<()> {
    let scopes = args.scope.iter().map(|s| parse_scope(s)).collect::<Result<Vec<_>>>()?;

    let name = args.name.clone().unwrap_or_else(|| default_binding_name(&args.actor));

    let mut rb = RoleBinding::new(
        &name,
        RoleBindingSpec {
            actor_id: args.actor.clone(),
            roles: args.role.clone(),
            scopes,
            expires_at: args.expires.clone(),
            granted_by: std::env::var("USER").ok(),
        },
    );
    rb.metadata.namespace = Some(args.namespace.clone());

    let client = build_client(kubeconfig.as_deref()).await?;
    let api: Api<RoleBinding> = Api::namespaced(client, &args.namespace);
    api.patch(&name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(&rb))
        .await
        .with_context(|| format!("applying RoleBinding {name}"))?;

    eprintln!("granted: RoleBinding {}/{name}", args.namespace);
    Ok(())
}

fn default_binding_name(actor: &str) -> String {
    // Use the first 6 hex chars of a random uuid v4 to disambiguate when
    // an operator grants the same actor twice with different scopes.
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{}-{}", sanitize_actor(actor), &suffix[..6.min(suffix.len())])
}

fn sanitize_actor(actor: &str) -> String {
    // CRD names must be RFC 1123 labels. Lowercase, replace anything
    // not [a-z0-9-] with `-`, trim leading/trailing `-`.
    let mut out = String::with_capacity(actor.len());
    for c in actor.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn parse_scope(s: &str) -> Result<ScopeSpec> {
    let (schema, ops) =
        s.split_once(':').ok_or_else(|| anyhow!("scope `{s}` must be `<schema>:<op>[,<op>]`"))?;
    if schema.is_empty() {
        bail!("scope `{s}` has empty schema");
    }
    let operations: Vec<String> =
        if ops.is_empty() { Vec::new() } else { ops.split(',').map(str::to_string).collect() };
    Ok(ScopeSpec {
        schema: schema.to_string(),
        version: None,
        operations,
        attributes: Default::default(),
    })
}

// ---------------------------------------------------------------------
// revoke
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct RevokeArgs {
    pub namespace: String,
    pub name: String,
    #[arg(long)]
    pub yes: bool,
}

pub(crate) async fn revoke(args: RevokeArgs, kubeconfig: &Option<String>) -> Result<()> {
    if !args.yes
        && !crate::confirm::confirm(&format!(
            "revoke RoleBinding {}/{}? this DELETEs the CRD.",
            args.namespace, args.name
        ))?
    {
        bail!("aborted");
    }
    let client = build_client(kubeconfig.as_deref()).await?;
    let api: Api<RoleBinding> = Api::namespaced(client, &args.namespace);
    api.delete(&args.name, &DeleteParams::default())
        .await
        .with_context(|| format!("deleting RoleBinding {}/{}", args.namespace, args.name))?;
    eprintln!("revoked: RoleBinding {}/{}", args.namespace, args.name);
    Ok(())
}

// ---------------------------------------------------------------------
// api-key
// ---------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub(crate) enum ApiKeyCmd {
    /// Create an ApiKey CRD; wait for the operator to mint the plaintext
    /// into a Secret; print the Secret name.
    Create {
        namespace: String,
        name: String,
        #[arg(long)]
        actor: String,
        /// `human` or `service`. Mostly informational; the audit chain
        /// uses it to render actor_type.
        #[arg(long, default_value = "service")]
        actor_type: String,
        /// Repeatable. Format: `<schema>:<op>[,<op>]`.
        #[arg(long)]
        scope: Vec<String>,
        /// RFC 3339 expiry.
        #[arg(long)]
        expires: Option<String>,
        /// Max seconds to wait for the operator to mint the secret.
        #[arg(long, default_value_t = 30)]
        wait_secs: u64,
    },
    /// DELETE an ApiKey CRD (the operator removes the backing Secret).
    Revoke {
        namespace: String,
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// List ApiKeys in a namespace (or all namespaces if --namespace not given).
    List {
        #[arg(short, long)]
        namespace: Option<String>,
    },
}

pub(crate) async fn api_key(cmd: ApiKeyCmd, kubeconfig: &Option<String>) -> Result<()> {
    let client = build_client(kubeconfig.as_deref()).await?;
    match cmd {
        ApiKeyCmd::Create { namespace, name, actor, actor_type, scope, expires, wait_secs } => {
            let scopes = scope.iter().map(|s| parse_scope(s)).collect::<Result<Vec<_>>>()?;
            let mut key = ApiKey::new(
                &name,
                ApiKeySpec { actor, actor_type, scopes, ip_allowlist: Vec::new(), expiry: expires },
            );
            key.metadata.namespace = Some(namespace.clone());

            let api: Api<ApiKey> = Api::namespaced(client.clone(), &namespace);
            api.patch(&name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(&key))
                .await
                .with_context(|| format!("applying ApiKey {namespace}/{name}"))?;
            eprintln!("ApiKey {namespace}/{name} applied; waiting up to {wait_secs}s for the operator to mint the secret...");

            let secret_ref =
                wait_for_secret_ref(&api, &name, Duration::from_secs(wait_secs)).await?;
            println!(
                "{}",
                json!({
                    "namespace":  namespace,
                    "name":       name,
                    "secret_ref": secret_ref,
                    "fetch_with": format!(
                        "kubectl get secret -n {namespace} {secret_ref} -o jsonpath='{{.data.key}}' | base64 -d"
                    ),
                })
            );
            Ok(())
        }
        ApiKeyCmd::Revoke { namespace, name, yes } => {
            if !yes
                && !crate::confirm::confirm(&format!(
                    "revoke ApiKey {namespace}/{name}? this DELETEs the CRD and its Secret."
                ))?
            {
                bail!("aborted");
            }
            let api: Api<ApiKey> = Api::namespaced(client, &namespace);
            api.delete(&name, &DeleteParams::default())
                .await
                .with_context(|| format!("deleting ApiKey {namespace}/{name}"))?;
            eprintln!("ApiKey {namespace}/{name} revoked");
            Ok(())
        }
        ApiKeyCmd::List { namespace } => {
            // Use Discovery so the list works even if `velocity-types`
            // ApiKey GVK diverges from what's deployed (the data is
            // the same shape regardless).
            let discovery =
                Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;
            let (ar, caps) = find_resource(&discovery, "ApiKey")?;
            let api: Api<kube::api::DynamicObject> = if crate::kube_helpers::is_namespaced(&caps) {
                match namespace {
                    Some(ns) => Api::namespaced_with(client, &ns, &ar),
                    None => Api::all_with(client, &ar),
                }
            } else {
                Api::all_with(client, &ar)
            };
            let list =
                api.list(&kube::api::ListParams::default()).await.context("listing ApiKeys")?;
            for o in list.items {
                let ns = o.metadata.namespace.unwrap_or_else(|| "<none>".into());
                let nm = o.metadata.name.unwrap_or_else(|| "<unnamed>".into());
                let phase = o
                    .data
                    .get("status")
                    .and_then(|s| s.get("phase"))
                    .and_then(Value::as_str)
                    .unwrap_or("—");
                let revoked = o
                    .data
                    .get("status")
                    .and_then(|s| s.get("revokedAt"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                println!(
                    "{ns}\t{nm}\t{phase}\t{}",
                    if revoked.is_empty() { "active" } else { "revoked" }
                );
            }
            Ok(())
        }
    }
}

/// Poll the ApiKey's status until `status.secretRef` shows up, or
/// `timeout` elapses. The controller writes the Secret + flips the
/// status in a single reconcile pass once the spec validates.
async fn wait_for_secret_ref(api: &Api<ApiKey>, name: &str, timeout: Duration) -> Result<String> {
    let started = Instant::now();
    loop {
        let obj = api.get(name).await.with_context(|| format!("polling ApiKey {name}"))?;
        if let Some(s) = obj.status.as_ref().and_then(|s| s.secret_ref.clone()) {
            return Ok(s);
        }
        if started.elapsed() > timeout {
            bail!(
                "timed out after {}s waiting for ApiKey {name} status.secretRef; \
                 inspect with `kubectl describe apikey -n <ns> {name}` or rerun with --wait-secs",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_scope_accepts_multi_op() {
        let s = parse_scope("purchase-order:read,update").unwrap();
        assert_eq!(s.schema, "purchase-order");
        assert_eq!(s.operations, vec!["read", "update"]);
    }

    #[test]
    fn parse_scope_accepts_empty_ops_meaning_all() {
        // Empty ops list = no operation restriction. Useful for
        // schema-level admin roles.
        let s = parse_scope("purchase-order:").unwrap();
        assert_eq!(s.schema, "purchase-order");
        assert!(s.operations.is_empty());
    }

    #[test]
    fn parse_scope_rejects_missing_colon() {
        assert!(parse_scope("purchase-order").is_err());
    }

    #[test]
    fn parse_scope_rejects_empty_schema() {
        assert!(parse_scope(":read").is_err());
    }

    #[test]
    fn sanitize_actor_lowercases_and_replaces_specials() {
        assert_eq!(sanitize_actor("Ravi.Kumar@Acme"), "ravi-kumar-acme");
    }

    #[test]
    fn sanitize_actor_trims_dashes() {
        assert_eq!(sanitize_actor("---a---"), "a");
    }

    #[test]
    fn default_binding_name_has_random_suffix() {
        let a = default_binding_name("svc-orders");
        let b = default_binding_name("svc-orders");
        assert_ne!(a, b);
        assert!(a.starts_with("svc-orders-"));
        assert!(a.len() <= "svc-orders-".len() + 6);
    }
}
