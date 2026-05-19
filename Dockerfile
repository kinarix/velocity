# syntax=docker/dockerfile:1.7
#
# Multi-stage build for any velocity workspace binary.
# Usage:
#   docker build --build-arg BIN=velocity-operator -t velocity-operator:dev .
#   docker build --build-arg BIN=velocity-webhook  -t velocity-webhook:dev  .

ARG RUST_VERSION=1.95
ARG DEBIAN_VERSION=bookworm

############################
# 1. Builder
############################
FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS builder

ARG BIN
ENV CARGO_TERM_COLOR=always

WORKDIR /work

# Cache deps: copy manifests first, fetch, then copy sources.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/

# Workspace deps + binary build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/work/target \
    cargo build --release --bin ${BIN} \
 && cp /work/target/release/${BIN} /usr/local/bin/app

############################
# 2. Runtime
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
