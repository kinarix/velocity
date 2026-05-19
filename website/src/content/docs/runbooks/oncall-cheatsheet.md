---
title: On-Call Cheatsheet
description: Essential information and commands for incident response
---

Quick reference for on-call engineers responding to Velocity incidents.

## Immediate Triage (First 5 Minutes)

When paged, establish context:

### 1. What's broken?

```bash
# Check all components
velocity status

# Expected output:
# API Server: Ready
# Operator: Ready
# Database: Ready
# Webhooks: Ready
```

If any component is degraded, continue to health checks below.

### 2. Get recent logs

```bash
# API logs (last 100 lines)
kubectl logs -n velocity-system -l app=velocity-api --tail=100 | tail -20

# Operator logs
kubectl logs -n velocity-system -l app=velocity-operator --tail=50

# Search for ERROR
kubectl logs -n velocity-system --since=10m -l app=velocity-api | grep -i error
```

### 3. Check metrics dashboard

```bash
# Port-forward Prometheus
kubectl port-forward -n monitoring svc/prometheus 9090:9090 &
# Open http://localhost:9090

# Or query directly:
curl -s http://prometheus.monitoring:9090/api/v1/query?query=velocity_api_requests_total | jq .
```

### 4. Identify affected schema (if specific to one)

```bash
# If incident alert includes schema name:
kubectl get sd {schema-name} -n {namespace}

# Check status and conditions:
kubectl describe sd {schema-name} -n {namespace}
```

## Health Checks

### API Server

```bash
# Is API responding?
curl -s http://localhost:8080/healthz
# Expected: 200 OK

# Is registry ready?
curl -s http://localhost:8080/readyz
# Expected: 200 OK (all schemas loaded)

# Check pod status
kubectl get pods -n velocity-system -l app=velocity-api
# All pods should be Ready 3/3
```

### Database (Postgres)

```bash
# Are connections healthy?
kubectl exec velocity-1 -n velocity-system -- psql -U velocity_admin -c \
  "SELECT datname, count(*) FROM pg_stat_activity GROUP BY datname;"

# Is primary elected?
kubectl get cluster velocity -n velocity-system -o json | jq '.status.primaryInstance'
# Should return a pod name (e.g., "velocity-1")

# Check replication lag
kubectl exec velocity-1 -n velocity-system -- psql -U velocity_admin -c \
  "SELECT slot_name, active, restart_lsn FROM pg_replication_slots;"
# lag_bytes should be < 1 MB
```

### Operator

```bash
# Is operator running?
kubectl get pods -n velocity-system -l app=velocity-operator
# Should be 1 Ready 1/1

# Is it actively reconciling?
kubectl logs -n velocity-system -l app=velocity-operator --tail=50 | grep -i reconcil
```

### Redis (Revocation Check)

```bash
# Is Redis available?
kubectl get pod -n redis
# Should be running

# Can we connect?
kubectl exec -it redis-0 -n redis -- redis-cli ping
# Expected: PONG

# Check key memory usage
kubectl exec -it redis-0 -n redis -- redis-cli INFO memory | grep used_memory_human
```

### Typesense (Tier-3 Search)

```bash
# Is Typesense running?
kubectl get pod -n typesense
# Should be 1 Ready

# Check collection health
kubectl exec typesense-0 -n typesense -- curl -s localhost:8108/collections | jq '.[].name'
```

## Common Issues & Quick Fixes

### API returning 503 REVOCATION_UNAVAILABLE

```bash
# Redis is down or unreachable
kubectl get pod -n redis

# Restart Redis
kubectl rollout restart statefulset redis -n redis

# Wait for recovery
kubectl wait --for=condition=ready pod redis-0 -n redis --timeout=5m

# Test
curl https://api.velocity.acme.com/healthz
```

### API returning 401 Invalid Bearer Token

```bash
# JWKS endpoint is down OR token is malformed/expired
# Check if this is widespread or isolated

# Get a valid token
TOKEN=$(curl -s https://auth.acme.com/token \
  -d "client_id=$CLIENT_ID&client_secret=$SECRET" | jq -r .access_token)

# Test with new token
curl -H "Authorization: Bearer $TOKEN" \
  https://api.velocity.acme.com/healthz

# If new token works, old tokens are just expired
# If new token fails, JWKS is unreachable (check network/DNS)
```

### Reconciler Hot-Loop

