//! Phase 12a (ADR-011): per-domain **data-API** workload orchestration.
//!
//! When a `Domain` has `spec.deployment.scope == domain`, the operator
//! materialises a dedicated `velocity-api` Deployment (run with
//! `VELOCITY_API_MODE=data`) plus a Service, an HPA, and an Ingress path so
//! that domain's REST traffic is served by its own pods in-process — isolating
//! its blast radius, scaling, and rollout from every other domain.
//!
//! Everything created here carries an `ownerReference` to the `Domain`, so
//! deleting the Domain (or flipping it back to `shared`) garbage-collects the
//! workload via Kubernetes' normal GC — `cleanup` only needs to handle the
//! flip-to-shared case explicitly.
//!
//! Routing note: each domain gets its own `Ingress` object sharing the
//! platform host. Ingress controllers (nginx, traefik, …) compose multiple
//! Ingress resources for the same host into one routing table, so this is the
//! "shared ingress, generated paths" outcome (ADR-011) implemented in a
//! race-free, idempotent, per-Domain-owned way — no read-modify-write of a
//! single shared object across concurrent reconciles.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::autoscaling::v2::{
    CrossVersionObjectReference, HorizontalPodAutoscaler, HorizontalPodAutoscalerSpec, MetricSpec,
    MetricTarget, ResourceMetricSource,
};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvFromSource, EnvVar, HTTPGetAction, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements, Secret, SecretEnvSource, Service, ServiceAccount, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::api::rbac::v1::{ClusterRoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::{Client, ResourceExt};
use velocity_types::crds::hierarchy::{DeploymentConfig, ResourceQuantities};
use velocity_types::crds::{Application, Domain};

use crate::context::DataApiSettings;
use crate::provisioner::PostgresProvisioner;

/// Server-side-apply field manager (matches the `slo_rules` convention).
const MANAGER: &str = "velocity-operator";
/// One data-API workload per domain namespace; the namespace is already
/// `{org}-{app}-{domain}`, so a fixed name is unique within it.
const NAME: &str = "velocity-data-api";
/// Service account created by the operator in every data-API namespace.
/// A chart-rendered ClusterRole `velocity-data-api` + a per-namespace
/// ClusterRoleBinding grant it cluster-wide SchemaDefinition/AuthStrategy watch.
const SA_NAME: &str = "velocity-data-api";
/// Chart-rendered ClusterRole granting the data-API SA cluster-wide read on
/// velocity.sh CRDs. The operator binds to it per namespace via a CRB.
const CLUSTER_ROLE: &str = "velocity-data-api";
const HTTP_PORT: i32 = 8080;
const HEALTH_PORT: i32 = 8081;
const DEFAULT_MIN_REPLICAS: i32 = 1;
const DEFAULT_MAX_REPLICAS: i32 = 10;

/// Outcome of a workload sync — surfaced into `DomainStatus.dataApiDeployment`.
#[derive(Debug)]
pub struct SyncedWorkload {
    pub deployment_name: String,
}

/// Ensure a `velocity-data-api` ServiceAccount and its ClusterRoleBinding exist
/// in `namespace`. The CRB binds the chart-rendered `velocity-data-api` ClusterRole
/// (which grants cluster-wide SD/AuthStrategy watch) to the SA so the pod's
/// informers can discover schemas across all namespaces.
///
/// The CRB is cluster-scoped but named `velocity-data-api-{namespace}` so that
/// each provisioned namespace gets its own entry. `cleanup()` deletes it.
async fn sync_rbac(kube: &Client, namespace: &str) -> anyhow::Result<()> {
    let pp = PatchParams::apply(MANAGER).force();

    // ServiceAccount in the target namespace.
    let sa = ServiceAccount {
        metadata: ObjectMeta {
            name: Some(SA_NAME.into()),
            namespace: Some(namespace.into()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".into(), MANAGER.into()),
                ("app.kubernetes.io/component".into(), "data-api".into()),
            ])),
            ..Default::default()
        },
        ..Default::default()
    };
    Api::<ServiceAccount>::namespaced(kube.clone(), namespace)
        .patch(SA_NAME, &pp, &Patch::Apply(&sa))
        .await?;

    // ClusterRoleBinding: binds the chart ClusterRole to this namespace's SA.
    let crb_name = format!("{CLUSTER_ROLE}-{namespace}");
    let crb = ClusterRoleBinding {
        metadata: ObjectMeta {
            name: Some(crb_name.clone()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".into(), MANAGER.into()),
                ("app.kubernetes.io/component".into(), "data-api".into()),
            ])),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".into(),
            kind: "ClusterRole".into(),
            name: CLUSTER_ROLE.into(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".into(),
            name: SA_NAME.into(),
            namespace: Some(namespace.into()),
            ..Default::default()
        }]),
    };
    Api::<ClusterRoleBinding>::all(kube.clone())
        .patch(&crb_name, &pp, &Patch::Apply(&crb))
        .await?;

    Ok(())
}

