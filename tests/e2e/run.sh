#!/usr/bin/env bash
# Phase 0 acceptance suite for velocity.
#
# Assumes a velocity helm release is already up in a k3d cluster (typically
# via `make k3d-up`). Cluster lifecycle, image building, helm install, and
# webhook TLS generation all live in scripts/k3d-up.sh — this script only
# exercises behavioural assertions:
#
#   TEST A — webhook DENIES a Domain whose namespace mismatches `org-app`
#   TEST B — webhook ADMITS a well-formed Domain
#   TEST C — operator provisions the Postgres schema + roles
#   TEST D — Domain.status.phase reaches Ready
#
# Non-interactive overrides:
#   VELOCITY_E2E_NAMESPACE=<ns>   (default: velocity-system)
#   VELOCITY_E2E_RELEASE=<name>   (default: velocity)
#   VELOCITY_E2E_SAMPLES=1        (apply samples/ after acceptance suite)
#
# Prereqs:
#   1. `make k3d-up` has been run (cluster + helm release ready)
#   2. docker-compose Postgres on :5434 reachable from the host (the cluster
#      pods reach it via host.k3d.internal)

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NS="${VELOCITY_E2E_NAMESPACE:-velocity-system}"
RELEASE="${VELOCITY_E2E_RELEASE:-velocity}"
SAMPLES="${VELOCITY_E2E_SAMPLES:-0}"
PG_DB="velocity"

#-----------------------------------------------------------------------------
# helpers
#-----------------------------------------------------------------------------
log()  { printf '\033[1;32m▸ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }

cleanup() {
    log "cleanup"
    kubectl delete domain procurement -n acme-supply-chain --ignore-not-found --timeout=10s >/dev/null 2>&1 || true
    kubectl delete ns acme-supply-chain --ignore-not-found --timeout=30s >/dev/null 2>&1 || true
}
trap cleanup EXIT

require() { command -v "$1" >/dev/null 2>&1 || fail "missing tool: $1"; }
require kubectl
require docker

#-----------------------------------------------------------------------------
# 0. preflight — cluster must be up, release must exist
#-----------------------------------------------------------------------------
kubectl cluster-info >/dev/null 2>&1 \
    || fail "kubectl cannot reach a cluster — run 'make k3d-up' first"

ctx=$(kubectl config current-context 2>/dev/null || true)
case "$ctx" in
    k3d-*) ok "context: ${ctx}" ;;
    *)     warn "current context '${ctx}' is not a k3d cluster — continuing anyway" ;;
esac

if ! kubectl get ns "${NS}" >/dev/null 2>&1; then
    fail "namespace '${NS}' not found — run 'make k3d-up' first"
fi
if ! kubectl -n "${NS}" get deploy "${RELEASE}-webhook" >/dev/null 2>&1; then
    fail "webhook deployment '${RELEASE}-webhook' not found in '${NS}' — run 'make k3d-up' first"
fi
ok "release '${RELEASE}' present in '${NS}'"

#-----------------------------------------------------------------------------
# 1. acceptance tests A–D
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
ok "TEST A passed"

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
ok "TEST B passed"

log "TEST C — operator should provision acme_supply_chain_procurement schema"
SCHEMA="acme_supply_chain_procurement"
for i in $(seq 1 30); do
    found=$(docker compose -f "${ROOT}/docker-compose.yml" exec -T -e PGPASSWORD=postgres \
        postgres psql -U postgres -d "${PG_DB}" -tA -c \
        "SELECT 1 FROM information_schema.schemata WHERE schema_name='${SCHEMA}'" 2>/dev/null || true)
    if [[ "$found" == "1" ]]; then break; fi
    sleep 2
done
[[ "$found" == "1" ]] || {
    warn "operator logs:"
    kubectl -n "$NS" logs deploy/${RELEASE}-operator --tail=50 || true
    fail "schema ${SCHEMA} not created within 60s"
}
ok "TEST C passed"

log "TEST D — Domain.status.phase should be Ready"
for i in $(seq 1 15); do
    phase=$(kubectl get domain procurement -n acme-supply-chain -o jsonpath='{.status.phase}' 2>/dev/null || true)
    if [[ "$phase" == "Ready" ]]; then break; fi
    sleep 2
done
[[ "$phase" == "Ready" ]] \
    || { kubectl get domain procurement -n acme-supply-chain -o yaml; fail "status.phase != Ready (got: '${phase}')"; }
ok "TEST D passed"

#-----------------------------------------------------------------------------
# 2. optional: apply samples/
#-----------------------------------------------------------------------------
if [[ "$SAMPLES" == "1" ]]; then
    log "applying samples/ (excluding 11-purgerequest)"
    for f in "${ROOT}"/samples/*.yaml; do
        [[ "$f" == *"11-purgerequest.yaml" ]] && continue
        kubectl apply -f "$f" || warn "  applying $f failed (continuing)"
    done
    ok "samples applied"
fi

echo
ok "Velocity acceptance suite: ALL TESTS PASSED"
