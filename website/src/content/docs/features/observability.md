---
title: Observability
description: Metrics, traces, and SLO enforcement
---

Velocity ships with comprehensive observability: Prometheus metrics, OpenTelemetry traces, and SLO-driven alerts. All signals are low-cardinality, safe for cost-effective monitoring at scale.

## Metrics

### Label Cardinality Rules

All metrics use strictly bounded label values to prevent cardinality explosion:

| Label | Values | Rationale |
|-------|--------|-----------|
| `operation` | `create`, `read`, `update`, `delete`, `restore`, `export`, `query`, `search` | Hardcoded operations |
| `outcome` | `success`, `error`, `denied`, `validation_error`, `not_found` | Fixed set |
| `actor_type` | `human`, `service`, `operator`, `scheduler`, `anonymous` | Fixed set |
| `strategy` | `jwt`, `oidc`, `api_key`, `none`, `composite` | Auth strategies |
| `tier` | `1`, `2`, `3` | Search tiers only |
| `schema` | Bounded by SchemaDefinition.metadata.name | Schema-scoped metrics only |

Schema labels are **excluded from high-cardinality metrics** (e.g., `velocity_api_request_duration_seconds` does not include schema). Include schema only on per-schema visibility metrics (e.g., `velocity_search_queries_total{tier="3", schema="..."}`, `velocity_cdc_lag_records{schema="..."}`).

**Never include:**
- Entity IDs (e.g., PO-001)
- User emails
- API key names
- Token identifiers
- HTTP paths with user input

### Core Metrics

#### API Request Metrics

```
# Histogram: request duration seconds
velocity_api_request_duration_seconds{
  operation="create|read|update|delete|query|search",
  outcome="success|error|denied|validation_error",
  actor_type="human|service|..."
}

# Counter: total requests
velocity_api_requests_total{
  operation="...",
  outcome="...",
  actor_type="..."
}

# Gauge: active requests
velocity_api_requests_inflight{
  operation="..."
}
```

Buckets: `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10]` seconds.

#### Authentication & Authorization

```
velocity_auth_checks_total{
  strategy="jwt|oidc|api_key|composite",
  outcome="success|denied_rbac|denied_cel_abac|denied_auth_invalid|denied_rate_limit",
  fail_mode="none|redis_unavailable_denied|jwks_unavailable_cached|..."
}

velocity_auth_check_duration_seconds{
  strategy="..."
}

velocity_auth_dependency_failures_total{
  dependency="redis|jwks|database|typesense",
  fail_mode="..."
}
```

#### Audit

```
velocity_audit_events_total{
  operation="create|update|delete|restore",
  schema="..." (optional, for per-schema tracking)
}

velocity_audit_chain_tampering_detected_total
# Alert: increase(...[5m]) > 0 → critical

velocity_audit_verification_duration_seconds{
  schema="..."
}
```

#### Search

```
velocity_search_queries_total{
  tier="1|2|3",
  schema="...",
  outcome="success|error|partial"
}

velocity_search_latency_seconds{
  tier="1|2|3"
}
# Histograms: [0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1, 5, 10]

velocity_search_result_size{
  tier="1|2|3"
}

velocity_typesense_documents{
  schema="..."
}

velocity_cdc_lag_records{
  schema="..."
}
# Alert: > 1000 for > 5m → warning
```

#### Archive & Purge

```
velocity_archive_runs_total{
  schema="...",
  trigger="age|size|cel",
  status="success|error|skipped"
}

velocity_archive_records_total{
  schema="...",
  destination="s3"
}

velocity_archive_duration_seconds{
  schema="..."
}

velocity_purge_requests_total{
  schema="...",
  status="approved|rejected|pending"
}

velocity_archive_s3_bytes{
  schema="...",
  partition_month="2026-05"
}
```

#### Time Machine

```
velocity_history_queries_total{
  operation="list|point_in_time|diff|restore|replay|snapshot",
  tier="hot|warm"
}

velocity_history_query_duration_seconds{
  operation="...",
  tier="..."
}
```

#### Database

```
velocity_postgres_pool_connections{
  pool="main|operator"
}

velocity_postgres_connection_wait_duration_seconds{
  pool="..."
}

velocity_postgres_query_duration_seconds{
  operation="select|insert|update|delete|call",
  table="schema_definition_check|purchase_order_v1|..."  (only if low-cardinality table set)
}
```

