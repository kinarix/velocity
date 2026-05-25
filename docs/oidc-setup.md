# Configuring an OIDC Provider

Velocity supports the OIDC authorization-code flow as an `AuthStrategy`
kind. Once configured, the API server itself drives the redirect dance —
users hit `/auth/login/{namespace}/{strategy-name}`, get bounced to the
IdP, and come back with a session cookie. The strategy is
namespace-scoped like every other CRD; SchemaDefinitions reference it
via `strategyRef`.

This guide is **IdP-agnostic**. Worked recipes for specific providers
(Keycloak, Auth0, Okta, Google, Azure AD) are in §6 — pick the one that
matches what you have, but the rest of the page applies to all of them.

The sample at [`samples/01b-authstrategy-oidc.yaml`](../samples/01b-authstrategy-oidc.yaml)
is a working template with placeholder URLs. The Rust types are in
`crates/velocity-types/src/crds/auth.rs`.

---

## 1. Required pieces

| Piece | Where | Purpose |
|---|---|---|
| `AuthStrategy` (`type: oidc`) | Velocity CRD | Declares endpoints, client_id, JWKS URL, claim mapping |
| k8s `Secret` | Same namespace as the strategy | Holds the OAuth2 client secret; referenced by `clientSecretRef` |
| Env var on the API pod | `VELOCITY_API_OIDC_CLIENT_SECRET_<NS>_<NAME>` | The plaintext the API reads at startup. Operator injects this from the Secret in-cluster; for local out-of-cluster runs you export it yourself. |
| IdP client registration | At the IdP | client_id, client_secret, redirect_uri, PKCE-S256 enabled |

---

## 2. Shape of the CRD

```yaml
apiVersion: velocity.sh/v1
kind: AuthStrategy
metadata:
  name: oidc-default              # any name — referenced by SchemaDefinitions
  namespace: platform
spec:
  type: oidc
  config:
    # ID-token verification (signature + iss/aud)
    issuers:
      - issuer:   "https://idp.example.com"
        jwksUrl:  "https://idp.example.com/.well-known/jwks.json"
        audience: "velocity-api"
        claims:
          actorId: "$.sub"
          email:    "$.email"
          roles:
            path: "$.roles"        # see §6 for IdP-specific paths
            transform: static_append
            values: [authenticated]
          attributes:
            store_id: "$.store_id"

    # OIDC redirect-flow specifics
    oidc:
      authorizationEndpoint: "https://idp.example.com/oauth2/authorize"
      tokenEndpoint:         "https://idp.example.com/oauth2/token"
      userinfoEndpoint:      "https://idp.example.com/oauth2/userinfo"
      clientId:              "velocity-api"
      clientSecretRef:
        name: oidc-client-secret
        key:  client_secret
      redirectUri:           "https://velocity.example.com/auth/callback/platform/oidc-default"
      scopes:                [openid, profile, email]
      issuer:                "https://idp.example.com"
      sessionTtl:            28800

    revocation:
      backend:  redis
      failOpen: false
```

### Pinned endpoints vs. `configUrl` (OIDC discovery)

Velocity supports two ways to populate the OIDC endpoint fields:

**Pinned (the example above).** You paste `authorizationEndpoint`,
`tokenEndpoint`, `userinfoEndpoint`, `issuer`, and `jwksUrl` into the
CRD. The API server never contacts the IdP's discovery doc at runtime,
so a compromised `.well-known/openid-configuration` cannot move the
redirect target between applies.

**Discovery (`configUrl`).** You set `spec.config.oidc.configUrl` to the
IdP's `.well-known/openid-configuration` URL and leave the endpoint
fields unset. The API server fetches the doc **once** when the
`AuthStrategy` is loaded into its in-memory registry (a Kubernetes
apply/init event), copies the values into the resolved strategy, and
never refetches until the CRD is re-applied. Functionally the endpoints
are still pinned — the difference is who pinned them: a human reading
the discovery doc by hand, or the operator on first sight of the CRD.

