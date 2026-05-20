# velocity-portal

React + Vite SPA for managing Velocity objects.

## Layout

- Dark theme + amber accent, monospace.
- Three-panel shell (sidebar / main / page-specific right panes).
- Routes mirror the `org/app/domain/object/version` addressing scheme.

## Backend coupling

All API calls are relative — in production `nginx` serves the SPA and proxies
`/api`, `/auth`, `/version`, `/healthz`, `/readyz` to the in-cluster
`velocity-api` Service (see `nginx.conf.template`). In dev, Vite proxies the
same paths to `$VELOCITY_API_BASE` (default `http://127.0.0.1:8080`).

CRD management views (AuthStrategy / RoleBinding / ApiKey / LogFilterPolicy /
LogRoutingPolicy / SchemaDefinition) are **create-only** in the portal: they
generate a YAML manifest with a live preview, then offer "Copy YAML" / "Copy
`velocity apply` command" buttons. Listing existing CRDs requires either
`velocity get` or `kubectl get` — there is no admin API for CRD reads today.

## Local development

```sh
cd portal
npm install
VELOCITY_API_BASE=http://localhost:8080 npm run dev
```

## Tests

```sh
npm run test
```

Vitest + jsdom. Covers the API client (fetch wrapper, error mapping, 401
event dispatch, 204 handling) and the YAML serialization shape used by
the visual editors.

## Build

```sh
npm run build         # → dist/
npm run preview       # serves dist/ on :8080 for smoke-test
```

## Container

```sh
docker build -t velocity-portal:dev portal/
docker run --rm -p 8080:8080 \
  -e VELOCITY_API_HOST=host.docker.internal:8080 \
  velocity-portal:dev
```

## Helm

Enable in the umbrella chart:

```yaml
portal:
  enabled: true
  image:
    repository: ghcr.io/<owner>/velocity-portal
    tag: v0.1.6
  config:
    default_auth_strategy: "velocity-system/portal-oidc"
    grafana_url: "https://grafana.example.com"
```

The chart mounts `config.json` from a ConfigMap so the portal binary stays
generic across deployments.
