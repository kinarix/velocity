# syntax=docker/dockerfile:1.7
#
# Two-stage build for the React SPA. Stage 1 builds the static bundle with
# Node; stage 2 serves it from nginx with /api and /auth proxied to the
# velocity-api Service (configured via env at runtime).

ARG NODE_VERSION=22
ARG NGINX_VERSION=1.27

############################
# 1. Builder
############################
FROM node:${NODE_VERSION}-alpine AS builder

WORKDIR /work

# Cache deps separately from sources so source-only changes don't
# re-fetch every npm package.
COPY package.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm install --no-audit --no-fund --include=dev

COPY tsconfig.json tsconfig.app.json tsconfig.node.json vite.config.ts \
     tailwind.config.ts postcss.config.js index.html ./
COPY src/   ./src/
COPY tests/ ./tests/

# Type-check + bundle. tsc -b errors propagate to the build.
RUN npm run build

############################
# 2. Runtime — nginx
############################
FROM nginx:${NGINX_VERSION}-alpine

# Replace default site with our template. The stock nginx:alpine
# entrypoint runs envsubst on every file under /etc/nginx/templates/
# at container start (step 20-envsubst-on-templates.sh), substituting
# *all* env vars in scope — so defaults must be ENV'd here, not set
# by a later /docker-entrypoint.d/ script (which runs AFTER envsubst).
ENV VELOCITY_API_HOST=velocity-api:8080 \
    LISTEN_PORT=8080 \
    # Tell the stock entrypoint to populate NGINX_LOCAL_RESOLVERS from
    # /etc/resolv.conf so the template's `resolver` directive picks up
    # whatever DNS the container was given (Docker or Kubernetes).
    NGINX_ENTRYPOINT_LOCAL_RESOLVERS=1 \
    # Restrict the stock envsubst pass to these three vars. Without
    # this, envsubst would also expand nginx-runtime variables like
    # $velocity_api_host that we want nginx itself to interpret.
    NGINX_ENVSUBST_FILTER="^(VELOCITY_API_HOST|LISTEN_PORT|NGINX_LOCAL_RESOLVERS)$"
COPY nginx.conf.template /etc/nginx/templates/default.conf.template

# Static SPA payload.
COPY --from=builder /work/dist /usr/share/nginx/html

# nginx:alpine already runs as the unprivileged `nginx` user.
EXPOSE 8080

# nginx-alpine's stock entrypoint already runs scripts in
# /docker-entrypoint.d/ and then exec's nginx. Don't override.