The trade-off is one HTTP round-trip per apply against not having to
copy/paste data the IdP already publishes. If discovery is unreachable
when the strategy lands, the strategy is **not** registered (fail-closed)
and the operator retries on the next informer event. Explicit fields in
the CRD always win — you can keep `configUrl` set for self-documentation
purposes and still override a specific endpoint.

Look the discovery doc up in either mode with:

```bash
curl -s https://idp.example.com/.well-known/openid-configuration \
  | jq '{authorization_endpoint, token_endpoint, userinfo_endpoint, issuer, jwks_uri}'
```

The pinned form pastes those into the CRD. The discovery form references
the URL itself — see `samples/01c-authstrategy-oidc-discovery.yaml` for
the minimal shape.

What discovery **does not** populate: `clientId`, `clientSecretRef`,
`redirectUri`, `scopes`, `issuers[].audience`, and `issuers[].claims`.
Those are registration data and policy decisions specific to your
deployment; the IdP has no way to tell us about them.

### Why `clientSecretRef` instead of an inline value

The CRD lives in etcd as plaintext. Velocity uses a Kubernetes `Secret`
in the same namespace; the operator reads it during reconcile and
injects the value into the API pod's env block. For out-of-cluster
local runs, you set the env var yourself (see §5).

### The `issuer` field appears twice intentionally

`issuers[].issuer` is the value the API expects in the `iss` claim of
the ID token (signature verification step). `oidc.issuer` is the value
the API stamps on outbound `/authorize` requests and uses to select
which `issuers[]` entry to verify against. They must be equal, but the
type system carries both so a future `composite` strategy can mix
multiple IdPs cleanly.

---

## 3. Register the client at your IdP

Every IdP's UI is different but the inputs Velocity needs are universal:

| Field | Value |
|---|---|
| Client type | Confidential (server-side) |
| Grant type | Authorization Code |
| Response type | `code` |
| PKCE | **Required (S256)** — Velocity always sends it |
| Redirect URI | `https://<your-api-host>/auth/callback/<namespace>/<strategy-name>` — exact match |
| Allowed scopes | `openid profile email` (plus any custom scopes for attribute claims) |
| Token endpoint auth | `client_secret_post` or `client_secret_basic` |

Copy the issued `client_id` and `client_secret` — you'll paste them
into the CRD and the Secret, respectively.

---

## 4. Apply the AuthStrategy + Secret

```bash
# Store the client secret in a k8s Secret first (NOT in the CRD):
kubectl create secret generic oidc-client-secret \
  -n platform \
  --from-literal=client_secret='<paste here>' \
  --dry-run=client -o yaml | kubectl apply -f -

# Then the strategy:
kubectl apply -f samples/01b-authstrategy-oidc.yaml
```

---

## 5. Wire the env var into the API server

The operator does this automatically when running in-cluster: it reads
the Secret named in `clientSecretRef`, derives the env-var name from
the strategy's namespace + name, and adds it to the API Deployment.

For out-of-cluster `cargo run` (the dev path), export it yourself —
the variable name is derived from the namespace and strategy name:

```bash
# Format: VELOCITY_API_OIDC_CLIENT_SECRET_<NS>_<NAME>
#   • uppercase
#   • hyphens → underscores
#   • dots    → underscores
# For an AuthStrategy named "oidc-default" in namespace "platform":
export VELOCITY_API_OIDC_CLIENT_SECRET_PLATFORM_OIDC_DEFAULT='<same value as the Secret>'

cargo run --bin velocity-platform-api
```

If you forget, the callback returns 500 with `oidc client secret not
found in env`. The exact env-var name resolution lives in
`crates/velocity-core/src/auth_handlers.rs` (search for
`VELOCITY_API_OIDC_CLIENT_SECRET_`).

---