/// Create or update the data-API Deployment + Service + HPA + Ingress for a
/// dedicated domain. Idempotent (server-side apply).
#[allow(clippy::too_many_arguments)] // org/app/domain/ns are distinct identity inputs; a struct would just move the noise
pub async fn sync(
    kube: &Client,
    provisioner: &PostgresProvisioner,
    settings: &DataApiSettings,
    domain: &Domain,
    namespace: &str,
    org: &str,
    app: &str,
    domain_name: &str,
    pg_schema: &str,
    cfg: &DeploymentConfig,
    ingress_host: Option<&str>,
) -> anyhow::Result<SyncedWorkload> {
    let owner = owner_ref(domain);
    let labels = labels(org, app, domain_name);
    let pp = PatchParams::apply(MANAGER).force();

    sync_rbac(kube, namespace).await?;

    // Project the env Secret (with the operator-minted per-domain DB
    // credential) into this namespace *before* the Deployment, so its
    // `envFrom` resolves on first rollout.
    let has_env = project_env_secret(kube, provisioner, settings, namespace, pg_schema, &owner, &labels)
        .await?;

    let dep = build_deployment(settings, namespace, &owner, &labels, cfg, has_env);
    Api::<Deployment>::namespaced(kube.clone(), namespace)
        .patch(NAME, &pp, &Patch::Apply(&dep))
        .await?;

    let svc = build_service(namespace, &owner, &labels);
    Api::<Service>::namespaced(kube.clone(), namespace)
        .patch(NAME, &pp, &Patch::Apply(&svc))
        .await?;

    let hpa = build_hpa(namespace, &owner, &labels, cfg);
    Api::<HorizontalPodAutoscaler>::namespaced(kube.clone(), namespace)
        .patch(NAME, &pp, &Patch::Apply(&hpa))
        .await?;

    if let Some(host) = ingress_host {
        let ing = build_ingress(namespace, &owner, &labels, host, org, app, domain_name);
        Api::<Ingress>::namespaced(kube.clone(), namespace)
            .patch(NAME, &pp, &Patch::Apply(&ing))
            .await?;
    }

    tracing::info!(%namespace, deployment = NAME, "data-API workload synced");
    Ok(SyncedWorkload { deployment_name: NAME.to_string() })
}

/// Remove a previously-created data-API workload when a domain flips from
/// `dedicated` back to `shared`. Best-effort: GC handles the delete-Domain
/// case via owner references; this handles the in-place flip. Missing objects
/// are not an error.
pub async fn cleanup(kube: &Client, namespace: &str) -> anyhow::Result<()> {
    use kube::api::DeleteParams;
    let dp = DeleteParams::default();
    let _ = Api::<Ingress>::namespaced(kube.clone(), namespace).delete(NAME, &dp).await;
    let _ = Api::<HorizontalPodAutoscaler>::namespaced(kube.clone(), namespace)
        .delete(NAME, &dp)
        .await;
    let _ = Api::<Service>::namespaced(kube.clone(), namespace).delete(NAME, &dp).await;
    let _ = Api::<Deployment>::namespaced(kube.clone(), namespace).delete(NAME, &dp).await;
    // ClusterRoleBinding is cluster-scoped and is not GC'd with the namespace.
    let crb_name = format!("{CLUSTER_ROLE}-{namespace}");
    let _ = Api::<ClusterRoleBinding>::all(kube.clone()).delete(&crb_name, &dp).await;
    Ok(())
}

