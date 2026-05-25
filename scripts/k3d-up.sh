#!/usr/bin/env bash
# Bring up velocity in a local k3d cluster.
#
# Cluster handling:
#   - no VELOCITY_CLUSTER set → picker:
#       lists existing k3d clusters, lets you pick one by number or 'n' for new
#   - VELOCITY_CLUSTER=<name> pinned → existing: use/recreate/new; missing: create
#
# Non-interactive overrides (skip prompts):
#   VELOCITY_CLUSTER=<name>                       pin cluster, skip picker
#   VELOCITY_AGENTS=<n>                           (default: 0)
#   VELOCITY_CLUSTER_ACTION=use|recreate|new      skip use/recreate/new prompt
#   VELOCITY_IMAGE_TAG=<tag>                      (default: dev)
#   VELOCITY_NAMESPACE=<ns>                       (default: velocity-system)
#   VELOCITY_RELEASE=<release>                    (default: velocity)
#   VELOCITY_HTTP_PORT=<port>                     (default: 8080)
#   VELOCITY_HTTPS_PORT=<port>                    (default: 8443)
#   VELOCITY_HOST=<host>                          (default: velocity.local)
#   VELOCITY_MINIMAL=1                            (webhook only; operator out-of-cluster)
#   VELOCITY_SKIP_BUILD=1                         (skip docker buildx bake)
#   VELOCITY_PROGRESS=auto|plain                  (buildx --progress)

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHART_DIR="${ROOT}/charts/velocity"
TLS_DIR="${ROOT}/data/webhook-tls"

CLUSTER="${VELOCITY_CLUSTER:-}"
CLUSTER_PINNED=0
[[ -n "${VELOCITY_CLUSTER:-}" ]] && CLUSTER_PINNED=1
DEFAULT_CLUSTER="velocity"
AGENTS="${VELOCITY_AGENTS:-0}"
CLUSTER_ACTION="${VELOCITY_CLUSTER_ACTION:-}"
IMAGE_TAG="${VELOCITY_IMAGE_TAG:-dev}"
NAMESPACE="${VELOCITY_NAMESPACE:-velocity-system}"
RELEASE="${VELOCITY_RELEASE:-velocity}"
HTTP_PORT="${VELOCITY_HTTP_PORT:-8080}"
HTTPS_PORT="${VELOCITY_HTTPS_PORT:-8443}"
HOST="${VELOCITY_HOST:-velocity.local}"
MINIMAL="${VELOCITY_MINIMAL:-0}"
SKIP_BUILD="${VELOCITY_SKIP_BUILD:-0}"
PROGRESS_MODE="${VELOCITY_PROGRESS:-auto}"

