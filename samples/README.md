# Sample CRDs

End-to-end walk-through of a single Velocity tenant, in apply order.

The sample chain provisions **`acme / supply-chain / procurement / purchase-order:v1`** —
one organisation, one application, one domain, one schema with fields,
RLS, audit, archive, and a couple of role bindings + an API key.

Files are numbered so `kubectl apply -f samples/` (which iterates in
lexicographic order) applies them in the correct sequence. The validating
webhook resolves every cross-reference at apply time — applying out of
order will be rejected with a clear `referenced X not found` message.

| # | File | Kind | Namespace |
|---|------|------|-----------|
| 00 | `00-namespaces.yaml`         | Namespace (×4)      | — |
| 01 | `01-authstrategy.yaml`       | AuthStrategy (JWT)  | `platform` |
| 01b | `01b-authstrategy-oidc.yaml` *(alternative — see [docs/oidc-setup.md](../docs/oidc-setup.md))* | AuthStrategy (OIDC) + Secret | `platform` |
| 02 | `02-organisation.yaml`       | Organisation        | `platform` |
| 03 | `03-application.yaml`        | Application         | `acme-platform` |
| 04 | `04-domain.yaml`             | Domain              | `acme-supply-chain` |
| 05 | `05-archivepolicy.yaml`      | ArchivePolicy       | `acme-supply-chain-procurement` |
| 06 | `06-logfilterpolicy.yaml`    | LogFilterPolicy     | `acme-supply-chain-procurement` |
| 07 | `07-logroutingpolicy.yaml`   | LogRoutingPolicy    | `acme-supply-chain-procurement` |
| 08 | `08-schemadefinition.yaml`   | SchemaDefinition    | `acme-supply-chain-procurement` |
| 09 | `09-rolebinding.yaml`        | RoleBinding (×2)    | `acme-supply-chain-procurement` |
| 10 | `10-apikey.yaml`             | ApiKey              | `acme-supply-chain-procurement` |
| 11 | `11-purgerequest.yaml`       | PurgeRequest *(operational, not part of setup)* | `acme-supply-chain-procurement` |

## Apply

**Fastest path:** let `make e2e` do it for you.

```bash
VELOCITY_E2E_SAMPLES=1 make e2e
```

The full-stack flow applies every file in `samples/` (except
`11-purgerequest.yaml`) automatically after the cluster is ready, so the
portal opens onto a populated tenant hierarchy.

**Manual path:** if you want to apply step by step (e.g. to demo the
webhook rejecting a malformed Domain), run them one at a time:

kubectl apply -f samples/00-namespaces.yaml
kubectl apply -f samples/01-authstrategy.yaml
kubectl apply -f samples/02-organisation.yaml
kubectl apply -f samples/03-application.yaml
kubectl apply -f samples/04-domain.yaml
kubectl apply -f samples/05-archivepolicy.yaml
kubectl apply -f samples/06-logfilterpolicy.yaml
kubectl apply -f samples/07-logroutingpolicy.yaml
kubectl apply -f samples/08-schemadefinition.yaml
kubectl apply -f samples/09-rolebinding.yaml
kubectl apply -f samples/10-apikey.yaml
```

Or in one shot (numeric order is preserved by lexicographic sort):

```bash
kubectl apply -f samples/
```

Skip `11-purgerequest.yaml` for setup — apply it explicitly only when you
actually want to purge data:

```bash
kubectl apply -f samples/11-purgerequest.yaml
```

## Verify

After all 11 resources are applied:

```bash
# Reconciler phase for each resource
kubectl get organisations -A
kubectl get applications -A
kubectl get domains -A
kubectl get schemadefinitions -A
kubectl get archivepolicies -A
kubectl get rolebindings -A
kubectl get apikeys -A

# Or one go:
kubectl get organisations,applications,domains,schemadefinitions,authstrategies,archivepolicies,logfilterpolicies,logroutingpolicies,rolebindings,apikeys -A
```

Expected: every resource's `STATUS` (or `.status.phase`) reaches `Ready`.

In Postgres:

```bash
make psql
\dn                                                  -- schemas
\dt acme_supply_chain_procurement.*                  -- expect purchase_order_v1 + audit/history/outbox siblings
SELECT * FROM platform.schema_definitions WHERE kind='purchase-order';
```

## Retrieving the API key plaintext

`samples/10-apikey.yaml` creates an `ApiKey` resource. The operator
generates a 256-bit secret, stores **only the SHA256** in
`status.keyHash`, and writes the plaintext to a one-shot k8s `Secret`
named in `status.secretRef`. Read it once, then rotate the consumer to
the value:

```bash
kubectl -n acme-supply-chain-procurement get apikey erp-sync-key \
  -o jsonpath='{.status.secretRef}'
# → e.g. erp-sync-api-key-secret

kubectl -n acme-supply-chain-procurement get secret erp-sync-api-key-secret \
  -o jsonpath='{.data.key}' | base64 -d
# → vel_dev_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV12wx
```

The plaintext Secret is intentionally not surfaced through `kubectl get
apikey -o yaml` — it lives only in the referenced Secret, which you can
delete after copying the value into your consumer's secret store.

## Tear down

```bash
# Lower the namespaces — every CRD object inside is namespaced, so
# deleting the namespaces cascades.
kubectl delete -f samples/00-namespaces.yaml
```

The operator's finalizers will quiesce schema drops in Postgres before
the namespace is fully removed. If a finalizer is stuck, see
`runbooks/operator-stuck-reconcile.md`.

## Customising

The sample chain is single-tenant (`tenancyMode: single` on the
Organisation) and uses `jwt-internal` for everything. To exercise the
multi-tenant path (ADR-010), set `tenancyMode: multi-tenant` and add
per-org `AuthStrategy` resources in `acme-platform` instead of the
shared `platform` namespace — the webhook then rejects cross-org `ref`s
in SchemaDefinitions.