## 6. IdP-specific recipes

Endpoints and the roles-claim path are the two things that differ
between IdPs. Everything else (PKCE, scopes, redirect_uri shape) is
universal.

| IdP | `authorizationEndpoint` | `tokenEndpoint` | Roles claim path |
|---|---|---|---|
| Keycloak | `/realms/<realm>/protocol/openid-connect/auth` | `/realms/<realm>/protocol/openid-connect/token` | `$.realm_access.roles` |
| Auth0 | `https://<tenant>.auth0.com/authorize` | `https://<tenant>.auth0.com/oauth/token` | `$.permissions` (RBAC enabled) or via custom rule |
| Okta | `https://<org>.okta.com/oauth2/<authServerId>/v1/authorize` | `https://<org>.okta.com/oauth2/<authServerId>/v1/token` | `$.groups` (configure in app's claims) |
| Google | `https://accounts.google.com/o/oauth2/v2/auth` | `https://oauth2.googleapis.com/token` | No groups in ID token — query Workspace Directory API and map into attributes |
| Azure AD | `/<tenant>/oauth2/v2.0/authorize` | `/<tenant>/oauth2/v2.0/token` | `$.roles` (App roles) or `$.groups` (group IDs only — name lookup via MS Graph) |

### Local development against a self-hosted IdP

For local end-to-end testing without depending on a SaaS IdP, the
simplest option is to run **Keycloak** (or **Dex**, or **Authentik**)
alongside the rest of the docker-compose stack:

```yaml
# docker-compose.override.yml (gitignored)
services:
  keycloak:
    image: quay.io/keycloak/keycloak:24.0
    command: ["start-dev", "--http-port=8080"]
    environment:
      KEYCLOAK_ADMIN: admin
      KEYCLOAK_ADMIN_PASSWORD: admin
    ports:
      - "8088:8080"
```

`docker compose up -d keycloak`, visit `http://localhost:8088`, create a
realm + client + test user, then point the AuthStrategy at it. Dex and
Authentik follow the same pattern; substitute their endpoint paths.

---

## 7. Multiple IdPs

Apply multiple `AuthStrategy` resources (one per IdP) and reference the
right one from each `SchemaDefinition.auth.strategyRef`.

Or set a `composite` strategy whose `children: []` lists them in order
— the middleware picks based on which credential scheme is present on
the request (Bearer JWT, OIDC session cookie, API key). It does **not**
fall through after a verification failure; that's deliberate, to
prevent attackers from playing two strategies' error oracles against
each other (see `crates/velocity-types/src/crds/auth.rs`).

---

## 8. Common failure modes

| Symptom | Cause |
|---|---|
| `invalid redirect_uri` from IdP | IdP's registered redirect URI doesn't EXACTLY match `oidc.redirectUri`. Trailing slash, scheme, port — all must match. |
| 401 on callback, `invalid_token` | Clock skew between API host and IdP > `clockSkew` (default 30 s). Sync NTP or raise the limit. |
| 401 on callback, `iss mismatch` | `issuers[].issuer` ≠ the `iss` claim the IdP issues. Decode a sample token at jwt.io and compare exactly — most mismatches are trailing slashes or HTTP vs HTTPS. |
| 500 on callback, `client secret not found` | `VELOCITY_API_OIDC_CLIENT_SECRET_<NS>_<NAME>` env var unset (out-of-cluster) or operator hasn't reconciled the Secret yet (in-cluster). |
| Roles never appear on the identity | `claims.roles.path` JSONPath doesn't match what your IdP puts in the ID token. Decode the token at jwt.io and check. |
| User authenticated but RBAC denies everything | Claim mapping worked but no `RoleBinding` exists for this `actorId`. Apply one — see `samples/09-rolebinding.yaml`. |
| `PKCE required` | Your IdP has PKCE disabled for confidential clients. Re-enable it — Velocity always sends `code_challenge`. |