#-----------------------------------------------------------------------------
# helpers
#-----------------------------------------------------------------------------
log()  { printf '\033[1;32m▸ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }

ask() {
    local prompt="$1" default="$2" answer
    if [[ ! -t 0 ]]; then echo "$default"; return; fi
    read -r -p "$(printf '\033[1;36m? %s [%s]: \033[0m' "$prompt" "$default")" answer
    echo "${answer:-$default}"
}

confirm() {
    local prompt="$1" default="$2" answer suffix
    if [[ ! -t 0 ]]; then [[ "$default" == "Y" ]]; return; fi
    [[ "$default" == "Y" ]] && suffix="[Y/n]" || suffix="[y/N]"
    read -r -p "$(printf '\033[1;36m? %s %s: \033[0m' "$prompt" "$suffix")" answer
    answer="${answer:-$default}"
    [[ "$answer" =~ ^[Yy] ]]
}

require() {
    local tool="$1" hint="$2"
    command -v "$tool" >/dev/null 2>&1 || fail "missing tool: $tool — $hint"
}

require k3d     "brew install k3d"
require kubectl "brew install kubernetes-cli"
require helm    "brew install helm"
require docker  "install Docker Desktop or OrbStack"
require openssl "should ship with macOS / Linux"
docker info >/dev/null 2>&1 || fail "docker daemon is not running"

#-----------------------------------------------------------------------------
# 1. cluster: detect, prompt, create
#-----------------------------------------------------------------------------
# Accepts what k3d lets you name a cluster (letters, digits, dot, dash,
# underscore). Defends against k3d leaking debug lines onto stdout.
_valid_cluster_name='^[a-zA-Z0-9][a-zA-Z0-9._-]*$'

list_clusters() {
    k3d cluster list --no-headers 2>/dev/null \
        | awk '{print $1}' \
        | grep -E "$_valid_cluster_name" || true
}

cluster_exists() {
    list_clusters | grep -Fxq "$CLUSTER"
}

pick_cluster() {
    local existing
    existing=$(list_clusters)
    if [[ -z "$existing" ]]; then
        log "no existing k3d clusters found"
        CLUSTER=$(ask "Name for the new cluster" "$DEFAULT_CLUSTER")
        CLUSTER_ACTION="create"
        return
    fi

    echo
    echo "Existing k3d clusters:"
    local i=1
    local default_idx=1
    local -a names=()
    while IFS= read -r name; do
        printf "  [%d] %s\n" "$i" "$name"
        names+=("$name")
        [[ "$name" == "$DEFAULT_CLUSTER" ]] && default_idx=$i
        ((i++))
    done <<< "$existing"
    printf "  [n] create a new cluster\n\n"

    local choice
    choice=$(ask "Pick a cluster (number) or 'n' to create new" "$default_idx")

    if [[ "$choice" == "n" || "$choice" == "N" ]]; then
        CLUSTER=$(ask "Name for the new cluster" "$DEFAULT_CLUSTER")
        CLUSTER_ACTION="create"
    elif [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#names[@]} )); then
        CLUSTER="${names[$((choice - 1))]}"
        CLUSTER_ACTION="use"
        ok "selected existing cluster '${CLUSTER}'"
    else
        fail "invalid choice: $choice"
    fi
}

create_cluster() {
    log "creating k3d cluster '${CLUSTER}' (HTTP/HTTPS on host ${HTTP_PORT}/${HTTPS_PORT})"
    local args=(--wait -p "${HTTP_PORT}:80@loadbalancer" -p "${HTTPS_PORT}:443@loadbalancer")
    (( AGENTS > 0 )) && args+=(--agents "$AGENTS")
    k3d cluster create "$CLUSTER" "${args[@]}"
    ok "cluster '${CLUSTER}' created"
}

delete_cluster() {
    log "deleting k3d cluster '${CLUSTER}'"
    k3d cluster delete "$CLUSTER"
}

set_kubectl_context() {
    kubectl config use-context "k3d-${CLUSTER}" >/dev/null
}

resolve_cluster() {
    if cluster_exists; then
        if [[ -z "$CLUSTER_ACTION" ]]; then
            warn "cluster '${CLUSTER}' already exists"
            CLUSTER_ACTION=$(ask "(u)se existing / (r)ecreate / (n)ew name?" "u")
        fi
        case "$CLUSTER_ACTION" in
            u|use)      log "using existing cluster '${CLUSTER}'" ;;
            r|recreate) delete_cluster; create_cluster ;;
            c|create)   warn "cluster '${CLUSTER}' already exists — using it"; CLUSTER_ACTION=use ;;
            n|new)
                CLUSTER=$(ask "New cluster name" "${CLUSTER}-2")
                CLUSTER_ACTION=""
                resolve_cluster
                return
                ;;
            *) fail "unknown action: $CLUSTER_ACTION" ;;
        esac
    else
        if [[ -z "$CLUSTER_ACTION" ]] || [[ "$CLUSTER_ACTION" == "use" ]]; then
            if [[ "$CLUSTER_ACTION" == "use" ]]; then
                fail "cluster '${CLUSTER}' not found (CLUSTER_ACTION=use)"
            fi
            confirm "Cluster '${CLUSTER}' not found. Create it?" "Y" \
                || fail "no cluster — aborting"
        fi
        create_cluster
    fi
    set_kubectl_context
}

if (( CLUSTER_PINNED == 0 )); then
    pick_cluster
fi
: "${CLUSTER:=$DEFAULT_CLUSTER}"
resolve_cluster
kubectl cluster-info >/dev/null 2>&1 || fail "kubectl cannot reach the cluster"
ok "k3d cluster '${CLUSTER}' reachable"

#-----------------------------------------------------------------------------
# 2. docker-compose deps (postgres/redis/kafka/typesense/minio on the host)
#-----------------------------------------------------------------------------
# values-dev.yaml points in-cluster services at host.k3d.internal for these.
log "ensuring docker-compose infra is up"
(cd "$ROOT" && docker compose up -d --wait) >/dev/null
ok "compose deps healthy (postgres :5434, redis :6380, typesense :8108, minio :9000)"

#-----------------------------------------------------------------------------
# 3. webhook TLS
#-----------------------------------------------------------------------------
log "generating webhook self-signed cert"
mkdir -p "${TLS_DIR}"
SVC="${RELEASE}-webhook.${NAMESPACE}.svc"
cat > "${TLS_DIR}/openssl.cnf" <<EOF
[ req ]
distinguished_name = dn
prompt             = no
req_extensions     = v3_req
[ dn ]
CN = ${SVC}
[ v3_req ]
basicConstraints = critical, CA:TRUE
keyUsage         = critical, digitalSignature, keyEncipherment, keyCertSign
extendedKeyUsage = serverAuth
subjectAltName   = @alt
[ alt ]
DNS.1 = ${SVC}
DNS.2 = ${SVC}.cluster.local
EOF
openssl req -x509 -nodes -newkey rsa:2048 -days 365 \
    -keyout "${TLS_DIR}/tls.key" \
    -out    "${TLS_DIR}/tls.crt" \
    -config "${TLS_DIR}/openssl.cnf" \
    -extensions v3_req >/dev/null 2>&1
