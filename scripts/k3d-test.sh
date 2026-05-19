#!/usr/bin/env bash
# Run `cargo test --workspace` inside an ephemeral k3d cluster.
#
# Behavior:
#   - If k3d is not installed: prints a warning and runs cargo test as-is.
#     Tests that need a kube cluster will get a "kubeconfig not found"
#     error and fail — that's expected; they need k3d.
#   - If k3d is installed: creates a fresh test cluster, writes a
#     scoped KUBECONFIG (so we don't clobber the user's), applies the
#     repo's CRDs, runs cargo test, then ALWAYS tears the cluster down
#     (including on Ctrl-C or test failure).
#
# Pass extra cargo args after `--`:
#   ./scripts/k3d-test.sh -- --test webhook_integration -- --nocapture
#
# Env knobs:
#   K3D_CLUSTER       cluster name (default: velocity-test-$$). Unique per
#                     PID so parallel invocations don't collide.
#   K3D_KEEP=1        skip teardown on success (still tears down on
#                     failure). Useful for poking at the cluster after
#                     a flake. Cluster name is printed at the start.
#   K3D_REUSE=1       attach to an existing cluster of this name
#                     instead of creating a new one. Skips teardown.
#                     For iterative local dev.
#   APPLY_CRDS=0      skip `kubectl apply -f crds/`. Default: apply.

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLUSTER="${K3D_CLUSTER:-velocity-test-$$}"
KUBECONFIG_FILE="${ROOT}/data/.k3d-test-kubeconfig-$$"
APPLY_CRDS="${APPLY_CRDS:-1}"

# Cargo args passed through to `cargo test`. Default: --workspace.
# If the caller passed `--`, everything after it goes to cargo.
CARGO_ARGS=("--workspace")
if [[ $# -gt 0 ]]; then
  if [[ "$1" == "--" ]]; then
    shift
    CARGO_ARGS=("$@")
  else
    CARGO_ARGS=("$@")
  fi
fi

log()  { printf '\033[36m[k3d-test]\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[33m[k3d-test]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[31m[k3d-test]\033[0m %s\n' "$*" >&2; }

# ── No k3d?  Fall through to plain cargo test. ────────────────────────────
if ! command -v k3d >/dev/null 2>&1; then
  warn "k3d not found in PATH — running cargo test without a kube cluster."
  warn "Install with:  brew install k3d   (macOS)"
  warn "         or:  curl -s https://raw.githubusercontent.com/k3d-io/k3d/main/install.sh | bash   (Linux)"
  exec cargo test "${CARGO_ARGS[@]}"
fi

if ! command -v kubectl >/dev/null 2>&1; then
  err "k3d is installed but kubectl is not — kube tests will fail."
  err "Install with:  brew install kubectl"
  exit 1
fi

mkdir -p "$(dirname "$KUBECONFIG_FILE")"

# Sweep any stale kubectl contexts/clusters/users from prior interrupted
# runs of this script. We name them deterministically (`k3d-velocity-test-*`,
# which is what k3d prepends), so this targets only our own leftovers and
# leaves the user's other contexts alone.
sweep_stale_default_kubeconfig() {
  local kubeconfig="${HOME}/.kube/config"
  [[ -f "$kubeconfig" ]] || return 0
  # `--kubeconfig` here so we never accidentally read $KUBECONFIG which
  # we may have already overridden to the per-run file.
  local contexts
  contexts=$(KUBECONFIG="$kubeconfig" kubectl config get-contexts -o name 2>/dev/null | grep -E '^k3d-velocity-test-' || true)
  if [[ -z "$contexts" ]]; then return 0; fi
  while IFS= read -r ctx; do
    local cluster_name="${ctx#k3d-}"
    log "removing stale kubeconfig entry: $ctx"
    KUBECONFIG="$kubeconfig" kubectl config delete-context "$ctx"      >/dev/null 2>&1 || true
    KUBECONFIG="$kubeconfig" kubectl config delete-cluster "$ctx"      >/dev/null 2>&1 || true
    KUBECONFIG="$kubeconfig" kubectl config delete-user    "admin@${cluster_name}" >/dev/null 2>&1 || true
  done <<< "$contexts"
}

# ── Teardown handler — runs on any exit (clean, failure, or signal). ─────
TEARDOWN_DONE=0
teardown() {
  # Ensure we only run teardown once even if the trap fires multiple
  # times (EXIT + ERR can both fire on certain failures).
  if [[ "$TEARDOWN_DONE" == 1 ]]; then return 0; fi
  TEARDOWN_DONE=1

  local rc=$?
  if [[ "${K3D_REUSE:-0}" == 1 ]]; then
    log "K3D_REUSE=1 — leaving cluster '$CLUSTER' running."
  elif [[ "$rc" == 0 && "${K3D_KEEP:-0}" == 1 ]]; then
    log "K3D_KEEP=1 and tests passed — leaving cluster '$CLUSTER' running."
    log "  kubeconfig: $KUBECONFIG_FILE"
    log "  teardown:   k3d cluster delete $CLUSTER"
  else
    log "tearing down cluster '$CLUSTER'…"
    k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
    rm -f "$KUBECONFIG_FILE"
    # k3d cluster delete already removes its own entry from the user's
    # default kubeconfig, but on a SIGPIPE / SIGKILL during create we
    # might leave one behind. Also sweep any other k3d-velocity-test-*
    # contexts left over from earlier crashed runs.
    sweep_stale_default_kubeconfig
  fi
  exit "$rc"
}
trap teardown EXIT INT TERM HUP PIPE

# ── Create (or attach to) the cluster. ──────────────────────────────────
if [[ "${K3D_REUSE:-0}" == 1 ]] && k3d cluster list "$CLUSTER" >/dev/null 2>&1; then
  log "reusing existing cluster '$CLUSTER'"
else
  log "creating k3d cluster '$CLUSTER' (this takes ~20s on first boot)…"
  # `--wait` blocks until the apiserver is responsive.
  # `--no-lb` skips the loadbalancer (unit tests talk to apiserver directly).
  # `--k3s-arg='--disable=...'` strips components we don't need to save memory.
  # `--kubeconfig-update-default=false` keeps the user's ~/.kube/config
  #   untouched — we read the cluster config straight into our per-run file
  #   below. (Defence-in-depth: sweep_stale_default_kubeconfig also runs at
  #   teardown for legacy/crash leftovers.)
  k3d cluster create "$CLUSTER" \
    --wait \
    --no-lb \
    --kubeconfig-update-default=false \
    --k3s-arg='--disable=traefik@server:*' \
    --k3s-arg='--disable=servicelb@server:*' \
    --k3s-arg='--disable=metrics-server@server:*' \
    >/dev/null
fi

# Write a kubeconfig scoped to this run so KUBECONFIG_FILE points at it
# without merging into ~/.kube/config.
k3d kubeconfig get "$CLUSTER" > "$KUBECONFIG_FILE"
export KUBECONFIG="$KUBECONFIG_FILE"

log "apiserver: $(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}' 2>/dev/null || echo unknown)"

# ── Apply CRDs so kube-rs Api<T> calls have a schema to talk to. ─────────
if [[ "$APPLY_CRDS" == 1 && -d "${ROOT}/crds" ]]; then
  log "applying CRDs from crds/"
  # `--server-side` avoids the last-applied-configuration annotation,
  # which keeps the cluster state clean for inspection on failure.
  kubectl apply --server-side -f "${ROOT}/crds/" >/dev/null
fi

log "running: cargo test ${CARGO_ARGS[*]}"
cargo test "${CARGO_ARGS[@]}"