#### Operator Reconciliation

```
velocity_operator_reconciliations_total{
  crd_kind="SchemaDefinition|AuthStrategy|ArchivePolicy|PurgeRequest|LogFilterPolicy",
  outcome="success|error|requeue"
}

velocity_operator_reconciliation_duration_seconds{
  crd_kind="..."
}

velocity_operator_requeue_delay_seconds{
  crd_kind="..."
}

velocity_schema_registry_size{
  # Gauge: number of schemas loaded
}

velocity_schema_registry_refresh_duration_seconds
```

#### Webhook

```
velocity_webhook_validations_total{
  crd_kind="...",
  outcome="allowed|denied"
}

velocity_webhook_validation_duration_seconds{
  crd_kind="..."
}
```

### Querying Metrics

Metrics are exported at `/metrics` in Prometheus format:

```bash
curl http://localhost:8080/metrics | grep velocity_
```

Example output:

```
# HELP velocity_api_requests_total Total API requests
# TYPE velocity_api_requests_total counter
velocity_api_requests_total{operation="create",outcome="success",actor_type="human"} 15042
velocity_api_requests_total{operation="read",outcome="success",actor_type="service"} 342123
velocity_api_requests_total{operation="update",outcome="denied_rbac",actor_type="human"} 7
```

## Traces

Every request is traced using OpenTelemetry (OTLP exporter to Jaeger/Datadog/Honeycomb).

### Trace Structure

Root span: `{schema.kind}.{operation}` (e.g., `PurchaseOrder.create`)

Attributes:

```
schema.kind                    "PurchaseOrder"
schema.path                    "acme/supply-chain/procurement/purchase-order/v1"
velocity.org                   "acme"
velocity.app                   "supply-chain"
velocity.domain                "procurement"
operation                      "create"
actor_id                       "ravi.kumar"
actor_type                     "human"
request.duration_ms            1234
outcome                        "success" | "error" | "denied"
http.status_code               201
validation_errors              (span event if validation fails)
audit_event_hash               "abc123def456..." (for disputes/forensics)
```

### Span Events

When exceptional conditions occur:

```
span.add_event(
  "validation_error",
  {
    "field": "supplier_code",
    "rule": "maxLength",
    "constraint": "100",
    "actual": 150
  }
)

span.add_event(
  "auth_failure",
  {
    "strategy": "jwt",
    "reason": "denied_rbac",
    "required_role": "procurement-writer"
  }
)

span.add_event(
  "database_error",
  {
    "operation": "insert",
    "code": "23505",  // unique violation
    "table": "acme_supply_chain_procurement.purchase_order_v1"
  }
)
```

### Trace Context Propagation

Every outgoing HTTP call (warm-reader, Typesense, webhook) includes the current span's `traceparent` header:

```
GET /api/acme/.../v1/archive/query
Traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
```

This allows complete request tracing from user → API → warm-reader → DataFusion → S3 in a single trace.

### Trace Export

Configure OTLP exporter in Helm:

```yaml
velocity:
  observability:
    traces:
      enabled: true
      exporter: otlp
      endpoint: http://otel-collector:4317  # gRPC
      samplingRate: 0.1  # Sample 10% of traces
```

## Structured Logging

All logs are emitted as single-line JSON by `tracing-subscriber`:

```json
{
  "timestamp": "2026-05-19T14:32:00Z",
  "level": "info",
  "target": "velocity_api",
  "message": "operation completed",
  "schema": "PurchaseOrder",
  "operation": "create",
  "entity_id": "PO-00000001",
  "actor": "ravi.kumar",
  "duration_ms": 45,
  "outcome": "success"
}
```

**Never log:**
- Full request/response bodies
- Entity data (PII/sensitive fields)
- API key values
- JWT tokens
- Database connection strings

**Always log:**
- Operation type, actor, schema
- Duration and outcome
- Failure reasons (error code, not error message)
- Audit event ID (for correlation with audit log)

Example error log:

```json
{
  "timestamp": "2026-05-19T14:33:00Z",
  "level": "error",
  "target": "velocity_api",
  "message": "validation failed",
  "schema": "PurchaseOrder",
  "operation": "create",
  "actor": "anita.sharma",
  "error_code": "VALIDATION_FAILED",
  "validation_errors": [
    {"field": "supplier_code", "rule": "maxLength", "constraint": "100"}
  ],
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```

