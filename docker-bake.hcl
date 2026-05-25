# docker buildx bake targets for local e2e.
#
# Usage:
#   docker buildx bake                              # build all "default" targets
#   docker buildx bake velocity-platform-api        # one target
#   docker buildx bake --load velocity              # build + load images into the local daemon
#
# Each target produces a `:dev` tag in the local daemon, which `scripts/k3d-up.sh`
# then imports into the k3d cluster.

group "default" {
    targets = [
        "velocity-operator",
        "velocity-webhook",
        "velocity-platform-api",
        "velocity-data-api",
        "velocity-search",
        "velocity-warm-reader",
        "velocity-archive-worker",
    ]
}

# Minimal-mode group — webhook only (matches VELOCITY_E2E_MINIMAL=1 in run.sh).
group "minimal" {
    targets = ["velocity-webhook"]
}

# All Rust binaries share the multi-stage Dockerfile at repo root and select
# their bin via the BIN build-arg. Platform left unset so buildx picks the
# host's native arch (linux/arm64 on Apple Silicon, linux/amd64 on x86).
target "_rust" {
    context    = "."
    dockerfile = "Dockerfile"
}

target "velocity-operator" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-operator" }
    tags     = ["velocity-operator:dev"]
}

target "velocity-webhook" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-webhook" }
    tags     = ["velocity-webhook:dev"]
}

target "velocity-platform-api" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-platform-api" }
    tags     = ["velocity-platform-api:dev"]
}

target "velocity-data-api" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-data-api" }
    tags     = ["velocity-data-api:dev"]
}

target "velocity-search" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-search" }
    tags     = ["velocity-search:dev"]
}

target "velocity-warm-reader" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-warm-reader" }
    tags     = ["velocity-warm-reader:dev"]
}

target "velocity-archive-worker" {
    inherits = ["_rust"]
    args     = { BIN = "velocity-archive-worker" }
    tags     = ["velocity-archive-worker:dev"]
}