/// Create or update the app-scoped data-API Deployment + Service + HPA + Ingress
/// in `namespace` (`{org}-{app}-shared`). The pod receives
/// `VELOCITY_API_LABEL_SELECTOR` so the kube informer inside it watches only
/// schemas labelled `velocity.sh/org={org},velocity.sh/app={app}` (excluding
/// those stamped `velocity.sh/dedicated=true`, which have their own pod).
///
/// Owner-references are intentionally absent — the Application lives in
/// `{org}-platform`, but k8s GC does not cascade owner refs across namespaces.
/// Finalizer-based cleanup is deferred to Stage 4; this function implements
/// the materialise path only.
#[allow(clippy::too_many_arguments)]
pub async fn sync_app(
    kube: &Client,
    provisioner: &PostgresProvisioner,
    settings: &DataApiSettings,
    application: &Application,
    namespace: &str,
    org: &str,
    app_name: &str,
    cfg: &DeploymentConfig,
    ingress_host: Option<&str>,
) -> anyhow::Result<SyncedWorkload> {
    let labels = labels_app(org, app_name);
    let pp = PatchParams::apply(MANAGER).force();

    sync_rbac(kube, namespace).await?;

    let label_selector =
        format!("velocity.sh/org={org},velocity.sh/app={app_name},velocity.sh/dedicated!=true");

    let has_env =
        project_env_secret_app(kube, provisioner, settings, namespace, org, app_name, &labels)
            .await?;

    let dep = build_deployment_app(settings, namespace, &labels, cfg, has_env, &label_selector);
    Api::<Deployment>::namespaced(kube.clone(), namespace)
        .patch(NAME, &pp, &Patch::Apply(&dep))
        .await?;

    let svc = build_service_app(namespace, &labels);
    Api::<Service>::namespaced(kube.clone(), namespace)
        .patch(NAME, &pp, &Patch::Apply(&svc))
        .await?;

    let hpa = build_hpa_app(namespace, &labels, cfg);
    Api::<HorizontalPodAutoscaler>::namespaced(kube.clone(), namespace)
        .patch(NAME, &pp, &Patch::Apply(&hpa))
        .await?;

    if let Some(host) = ingress_host {
        let ing = build_ingress_app(namespace, &labels, host, org, app_name);
        Api::<Ingress>::namespaced(kube.clone(), namespace)
            .patch(NAME, &pp, &Patch::Apply(&ing))
            .await?;
    }

    tracing::info!(%namespace, deployment = NAME, app = %application.name_any(), "app-scoped data-API workload synced");
    Ok(SyncedWorkload { deployment_name: NAME.to_string() })
}

/// Path under the shared host that routes to this app's data-API.
/// `/api/{org}/{app}` is a prefix of every per-schema route in the app
/// (`/api/{org}/{app}/{domain}/{object}/{version}/...`). Domain-scoped pods
/// use the longer `/api/{org}/{app}/{domain}` prefix which wins in nginx
/// longest-prefix routing — so domain-scoped pods serve their specific domain
/// while this pod catches everything else.
pub fn ingress_path_app(org: &str, app: &str) -> String {
    format!("/api/{org}/{app}")
}

fn labels_app(org: &str, app: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "velocity".into()),
        ("app.kubernetes.io/component".into(), "data-api".into()),
        ("app.kubernetes.io/managed-by".into(), MANAGER.into()),
        ("velocity.sh/org".into(), org.into()),
        ("velocity.sh/app".into(), app.into()),
    ])
}

/// ObjectMeta without an owner-reference. Used for app-scoped resources where
/// the owner (Application) is in a different namespace — k8s GC does not
/// cascade across namespaces for namespaced resources (Stage 4 adds finalizers).
fn meta_without_owner(namespace: &str, labels: &BTreeMap<String, String>) -> ObjectMeta {
    ObjectMeta {
        name: Some(NAME.into()),
        namespace: Some(namespace.into()),
        labels: Some(labels.clone()),
        ..Default::default()
    }
}