```bash
# Operator keeps reconciling the same schema infinitely
# Check the schema status for errors:
kubectl describe sd {schema-name} -n {namespace}

# Look for "last reconcile error" in conditions

# If error is "field validation failed", check CRD YAML:
kubectl get sd {schema-name} -n {namespace} -o yaml | head -30

# Fix the CRD error and re-apply:
kubectl apply -f fixed-schema.yaml

# Or force a clean reconcile:
kubectl delete sd {schema-name} -n {namespace}
kubectl apply -f fixed-schema.yaml
```

### Outbox Unbounded Growth

```bash
# CDC worker crashed, outbox is not draining
# Check worker logs:
kubectl logs -n velocity-system -l app=velocity-archive-worker --tail=50

# Check outbox lag
kubectl exec velocity-1 -n velocity-system -- psql -U velocity_admin -c \
  "SELECT COUNT(*) as unpublished FROM acme_supply_chain_procurement.purchase_order_v1_outbox WHERE published_at IS NULL;"

# If > 10K, restart CDC worker:
kubectl rollout restart deployment velocity-api -n velocity-system

# Verify recovery
sleep 30 && \
kubectl exec velocity-1 -n velocity-system -- psql -U velocity_admin -c \
  "SELECT COUNT(*) as unpublished FROM acme_supply_chain_procurement.purchase_order_v1_outbox WHERE published_at IS NULL;"
# Should decrease rapidly
```

### Schema Apply Fails with "Namespace Mismatch"

```bash
# Webhook rejects schema because namespace doesn't match org-app-domain pattern
# Expected format: {org}-{app}-{domain}

# Check your namespace
kubectl get schema -n {current_namespace}
# Expected: namespace = acme-supply-chain-procurement

# If mismatch, create schema in correct namespace:
kubectl apply -f schema.yaml -n acme-supply-chain-procurement
```

## Trace an Incident

When incident is ongoing, gather evidence:

### Get Request Trace ID

```bash
# From API logs
kubectl logs -n velocity-system -l app=velocity-api --since=5m | grep "trace_id" | head -1
# Output: "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
```

### Fetch Full Trace

```bash
# Query Jaeger
curl -s "http://jaeger.monitoring:16686/api/traces?service=velocity-api&traceID=4bf92f3577b34da6a3ce929d0e0e4736" | jq .
```

### Check Audit Log

```bash
# Find all operations by affected actor
velocity audit list --actor ravi.kumar --since 10m

# Find specific entity
velocity audit list --schema acme/supply-chain/procurement/purchase-order/v1 --entity-id PO-001

# Verify chain integrity
velocity audit verify --schema acme/supply-chain/procurement/purchase-order/v1 --entity-id PO-001
```

### Correlate with Metrics

```bash
# Port-forward to Prometheus and query during incident window
kubectl port-forward -n monitoring svc/prometheus 9090:9090 &

# Query API errors spike
# Expression: rate(velocity_api_requests_total{outcome="error"}[1m])

# Query auth failures
# Expression: rate(velocity_auth_checks_total{outcome!="success"}[1m])

# Query search latency p99
# Expression: histogram_quantile(0.99, rate(velocity_search_latency_seconds[5m]))
```

## Escalation Path

1. **Self-healing (5 min):** Try health checks and quick fixes above
2. **Page on-call backup (10 min):** If issue persists, page backup
3. **Escalate to team lead (15 min):** If no improvement
4. **Major incident (20 min):** Page all-hands, open war room

## Post-Incident (After Fix)

- [ ] Note exact time incident started and ended
- [ ] Collect logs: `kubectl logs -n velocity-system --since=1h > /tmp/logs.txt`
- [ ] Take screenshot of metrics at incident time
- [ ] File ticket with timeline
- [ ] Link to Slack conversation
- [ ] Schedule post-mortem if critical (SEV-1)

## Useful Aliases

```bash
# Add to ~/.zshrc or ~/.bashrc

alias vlog='kubectl logs -n velocity-system -l app=velocity-api --tail=100'
alias vop='kubectl logs -n velocity-system -l app=velocity-operator --tail=50'
alias vstat='velocity status'
alias vsc='kubectl get sd -A'
alias vhealth='curl -s http://localhost:8080/healthz && echo OK'

# Quick exec into Postgres
alias vpg='kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity'
```

## Quick Contacts

- **API/Platform Team:** #velocity Slack
- **Database Team:** #database Slack
- **Security Team:** #security Slack
- **Page on-call:** /page-oncall (Slack command)

