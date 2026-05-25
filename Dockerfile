# syntax=docker/dockerfile:1.7
#
# Multi-stage build for any velocity workspace binary (BIN arg).
# Usage:
#   docker build --build-arg BIN=velocity-operator     -t velocity-operator:dev     .
#   docker build --build-arg BIN=velocity-webhook      -t velocity-webhook:dev      .
#   docker build --build-arg BIN=velocity-platform-api -t velocity-platform-api:dev .
#   docker build --build-arg BIN=velocity-data-api     -t velocity-data-api:dev     .
#   docker build --build-arg BIN=velocity-search       -t velocity-search:dev       .
#
# Phase 12a (ADR-011) split the old `velocity-api` binary into three:
#   * velocity-platform-api — admin/UI backend (serves the embedded SPA)
#   * velocity-data-api     — per-domain CRUD/query/time-machine/archive
#   * velocity-search       — all Tier-3 search + CDC
# Only `velocity-platform-api` embeds + serves the portal SPA (via
# `rust-embed`); `velocity-data-api` and `velocity-search` don't link any UI
# code. Stage 1 builds the Vite bundle; stage 2 copies `dist/` into
# `crates/velocity-platform-api/static/` before `cargo build` so the macro
# picks it up. For non-platform builds the folder is simply empty.

ARG RUST_VERSION=1.95
ARG DEBIAN_VERSION=bookworm
ARG NODE_VERSION=22

############################
# 1. Portal SPA builder
############################
# Always run — small (<60s cold, ~5s warm), cached across all targets.
# When the portal sources are unchanged this stage's layers reuse the
# previous result, so even cold operator/webhook builds barely pay for it.
FROM node:${NODE_VERSION}-alpine AS portal-builder

WORKDIR /portal

# Cache deps separately from sources.
COPY portal/package.json portal/package-lock.json* ./
RUN --mount=type=cache,target=/root/.npm \
    npm install --no-audit --no-fund --include=dev

COPY portal/tsconfig.json portal/tsconfig.app.json portal/tsconfig.node.json \
     portal/vite.config.ts portal/tailwind.config.ts portal/postcss.config.js \
     portal/index.html ./
COPY portal/src/   ./src/
COPY portal/tests/ ./tests/

# Type-check + bundle. tsc -b errors propagate to the build.
RUN npm run build

############################
# 2. Rust builder
############################
FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS builder

ARG BIN
ENV CARGO_TERM_COLOR=always

WORKDIR /work

# Cache deps: copy manifests first, fetch, then copy sources.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/

# Drop the portal bundle where rust-embed expects it. The folder is
# committed empty in the repo (with a README placeholder), and is on
# .gitignore so local cargo runs don't see stale assets.
COPY --from=portal-builder /portal/dist/ /work/crates/velocity-platform-api/static/

# Workspace deps + binary build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/work/target \
    cargo build --release --bin ${BIN} \
 && cp /work/target/release/${BIN} /usr/local/bin/app

############################
# 3. Runtime
############################
FROM debian:${DEBIAN_VERSION}-slim AS runtime

# CA bundle (Postgres TLS, JWKS, OTel exporter, etc.) + tini for PID 1.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# Non-root runtime user.
RUN groupadd --system --gid 65532 velocity \
 && useradd  --system --uid 65532 --gid velocity --no-create-home velocity

COPY --from=builder /usr/local/bin/app /usr/local/bin/app

USER 65532:65532
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/app"]