/// Stable pod selector for app-scoped pods: component + app label.
/// Domain label is intentionally absent (there is none for app-scope).
fn selector_labels_app(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for k in ["app.kubernetes.io/component", "velocity.sh/app"] {
        if let Some(v) = labels.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    m
}

fn build_deployment_app(
    settings: &DataApiSettings,
    namespace: &str,
    labels: &BTreeMap<String, String>,
    cfg: &DeploymentConfig,
    has_env: bool,
    label_selector: &str,
) -> Deployment {
    let mut env = vec![
        EnvVar { name: "VELOCITY_API_MODE".into(), value: Some("data".into()), value_from: None },
        EnvVar {
            name: "VELOCITY_API_LABEL_SELECTOR".into(),
            value: Some(label_selector.into()),
            value_from: None,
        },
    ];
    if settings.anonymous_auth {
        env.push(EnvVar {
            name: "VELOCITY_API_AUTH_MODE".into(),
            value: Some("anonymous".into()),
            value_from: None,
        });
    }

    let env_from = settings.env_secret.as_ref().filter(|_| has_env).map(|s| {
        vec![EnvFromSource {
            secret_ref: Some(SecretEnvSource { name: s.clone(), optional: Some(false) }),
            ..Default::default()
        }]
    });

    let resources = ResourceRequirements {
        requests: cfg.resources.as_ref().and_then(|r| quantities(&r.requests)),
        limits: cfg.resources.as_ref().and_then(|r| quantities(&r.limits)),
        ..Default::default()
    };

    let container = Container {
        name: "api".into(),
        image: Some(settings.image.clone()),
        ports: Some(vec![
            ContainerPort {
                name: Some("http".into()),
                container_port: HTTP_PORT,
                ..Default::default()
            },
            ContainerPort {
                name: Some("health".into()),
                container_port: HEALTH_PORT,
                ..Default::default()
            },
        ]),
        env: Some(env),
        env_from,
        resources: Some(resources),
        readiness_probe: Some(probe("/readyz", 5, 10)),
        liveness_probe: Some(probe("/healthz", 15, 20)),
        ..Default::default()
    };

    let min = cfg.min_replicas.map(|n| n as i32).unwrap_or(DEFAULT_MIN_REPLICAS).max(1);

    Deployment {
        metadata: meta_without_owner(namespace, labels),
        spec: Some(DeploymentSpec {
            replicas: Some(min),
            selector: LabelSelector {
                match_labels: Some(selector_labels_app(labels)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta { labels: Some(labels.clone()), ..Default::default() }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    service_account_name: Some(SA_NAME.into()),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_service_app(namespace: &str, labels: &BTreeMap<String, String>) -> Service {
    Service {
        metadata: meta_without_owner(namespace, labels),
        spec: Some(ServiceSpec {
            selector: Some(selector_labels_app(labels)),
            ports: Some(vec![ServicePort {
                name: Some("http".into()),
                port: HTTP_PORT,
                target_port: Some(IntOrString::String("http".into())),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_hpa_app(
    namespace: &str,
    labels: &BTreeMap<String, String>,
    cfg: &DeploymentConfig,
) -> HorizontalPodAutoscaler {
    let min = cfg.min_replicas.map(|n| n as i32).unwrap_or(DEFAULT_MIN_REPLICAS).max(1);
    let max = cfg.max_replicas.map(|n| n as i32).unwrap_or(DEFAULT_MAX_REPLICAS).max(min);
    HorizontalPodAutoscaler {
        metadata: meta_without_owner(namespace, labels),
        spec: Some(HorizontalPodAutoscalerSpec {
            scale_target_ref: CrossVersionObjectReference {
                api_version: Some("apps/v1".into()),
                kind: "Deployment".into(),
                name: NAME.into(),
            },
            min_replicas: Some(min),
            max_replicas: max,
            metrics: Some(vec![MetricSpec {
                type_: "Resource".into(),
                resource: Some(ResourceMetricSource {
                    name: "cpu".into(),
                    target: MetricTarget {
                        type_: "Utilization".into(),
                        average_utilization: Some(80),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_ingress_app(
    namespace: &str,
    labels: &BTreeMap<String, String>,
    host: &str,
    org: &str,
    app: &str,
) -> Ingress {
    Ingress {
        metadata: meta_without_owner(namespace, labels),
        spec: Some(IngressSpec {
            rules: Some(vec![IngressRule {
                host: Some(host.into()),
                http: Some(HTTPIngressRuleValue {
                    paths: vec![HTTPIngressPath {
                        path: Some(ingress_path_app(org, app)),
                        path_type: "Prefix".into(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: NAME.into(),
                                port: Some(ServiceBackendPort {
                                    number: Some(HTTP_PORT),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    }],
                }),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Mint the app-scoped DB credential and project the data-API env Secret into
/// `namespace` (`{org}-{app}-shared`). Returns `true` when the Secret was
/// created. Returns `Ok(false)` when projection isn't configured.
async fn project_env_secret_app(
    kube: &Client,
    provisioner: &PostgresProvisioner,
    settings: &DataApiSettings,
    namespace: &str,
    org: &str,
    app_name: &str,
    labels: &BTreeMap<String, String>,
) -> anyhow::Result<bool> {
    let (Some(target), Some(source)) =
        (settings.env_secret.as_ref(), settings.env_source_secret.as_ref())
    else {
        tracing::warn!(
            %namespace,
            "app-scoped data-API env projection not configured (no env_secret/env_source_secret); \
             pod will start without DB env"
        );
        return Ok(false);
    };

    let src = Api::<Secret>::namespaced(kube.clone(), &settings.system_namespace)
        .get(source)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "reading data-API source secret {}/{source}: {e}",
                settings.system_namespace
            )
        })?;
    let mut data = decode_secret(&src);

    let existing_pw = Api::<Secret>::namespaced(kube.clone(), namespace)
        .get_opt(target)
        .await?
        .as_ref()
        .and_then(|s| decode_secret(s).get("VELOCITY_API_PG_PASSWORD").cloned());
    let password = existing_pw.unwrap_or_else(generate_password);

    let role = provisioner
        .ensure_app_data_api_login_role(org, app_name, &password)
        .await
        .map_err(|e| anyhow::anyhow!("provisioning app data-API role for {org}/{app_name}: {e}"))?;

    data.insert("VELOCITY_API_PG_USER".into(), role);
    data.insert("VELOCITY_API_PG_PASSWORD".into(), password);
    data.remove("VELOCITY_API_PG_URL");

    let meta = ObjectMeta {
        name: Some(target.clone()),
        namespace: Some(namespace.into()),
        labels: Some(labels.clone()),
        ..Default::default()
    };
    let secret = Secret {
        metadata: meta,
        string_data: Some(data.into_iter().collect()),
        type_: Some("Opaque".into()),
        ..Default::default()
    };
    Api::<Secret>::namespaced(kube.clone(), namespace)
        .patch(target, &PatchParams::apply(MANAGER).force(), &Patch::Apply(&secret))
        .await?;
    tracing::info!(%namespace, secret = %target, "app-scoped data-API env Secret projected");
    Ok(true)
}

/// Mint the per-domain DB credential and project the data-API env Secret into
/// `namespace`. Returns `true` when the Secret was created (so the Deployment
/// should `envFrom` it). Returns `Ok(false)` when projection isn't configured
/// (no `env_secret`/`env_source_secret`) — the Deployment is still created,
/// just without DB env, and a warning is logged.
async fn project_env_secret(
    kube: &Client,
    provisioner: &PostgresProvisioner,
    settings: &DataApiSettings,
    namespace: &str,
    pg_schema: &str,
    owner: &OwnerReference,
    labels: &BTreeMap<String, String>,
) -> anyhow::Result<bool> {
    let (Some(target), Some(source)) =
        (settings.env_secret.as_ref(), settings.env_source_secret.as_ref())
    else {
        tracing::warn!(
            %namespace,
            "data-API env projection not configured (no env_secret/env_source_secret); \
             pod will start without DB env"
        );
        return Ok(false);
    };

    // 1. Read the chart-provided source env from the operator's namespace.
    let src = Api::<Secret>::namespaced(kube.clone(), &settings.system_namespace)
        .get(source)
        .await
        .map_err(|e| {
            anyhow::anyhow!("reading data-API source secret {}/{source}: {e}", settings.system_namespace)
        })?;
    let mut data = decode_secret(&src);

    // 2. Reuse the existing per-domain password if the target secret already
    //    exists; otherwise mint one. Never rotate underneath a running pod.
    let existing_pw = Api::<Secret>::namespaced(kube.clone(), namespace)
        .get_opt(target)
        .await?
        .as_ref()
        .and_then(|s| decode_secret(s).get("VELOCITY_API_PG_PASSWORD").cloned());
    let password = existing_pw.unwrap_or_else(generate_password);

    // 3. Mint / re-assert the per-domain LOGIN role with that password.
    let role = provisioner
        .ensure_data_api_login_role(pg_schema, &password)
        .await
        .map_err(|e| anyhow::anyhow!("provisioning data-API role for {pg_schema}: {e}"))?;

    // 4. Override the DB identity; drop any stale full URL so the API composes
    //    its connection from the (host/port/db) parts + this user/password.
    data.insert("VELOCITY_API_PG_USER".into(), role);
    data.insert("VELOCITY_API_PG_PASSWORD".into(), password);
    data.remove("VELOCITY_API_PG_URL");

    // 5. Apply the projected Secret (owner-ref'd → GC'd with the Domain).
    let secret = Secret {
        metadata: ObjectMeta { name: Some(target.clone()), ..meta(namespace, owner, labels) },
        string_data: Some(data.into_iter().collect()),
        type_: Some("Opaque".into()),
        ..Default::default()
    };
    Api::<Secret>::namespaced(kube.clone(), namespace)
        .patch(target, &PatchParams::apply(MANAGER).force(), &Patch::Apply(&secret))
        .await?;
    tracing::info!(%namespace, secret = %target, "data-API env Secret projected");
    Ok(true)
}

/// Decode a Secret's `data` (base64 → bytes) into a UTF-8 string map. Binary
/// values that aren't valid UTF-8 are skipped (the data-API env is all text).
fn decode_secret(s: &Secret) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Some(d) = &s.data {
        for (k, v) in d {
            if let Ok(s) = String::from_utf8(v.0.clone()) {
                m.insert(k.clone(), s);
            }
        }
    }
    m
}

/// 64 lowercase-hex chars (~128 bits) — safe to embed as a SQL string literal
/// (see `provisioner::validate_hex_secret`).
fn generate_password() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// Path under the shared host that routes to this domain's data-API.
/// `/api/{org}/{app}/{domain}` — a prefix of every per-schema route in that
/// domain (`/api/{org}/{app}/{domain}/{object}/{version}/...`).
pub fn ingress_path(org: &str, app: &str, domain: &str) -> String {
    format!("/api/{org}/{app}/{domain}")
}

fn owner_ref(domain: &Domain) -> OwnerReference {
    OwnerReference {
        api_version: "velocity.sh/v1".into(),
        kind: "Domain".into(),
        name: domain.name_any(),
        uid: domain.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

fn labels(org: &str, app: &str, domain: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "velocity".into()),
        ("app.kubernetes.io/component".into(), "data-api".into()),
        ("app.kubernetes.io/managed-by".into(), MANAGER.into()),
        ("velocity.sh/org".into(), org.into()),
        ("velocity.sh/app".into(), app.into()),
        ("velocity.sh/domain".into(), domain.into()),
    ])
}

fn meta(
    namespace: &str,
    owner: &OwnerReference,
    labels: &BTreeMap<String, String>,
) -> ObjectMeta {
    ObjectMeta {
        name: Some(NAME.into()),
        namespace: Some(namespace.into()),
        labels: Some(labels.clone()),
        owner_references: Some(vec![owner.clone()]),
        ..Default::default()
    }
}

fn quantities(q: &Option<ResourceQuantities>) -> Option<BTreeMap<String, Quantity>> {
    let q = q.as_ref()?;
    let mut m = BTreeMap::new();
    if let Some(cpu) = &q.cpu {
        m.insert("cpu".into(), Quantity(cpu.clone()));
    }
    if let Some(mem) = &q.memory {
        m.insert("memory".into(), Quantity(mem.clone()));
    }
    (!m.is_empty()).then_some(m)
}

fn build_deployment(
    settings: &DataApiSettings,
    namespace: &str,
    owner: &OwnerReference,
    labels: &BTreeMap<String, String>,
    cfg: &DeploymentConfig,
    has_env: bool,
) -> Deployment {
    let mut env = vec![
        EnvVar { name: "VELOCITY_API_MODE".into(), value: Some("data".into()), value_from: None },
        EnvVar {
            name: "VELOCITY_API_NAMESPACE".into(),
            value: Some(namespace.into()),
            value_from: None,
        },
    ];
    if settings.anonymous_auth {
        env.push(EnvVar {
            name: "VELOCITY_API_AUTH_MODE".into(),
            value: Some("anonymous".into()),
            value_from: None,
        });
    }

    let env_from = settings.env_secret.as_ref().filter(|_| has_env).map(|s| {
        vec![EnvFromSource {
            secret_ref: Some(SecretEnvSource { name: s.clone(), optional: Some(false) }),
            ..Default::default()
        }]
    });

    let resources = ResourceRequirements {
        requests: cfg.resources.as_ref().and_then(|r| quantities(&r.requests)),
        limits: cfg.resources.as_ref().and_then(|r| quantities(&r.limits)),
        ..Default::default()
    };

    let container = Container {
        name: "api".into(),
        image: Some(settings.image.clone()),
        ports: Some(vec![
            ContainerPort { name: Some("http".into()), container_port: HTTP_PORT, ..Default::default() },
            ContainerPort {
                name: Some("health".into()),
                container_port: HEALTH_PORT,
                ..Default::default()
            },
        ]),
        env: Some(env),
        env_from,
        resources: Some(resources),
        readiness_probe: Some(probe("/readyz", 5, 10)),
        liveness_probe: Some(probe("/healthz", 15, 20)),
        ..Default::default()
    };

    let min = cfg.min_replicas.map(|n| n as i32).unwrap_or(DEFAULT_MIN_REPLICAS).max(1);

    Deployment {
        metadata: meta(namespace, owner, labels),
        spec: Some(DeploymentSpec {
            // HPA owns the live replica count after creation; this is the
            // floor the Deployment is created with.
            replicas: Some(min),
            selector: LabelSelector {
                match_labels: Some(selector_labels(labels)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta { labels: Some(labels.clone()), ..Default::default() }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    service_account_name: Some(SA_NAME.into()),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The selector must be stable, so it uses only the identity labels (not the
/// full recommended-label set, which can change between chart versions).
fn selector_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for k in ["app.kubernetes.io/component", "velocity.sh/domain"] {
        if let Some(v) = labels.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    m
}

fn probe(path: &str, initial_delay: i32, period: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::String("health".into()),
            ..Default::default()
        }),
        initial_delay_seconds: Some(initial_delay),
        period_seconds: Some(period),
        ..Default::default()
    }
}

fn build_service(
    namespace: &str,
    owner: &OwnerReference,
    labels: &BTreeMap<String, String>,
) -> Service {
    Service {
        metadata: meta(namespace, owner, labels),
        spec: Some(ServiceSpec {
            selector: Some(selector_labels(labels)),
            ports: Some(vec![ServicePort {
                name: Some("http".into()),
                port: HTTP_PORT,
                target_port: Some(IntOrString::String("http".into())),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_hpa(
    namespace: &str,
    owner: &OwnerReference,
    labels: &BTreeMap<String, String>,
    cfg: &DeploymentConfig,
) -> HorizontalPodAutoscaler {
    let min = cfg.min_replicas.map(|n| n as i32).unwrap_or(DEFAULT_MIN_REPLICAS).max(1);
    let max = cfg.max_replicas.map(|n| n as i32).unwrap_or(DEFAULT_MAX_REPLICAS).max(min);
    HorizontalPodAutoscaler {
        metadata: meta(namespace, owner, labels),
        spec: Some(HorizontalPodAutoscalerSpec {
            scale_target_ref: CrossVersionObjectReference {
                api_version: Some("apps/v1".into()),
                kind: "Deployment".into(),
                name: NAME.into(),
            },
            // min-replicas decision (ADR-011): never scale to zero.
            min_replicas: Some(min),
            max_replicas: max,
            metrics: Some(vec![MetricSpec {
                type_: "Resource".into(),
                resource: Some(ResourceMetricSource {
                    name: "cpu".into(),
                    target: MetricTarget {
                        type_: "Utilization".into(),
                        average_utilization: Some(80),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_ingress(
    namespace: &str,
    owner: &OwnerReference,
    labels: &BTreeMap<String, String>,
    host: &str,
    org: &str,
    app: &str,
    domain: &str,
) -> Ingress {
    Ingress {
        metadata: meta(namespace, owner, labels),
        spec: Some(IngressSpec {
            rules: Some(vec![IngressRule {
                host: Some(host.into()),
                http: Some(HTTPIngressRuleValue {
                    paths: vec![HTTPIngressPath {
                        path: Some(ingress_path(org, app, domain)),
                        path_type: "Prefix".into(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: NAME.into(),
                                port: Some(ServiceBackendPort {
                                    number: Some(HTTP_PORT),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    }],
                }),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_types::crds::hierarchy::DeploymentScope;

    fn cfg() -> DeploymentConfig {
        DeploymentConfig {
            scope: DeploymentScope::Domain,
            min_replicas: Some(2),
            max_replicas: Some(7),
            resources: Some(velocity_types::crds::hierarchy::DeploymentResources {
                requests: Some(ResourceQuantities {
                    cpu: Some("250m".into()),
                    memory: Some("256Mi".into()),
                }),
                limits: None,
            }),
        }
    }

    fn owner() -> OwnerReference {
        OwnerReference { name: "procurement".into(), uid: "uid-1".into(), ..Default::default() }
    }

    #[test]
    fn ingress_path_is_domain_prefix() {
        assert_eq!(ingress_path("acme", "supply-chain", "procurement"), "/api/acme/supply-chain/procurement");
    }

    #[test]
    fn deployment_injects_mode_and_namespace_and_envfrom() {
        let settings = DataApiSettings {
            image: "ghcr.io/kinarix/velocity-api:1.2.3".into(),
            anonymous_auth: true,
            ingress_host: Some("velocity.local".into()),
            env_secret: Some("velocity-data-api-env".into()),
            env_source_secret: Some("velocity-data-api-env-source".into()),
            system_namespace: "velocity-system".into(),
        };
        let labels = labels("acme", "supply-chain", "procurement");
        let dep = build_deployment(&settings, "acme-supply-chain-procurement", &owner(), &labels, &cfg(), true);
        let spec = dep.spec.unwrap();
        assert_eq!(spec.replicas, Some(2));
        let c = &spec.template.spec.unwrap().containers[0];
        assert_eq!(c.image.as_deref(), Some("ghcr.io/kinarix/velocity-api:1.2.3"));
        let env = c.env.as_ref().unwrap();
        let mode = env.iter().find(|e| e.name == "VELOCITY_API_MODE").unwrap();
        assert_eq!(mode.value.as_deref(), Some("data"));
        let ns = env.iter().find(|e| e.name == "VELOCITY_API_NAMESPACE").unwrap();
        assert_eq!(ns.value.as_deref(), Some("acme-supply-chain-procurement"));
        assert!(env.iter().any(|e| e.name == "VELOCITY_API_AUTH_MODE"));
        assert_eq!(
            c.env_from.as_ref().unwrap()[0].secret_ref.as_ref().unwrap().name,
            "velocity-data-api-env"
        );
        // requests carried through; limits omitted
        let req = c.resources.as_ref().unwrap().requests.as_ref().unwrap();
        assert_eq!(req.get("cpu").unwrap().0, "250m");
    }

    #[test]
    fn hpa_respects_min_max_and_never_zero() {
        let labels = labels("acme", "supply-chain", "procurement");
        let hpa = build_hpa("ns", &owner(), &labels, &cfg());
        let spec = hpa.spec.unwrap();
        assert_eq!(spec.min_replicas, Some(2));
        assert_eq!(spec.max_replicas, 7);
        assert_eq!(spec.scale_target_ref.name, NAME);
    }

    #[test]
    fn min_replicas_floor_is_one_even_if_zero_requested() {
        let labels = labels("o", "a", "d");
        let mut c = cfg();
        c.min_replicas = Some(0);
        let hpa = build_hpa("ns", &owner(), &labels, &c);
        assert_eq!(hpa.spec.unwrap().min_replicas, Some(1));
    }

    #[test]
    fn owner_ref_marks_controller_for_gc() {
        let o = owner();
        // build_service carries the owner ref so GC removes it with the Domain.
        let labels = labels("o", "a", "d");
        let svc = build_service("ns", &o, &labels);
        let refs = svc.metadata.owner_references.unwrap();
        assert_eq!(refs[0].name, "procurement");
    }

    // ── app-scoped workload tests ────────────────────────────────────────────

    #[test]
    fn ingress_path_app_is_two_segment() {
        assert_eq!(ingress_path_app("acme", "supply-chain"), "/api/acme/supply-chain");
    }

    #[test]
    fn ingress_path_app_is_shorter_than_domain_path() {
        // nginx longest-prefix routing: domain-scoped path must win for its domain
        let app = ingress_path_app("acme", "supply-chain");
        let domain = ingress_path("acme", "supply-chain", "procurement");
        assert!(domain.starts_with(&app));
        assert!(domain.len() > app.len());
    }

    #[test]
    fn selector_labels_app_has_component_and_app_but_no_domain() {
        let labels = labels_app("acme", "supply-chain");
        let sel = selector_labels_app(&labels);
        assert!(sel.contains_key("app.kubernetes.io/component"));
        assert!(sel.contains_key("velocity.sh/app"));
        assert!(!sel.contains_key("velocity.sh/domain"));
    }

    #[test]
    fn deployment_app_injects_label_selector_not_namespace() {
        let settings = DataApiSettings {
            image: "ghcr.io/kinarix/velocity-api:1.2.3".into(),
            anonymous_auth: false,
            ingress_host: None,
            env_secret: Some("velocity-data-api-env".into()),
            env_source_secret: Some("velocity-data-api-env-source".into()),
            system_namespace: "velocity-system".into(),
        };
        let sel = "velocity.sh/org=acme,velocity.sh/app=supply-chain,velocity.sh/dedicated!=true";
        let labels = labels_app("acme", "supply-chain");
        let dep = build_deployment_app(&settings, "acme-supply-chain-shared", &labels, &cfg(), false, sel);
        let c = &dep.spec.unwrap().template.spec.unwrap().containers[0];
        let env = c.env.as_ref().unwrap();
        let got = env.iter().find(|e| e.name == "VELOCITY_API_LABEL_SELECTOR").unwrap();
        assert_eq!(got.value.as_deref(), Some(sel));
        assert!(env.iter().all(|e| e.name != "VELOCITY_API_NAMESPACE"),
            "app-scoped pod must not receive VELOCITY_API_NAMESPACE");
    }

    #[test]
    fn deployment_app_has_no_owner_reference() {
        let settings = DataApiSettings {
            image: "ghcr.io/kinarix/velocity-api:1.2.3".into(),
            anonymous_auth: false,
            ingress_host: None,
            env_secret: None,
            env_source_secret: None,
            system_namespace: "velocity-system".into(),
        };
        let labels = labels_app("acme", "supply-chain");
        let dep = build_deployment_app(&settings, "acme-supply-chain-shared", &labels, &cfg(), false,
            "velocity.sh/org=acme,velocity.sh/app=supply-chain,velocity.sh/dedicated!=true");
        assert!(dep.metadata.owner_references.is_none(),
            "app-scoped resources must not carry owner refs (cross-namespace GC not supported)");
    }

    #[test]
    fn hpa_app_respects_min_max_and_targets_deployment() {
        let labels = labels_app("acme", "supply-chain");
        let hpa = build_hpa_app("acme-supply-chain-shared", &labels, &cfg());
        let spec = hpa.spec.unwrap();
        assert_eq!(spec.min_replicas, Some(2));
        assert_eq!(spec.max_replicas, 7);
        assert_eq!(spec.scale_target_ref.name, NAME);
        assert!(hpa.metadata.owner_references.is_none());
    }

    #[test]
    fn meta_without_owner_has_no_owner_references() {
        let labels = labels_app("acme", "supply-chain");
        let m = meta_without_owner("acme-supply-chain-shared", &labels);
        assert!(m.owner_references.is_none());
        assert_eq!(m.name.as_deref(), Some(NAME));
        assert_eq!(m.namespace.as_deref(), Some("acme-supply-chain-shared"));
    }
}
