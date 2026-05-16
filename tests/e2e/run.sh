#!/usr/bin/env bash
# Phase 0 end-to-end test against a local minikube cluster.
#
# Acceptance criteria (see docs/phases.md, "Phase 0"):
#   1. CRDs install cleanly.
#   2. Webhook rejects a Domain whose namespace mismatches `org-app`.
#   3. Webhook accepts a well-formed Domain.
#   4. Operator provisions the Postgres schema + roles for that Domain.
#
# Prereqs (run on host):
#   - docker compose stack running (Postgres on :5434, migrations applied)
#       make up && make db-bootstrap && make migrate
#   - minikube running
#   - kubectl / helm / openssl in PATH

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NS="velocity-system"
RELEASE="velocity"
TLS_DIR="${ROOT}/data/webhook-tls"
PG_HOST="host.minikube.internal"
PG_PORT="5434"
PG_USER="velocity_operator"
PG_DB="velocity"
PG_PASSWORD="velocity_operator_dev"

# Operator is run from outside the cluster for Phase 0 — it watches
# minikube's API server and provisions schemas in the docker-compose
# Postgres. The webhook runs in-cluster.
OPERATOR_PID=""
PORT_FORWARD_PID=""

#-----------------------------------------------------------------------------
# helpers
#-----------------------------------------------------------------------------
log()  { printf '\033[1;32m▸ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }

cleanup() {
    log "cleanup"
    # Kill the wrapper subshell AND the cargo/operator child it spawned.
    [[ -n "$OPERATOR_PID" ]] && kill "$OPERATOR_PID" 2>/dev/null || true
    pkill -f 'target/(debug|release)/velocity-operator' 2>/dev/null || true
    [[ -n "$PORT_FORWARD_PID" ]] && kill "$PORT_FORWARD_PID" 2>/dev/null || true
    kubectl delete domain procurement -n acme-supply-chain --ignore-not-found --timeout=10s >/dev/null 2>&1 || true
    kubectl delete ns acme-supply-chain --ignore-not-found --timeout=30s >/dev/null 2>&1 || true
}
trap cleanup EXIT

require() { command -v "$1" >/dev/null 2>&1 || fail "missing tool: $1"; }
require kubectl
require helm
require minikube
require openssl
require docker

#-----------------------------------------------------------------------------
# 1. ensure minikube
#-----------------------------------------------------------------------------
log "verifying minikube"
minikube status >/dev/null 2>&1 || fail "minikube is not running — run: minikube start"
kubectl cluster-info >/dev/null 2>&1 || fail "kubectl cannot reach the cluster"
ok "minikube up"

#-----------------------------------------------------------------------------
# 2. generate self-signed TLS for the webhook
#-----------------------------------------------------------------------------
log "generating webhook self-signed cert"
mkdir -p "${TLS_DIR}"
SVC="${RELEASE}-webhook.${NS}.svc"
cat > "${TLS_DIR}/openssl.cnf" <<EOF
[ req ]
distinguished_name = dn
prompt             = no
req_extensions     = v3_req
[ dn ]
CN = ${SVC}
[ v3_req ]
keyUsage         = critical, digitalSignature, keyEncipherment
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
# 3. build images into minikube's daemon
#-----------------------------------------------------------------------------
log "building webhook image into minikube"
eval "$(minikube docker-env)"
docker build --build-arg BIN=velocity-webhook -t velocity-webhook:dev "${ROOT}"
eval "$(minikube docker-env -u)"
ok "image velocity-webhook:dev present in minikube"

#-----------------------------------------------------------------------------
# 4. install chart
#-----------------------------------------------------------------------------
log "applying CRDs"
kubectl apply -f "${ROOT}/crds/" >/dev/null
ok "CRDs applied"

log "installing chart (webhook only — operator runs out-of-cluster)"
kubectl create ns "${NS}" --dry-run=client -o yaml | kubectl apply -f -
kubectl -n "${NS}" create secret tls "${RELEASE}-webhook-tls" \
    --cert="${TLS_DIR}/tls.crt" \
    --key="${TLS_DIR}/tls.key" \
    --dry-run=client -o yaml | kubectl apply -f -

helm upgrade --install "${RELEASE}" "${ROOT}/charts/velocity" \
    --namespace "${NS}" \
    --skip-crds \
    -f "${ROOT}/charts/velocity/values-dev.yaml" \
    --set fullnameOverride="${RELEASE}" \
    --set operator.enabled=false \
    --set webhook.image.tag=dev \
    --set webhook.replicaCount=1 \
    --set webhook.failurePolicy=Fail \
    --set image.registry=docker.io \
    --set image.repository=library \
    --set image.pullPolicy=IfNotPresent \
    --set webhook.tls.caBundle="${CA_BUNDLE}" \
    --set webhook.tls.existingSecret="${RELEASE}-webhook-tls" \
    --wait --timeout=120s
ok "helm release ready"

#-----------------------------------------------------------------------------
# 5. operator: run on host, talk to minikube API + docker-compose Postgres
#-----------------------------------------------------------------------------
log "starting operator out-of-cluster"
# Kill any stale operator from a previous failed run so we don't fight for :8081.
pkill -f 'target/(debug|release)/velocity-operator' 2>/dev/null || true
sleep 1
export VELOCITY_OPERATOR_PG_URL="postgres://${PG_USER}:${PG_PASSWORD}@localhost:${PG_PORT}/${PG_DB}"
export VELOCITY_OPERATOR_PRETTY_LOGS=1
export RUST_LOG="info,velocity_operator=debug"
( cd "${ROOT}" && exec cargo run --quiet --bin velocity-operator ) \
    >/tmp/velocity-operator.log 2>&1 &
OPERATOR_PID=$!
sleep 12
pgrep -f 'target/(debug|release)/velocity-operator' >/dev/null \
    || { warn "operator log:"; cat /tmp/velocity-operator.log; fail "operator died"; }
ok "operator pid=${OPERATOR_PID}"

#-----------------------------------------------------------------------------
# 6. acceptance check A — webhook DENIES a namespace mismatch
#-----------------------------------------------------------------------------
log "TEST A — webhook should DENY namespace mismatch"
kubectl create ns acme-supply --dry-run=client -o yaml | kubectl apply -f - >/dev/null
set +e
out=$(kubectl apply -n acme-supply -f - 2>&1 <<'EOF'
apiVersion: velocity.sh/v1
kind: Domain
metadata:
  name: procurement
  labels: { "velocity.sh/org": "acme" }
spec:
  app: supply-chain
  displayName: Procurement
  access: { defaultRole: viewer, adminRole: domain-admin }
EOF
)
rc=$?
set -e
kubectl delete ns acme-supply --ignore-not-found --timeout=30s >/dev/null 2>&1 || true
if [[ $rc -eq 0 ]]; then fail "webhook unexpectedly admitted mismatch: $out"; fi
echo "$out" | grep -q "acme-supply-chain" \
    || fail "deny message missing expected namespace: $out"
ok "TEST A passed — deny message mentions expected ns"

#-----------------------------------------------------------------------------
# 7. acceptance check B — webhook ADMITS a valid Domain
#-----------------------------------------------------------------------------
log "TEST B — webhook should ADMIT a valid Domain"
kubectl create ns acme-supply-chain --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl apply -n acme-supply-chain -f - <<'EOF'
apiVersion: velocity.sh/v1
kind: Domain
metadata:
  name: procurement
  labels: { "velocity.sh/org": "acme" }
spec:
  app: supply-chain
  displayName: Procurement
  access: { defaultRole: viewer, adminRole: domain-admin }
EOF
ok "TEST B passed — apply succeeded"

#-----------------------------------------------------------------------------
# 8. acceptance check C — operator provisions Postgres schema
#-----------------------------------------------------------------------------
log "TEST C — operator should provision acme_supply_chain_procurement schema"
SCHEMA="acme_supply_chain_procurement"
for i in $(seq 1 30); do
    # use the superuser to query — velocity_api/operator both can SELECT
    # information_schema but we want zero-friction visibility here.
    found=$(docker compose -f "${ROOT}/docker-compose.yml" exec -T -e PGPASSWORD=postgres \
        postgres psql -U postgres -d "${PG_DB}" -tA -c \
        "SELECT 1 FROM information_schema.schemata WHERE schema_name='${SCHEMA}'" 2>/dev/null || true)
    if [[ "$found" == "1" ]]; then break; fi
    sleep 2
done
[[ "$found" == "1" ]] || { warn "operator log:"; tail -50 /tmp/velocity-operator.log; fail "schema ${SCHEMA} not created within 60s"; }
ok "TEST C passed — schema ${SCHEMA} provisioned"

#-----------------------------------------------------------------------------
# 9. acceptance check D — status reflects Ready
#-----------------------------------------------------------------------------
log "TEST D — Domain.status.phase should be Ready"
for i in $(seq 1 15); do
    phase=$(kubectl get domain procurement -n acme-supply-chain -o jsonpath='{.status.phase}' 2>/dev/null || true)
    if [[ "$phase" == "Ready" ]]; then break; fi
    sleep 2
done
[[ "$phase" == "Ready" ]] || { kubectl get domain procurement -n acme-supply-chain -o yaml; fail "status.phase != Ready (got: '${phase}')"; }
ok "TEST D passed — status.phase=Ready"

echo
ok "Phase 0 end-to-end acceptance: ALL TESTS PASSED"