## SLO & Alerting

### Defining SLOs

SLOs are defined as PrometheusRule manifests:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: velocity-slos
spec:
  groups:
    - name: velocity.rules
      interval: 30s
      rules:
        # API availability: 99.9% of requests succeed or are denied (not 5xx)
        - record: velocity:api:availability
          expr: |
            sum(rate(velocity_api_requests_total{outcome!="error"}[5m])) /
            sum(rate(velocity_api_requests_total[5m]))
        
        # Search latency p99: Tier 3 < 100ms for 99% of queries
        - record: velocity:search:latency:p99
          expr: histogram_quantile(0.99, rate(velocity_search_latency_seconds{tier="3"}[5m]))
        
        # Archive lag: < 1000 unpublished outbox records
        - record: velocity:archive:lag
          expr: count(velocity_cdc_lag_records > 100)

        # Audit chain integrity: 0 tampering detections per month
        - alert: AuditTamperingDetected
          expr: increase(velocity_audit_chain_tampering_detected_total[5m]) > 0
          for: 1m
          severity: critical
          annotations:
            summary: "Audit chain tampering detected in {{ $labels.schema }}"
            action: "Quarantine schema, contact security team immediately"

        # Redis revocation check failure: 503 responses on surge
        - alert: AuthDependencyFailure
          expr: rate(velocity_auth_dependency_failures_total{fail_mode="redis_unavailable_denied"}[5m]) > 0.01
          for: 2m
          severity: warning
          annotations:
            summary: "Redis revocation checks failing; auth requests being denied"
            action: "Check Redis status; restart if unhealthy"

        # Archive stalled: not run in 24 hours
        - alert: ArchiveStalled
          expr: time() - max(platform.archive_runs.completed_at by (schema)) > 86400
          for: 1h
          severity: warning
          annotations:
            summary: "Archive for {{ $labels.schema }} has not run in 24 hours"
            action: "Check archive worker logs; restart if hung"

        # Outbox growth unbounded
        - alert: OutboxLag
          expr: velocity_cdc_lag_records > 10000
          for: 5m
          severity: critical
          annotations:
            summary: "Outbox lag > 10K records for {{ $labels.schema }}"
            action: "Restart CDC worker; investigate Typesense availability"

        # API latency p99 regression
        - alert: ApiLatencyRegression
          expr: |
            histogram_quantile(0.99, rate(velocity_api_request_duration_seconds[5m])) > 1
          for: 5m
          severity: warning
          annotations:
            summary: "API latency p99 > 1s (regression)"
            action: "Check query complexity, pool exhaustion, schema drift"

        # Schema registry not ready
        - alert: SchemaRegistryNotReady
          expr: velocity_schema_registry_size == 0
          for: 1m
          severity: critical
          annotations:
            summary: "Schema registry has no schemas loaded (API pod may have crashed)"
            action: "Check API pod logs; restart if necessary"
```

Apply to cluster:

```bash
kubectl apply -f prometheus-rules.yaml
```

### Alerting Best Practices

1. **Alert on SLO misses, not raw thresholds.** Use SLO recording rules + alert only when SLO violated over 5m window.
2. **Tag alerts with severity.** Critical alerts → on-call paged. Warning → logged for triage.
3. **Include context in annotations.** Always provide `summary` (user-facing), `action` (runbook link or quick fix).
4. **Route to Slack/PagerDuty.** Alertmanager routes by severity:
   ```yaml
   alerting_rules:
   - alert: AuditTamperingDetected
     annotations:
       slack_channel: "#security"
       pagerduty: true
   ```

## Observability Monitoring Checklist

- [ ] Prometheus scraping `/metrics` every 30s
- [ ] Traces exported to collector (sample rate 10%)
- [ ] Logs aggregated (JSON-only) to ELK/Splunk
- [ ] SLO dashboards deployed (Grafana)
- [ ] Alert routing tested (Slack, PagerDuty)
- [ ] On-call runbooks linked to alerts
- [ ] Monthly SLO review (target 99.9% API availability)
- [ ] No high-cardinality labels escaping (audit `velocity_*` metrics)
- [ ] Trace sampling covers both success and failure paths
- [ ] Log retention: 30 days hot, 7 years warm (audit log only)

