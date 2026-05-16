# Operator stuck reconcile

**Symptom**
- A `Domain` / `Application` / `Organisation` / `SchemaDefinition` is in `status.phase=Pending` or `Failed` for > 5 min.
- `kube_runtime_reconcile_failures_total` is rising.
- The CRD's `status.conditions` carries a non-transient error.

**Severity**: ticket (page if the CRD blocks a release).

## Triage (5 min)

1. Identify the stuck object:
   ```bash
   kubectl get domains,applications,organisations,schemadefinitions \
       -A -o jsonpath='{range .items[?(@.status.phase!="Ready")]}{.kind}{"/"}{.metadata.namespace}{"/"}{.metadata.name}{"\t"}{.status.phase}{"\t"}{.status.message}{"\n"}{end}'
   ```
2. Get the latest operator logs filtered to that object:
   ```bash
   kubectl logs -n velocity-system deploy/velocity-operator --tail=500 \
       | jq 'select(.object_ref | test("<name>"))'
   ```
3. Categorise the error:
   - `permission denied for database velocity` → operator role missing `CREATE`. Grant it: `GRANT CREATE ON DATABASE velocity TO velocity_operator;` (see [`db/init/01-roles.sql`](../db/init/01-roles.sql)).
   - `BYPASSRLS violation` → operator refusing to start. Fix the role per ADR-007 and restart.
   - `kube api: 403` / `forbidden` → ClusterRole is missing a verb. Compare against [`charts/velocity/templates/rbac.yaml`](../charts/velocity/templates/rbac.yaml).
   - `connection refused` → see [postgres-failover.md](postgres-failover.md).
   - validation rejection in webhook (`Domain namespace ... must equal ...`) → fix the CRD; this is not an operator bug.

## Mitigation

- **Transient / network**: nothing to do — controller backoff will retry. Confirm with `kubectl events`.
- **Schema not provisioned**: re-trigger reconcile by adding a label:
  ```bash
  kubectl annotate domain/<name> -n <ns> velocity.sh/touched="$(date -u +%s)" --overwrite
  ```
  (The controller watches all events, so any change re-queues it.)
- **Operator wedged on a single object**: increase verbosity and restart:
  ```bash
  kubectl set env deploy/velocity-operator -n velocity-system RUST_LOG=debug,velocity_operator=trace
  kubectl rollout restart deploy/velocity-operator -n velocity-system
  ```
- **Domain provisioning partially failed** (schema exists but roles missing): the reconciler is idempotent — let it retry. If `CREATE ROLE` was the failing step, check the operator has `CREATEROLE` (it does by default per `db/init/01-roles.sql`).

## Root cause

- Capture the operator's full log window for the failed reconcile.
- `kubectl get <kind>/<name> -o yaml` immediately after recovery to compare `status` and `metadata.annotations`.
- Note which downstream object failed (Postgres schema? k8s Namespace? Status patch?). Each has a different remediation path.

## Postmortem

Link the incident doc here.