CA_BUNDLE="$(base64 < "${TLS_DIR}/tls.crt" | tr -d '\n')"
ok "tls cert generated at ${TLS_DIR}"

#-----------------------------------------------------------------------------
# 4. build + load images via docker buildx bake
#-----------------------------------------------------------------------------
if [[ "$SKIP_BUILD" == "1" ]]; then
    warn "skipping image build (VELOCITY_SKIP_BUILD=1)"
else
    if [[ "$MINIMAL" == "1" ]]; then
        BAKE_GROUP="minimal"
        log "MINIMAL mode — building webhook image only"
    else
        BAKE_GROUP="default"
        log "building all velocity images via 'docker buildx bake' (parallel)"
    fi
    (cd "${ROOT}" && docker buildx bake --load --progress="${PROGRESS_MODE}" "${BAKE_GROUP}") \
        || fail "image build failed — re-run with VELOCITY_PROGRESS=plain for full logs"
    ok "images built"
fi

if [[ "$MINIMAL" == "1" ]]; then
    IMAGES=("velocity-webhook:${IMAGE_TAG}")
else
    IMAGES=(
        "velocity-operator:${IMAGE_TAG}"
        "velocity-webhook:${IMAGE_TAG}"
        "velocity-api:${IMAGE_TAG}"
        "velocity-warm-reader:${IMAGE_TAG}"
        "velocity-archive-worker:${IMAGE_TAG}"
    )
fi
log "loading ${#IMAGES[@]} image(s) into k3d cluster '${CLUSTER}'"
k3d image import "${IMAGES[@]}" -c "$CLUSTER" >/dev/null
ok "images present in cluster"

#-----------------------------------------------------------------------------
# 5. apply CRDs + create namespace + webhook TLS Secret
#-----------------------------------------------------------------------------
log "applying CRDs"
kubectl apply -f "${ROOT}/crds/" >/dev/null
ok "CRDs applied"

kubectl create ns "${NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n "${NAMESPACE}" create secret tls "${RELEASE}-webhook-tls" \
    --cert="${TLS_DIR}/tls.crt" \
    --key="${TLS_DIR}/tls.key" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null

#-----------------------------------------------------------------------------
# 6. helm install
#-----------------------------------------------------------------------------
HELM_BASE=(
    helm upgrade --install "${RELEASE}" "${CHART_DIR}"
    --namespace "${NAMESPACE}"
    --skip-crds
    -f "${CHART_DIR}/values.yaml"
    -f "${CHART_DIR}/values-dev.yaml"
    --set fullnameOverride="${RELEASE}"
    --set image.registry=docker.io
    --set image.repository=library
    --set image.pullPolicy=IfNotPresent
    --set webhook.image.tag="${IMAGE_TAG}"
    --set webhook.tls.caBundle="${CA_BUNDLE}"
    --set webhook.tls.existingSecret="${RELEASE}-webhook-tls"
    --set "api.ingress.hosts[0].host=${HOST}"
    --wait --timeout=180s
)

if [[ "$MINIMAL" == "1" ]]; then
    log "installing chart (MINIMAL: webhook only — operator runs out-of-cluster)"
    "${HELM_BASE[@]}" \
        --set operator.enabled=false \
        --set api.enabled=false \
        --set warmReader.enabled=false \
        --set archiveWorker.enabled=false \
        --set logProcessor.enabled=false \
        --set logCollector.enabled=false
else
    log "installing chart (full stack)"
    "${HELM_BASE[@]}" \
        --set operator.image.tag="${IMAGE_TAG}" \
        --set api.image.tag="${IMAGE_TAG}" \
        --set warmReader.image.tag="${IMAGE_TAG}" \
        --set archiveWorker.image.tag="${IMAGE_TAG}"
fi
ok "helm release '${RELEASE}' ready"

# /etc/hosts check — the portal Ingress is host-scoped, so the browser needs
# to resolve $HOST to 127.0.0.1. We don't edit /etc/hosts ourselves (sudo).
if ! grep -qE "^[^#]*\b${HOST//./\\.}\b" /etc/hosts 2>/dev/null; then
    warn "${HOST} is not in /etc/hosts — add it with:"
    echo "    echo '127.0.0.1 ${HOST}' | sudo tee -a /etc/hosts"
fi

#-----------------------------------------------------------------------------
# 7. summary
#-----------------------------------------------------------------------------
cat <<EOF

$(ok "velocity is up")

  Portal + API:  http://${HOST}:${HTTP_PORT}/        (SPA at /, API at /api, embedded in one binary)
  Minio console: http://localhost:9001/              (user: velocity / pass: velocity_dev)

  make k3d-logs       # tail aggregate logs
  make k3d-status     # list resources in ${NAMESPACE}
  make k3d-redeploy   # rebuild + roll the pods
  make k3d-clean      # helm uninstall + namespace cleanup (cluster kept)

  Tear down the cluster entirely:
    k3d cluster delete ${CLUSTER}
EOF
