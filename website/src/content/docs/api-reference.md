---
title: API Reference
description: Complete REST API endpoint documentation
---

Velocity exposes a generic REST API built from your SchemaDefinitions. Every endpoint follows the same conventions for request/response shape, error handling, and pagination.

## Base URL

```
https://api.velocity.acme.com/api
```

All paths are relative to this base. Organization is configured at deploy time.

## Authentication

All requests require a valid bearer token. Supported strategies:
- **JWT** (most common): obtained from your identity provider
- **API key**: generated via `velocity api-key create`
- **OIDC session**: cookie-based after `/auth/callback`

```bash
curl -H "Authorization: Bearer <token>" \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1
```

## Response Shape

### Success Response (2xx)

```json
{
  "data": { /* object or array */ },
  "status": "success",
  "requestId": "req-abc123"
}
```

### Error Response (4xx, 5xx)

```json
{
  "code": "SCHEMA_NOT_FOUND",
  "message": "Schema does not exist",
  "requestId": "req-abc123",
  "details": {
    "schema_path": "acme/supply-chain/procurement/purchase-order/v1"
  }
}
```

## Common Headers

### Request

- `Authorization: Bearer <token>` (required)
- `Content-Type: application/json` (for POST/PATCH)
- `Idempotency-Key: <uuid>` (optional; enables idempotent writes)
- `X-Request-ID: <id>` (optional; propagates for tracing)

### Response

- `X-Request-ID: <id>` (echoes or generates)
- `X-RateLimit-Remaining: <count>`
- `X-RateLimit-Reset: <unix-timestamp>`

## Endpoint Groups

### Platform

These are fixed endpoints that don't depend on schema definitions.

#### GET /version

Retrieve client and server build information.

```bash
curl https://api.velocity.acme.com/api/version
```

Response:

```json
{
  "data": {
    "client_version": "0.9.0",
    "server_version": "0.9.0",
    "build_date": "2026-05-19T00:00:00Z",
    "git_commit": "abc123def456"
  }
}
```

#### GET /api

List all available schemas in the platform.

```bash
curl -H "Authorization: Bearer <token>" \
  https://api.velocity.acme.com/api/api
```

Response:

```json
{
  "data": [
    {
      "path": "acme/supply-chain/procurement/purchase-order/v1",
      "displayName": "Purchase Order",
      "description": "Supplier purchase orders",
      "fields": 6,
      "hasTimeMachine": true,
      "searchTier": 2
    }
  ]
}
```

#### GET /healthz

Server readiness check. Used by load balancers.

```bash
curl https://api.velocity.acme.com/api/healthz
```

Response: `200 OK` (ready) or `503 Service Unavailable` (not ready).

#### GET /readyz

Kubernetes readiness gate. Returns 200 only after the schema informer has synced.

```bash
curl https://api.velocity.acme.com/api/readyz
```

#### GET /metrics

Prometheus metrics in exposition format.

```bash
curl https://api.velocity.acme.com/api/metrics | grep velocity_operations_total
```

### CRUD Endpoints

#### POST /{path}

Create a record.

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: create-po-1" \
  -d '{
    "id": "PO-00000001",
    "supplier_code": "TATA001",
    "amount": 50000.00,
    "status": "draft",
    "notes": "Office supplies"
  }'
```

Response (201 Created):

```json
{
  "data": {
    "id": "PO-00000001",
    "supplier_code": "TATA001",
    "amount": 50000.00,
    "status": "draft",
    "notes": "Office supplies",
    "created_at": "2026-05-19T14:32:00Z",
    "created_by": "ravi.kumar",
    "version": 1
  }
}
```

**Status codes:**
- `201 Created` — Record created
- `400 Bad Request` — Validation failed
- `409 Conflict` — Unique constraint violation
- `403 Forbidden` — Insufficient access
- `503 Service Unavailable` — Postgres or Redis unavailable

#### GET /{path}

List records with optional filtering, sorting, and pagination.

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'filter[status]=approved' \
  --data-urlencode 'sort=-amount' \
  --data-urlencode 'limit=10' \
  --data-urlencode 'offset=0'
```

Query parameters:

| Parameter | Type | Example | Note |
|-----------|------|---------|------|
| `filter[field]` | string | `approved` | Equality filter. Multiple allowed. |
| `filter[field][op]` | string | `gt,lt,gte,lte,ne,in,exists` | Operator. Default is `eq`. |
| `sort` | string | `-amount,+supplier_code` | Prefix `-` for DESC. |
| `limit` | int | `50` | Max results. Default 100. |
| `offset` | int | `50` | Skip N records (cursor pagination preferred). |
| `cursor` | string | `abc123` | Opaque cursor from previous response. |
| `include` | string | `supplier` | Join related schema (deferred). |

Response (200 OK):

```json
{
  "data": [
    { "id": "PO-00000001", "amount": 75000, "status": "approved", ... },
    { "id": "PO-00000002", "amount": 60000, "status": "approved", ... }
  ],
  "pagination": {
    "limit": 10,
    "offset": 0,
    "total": 542,
    "cursor": "next-cursor-token"
  }
}
```

#### GET /{path}/{id}

Get a single record by ID.

```bash
curl https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001 \
  -H "Authorization: Bearer <token>"
```

Response (200 OK):

```json
{
  "data": {
    "id": "PO-00000001",
    "supplier_code": "TATA001",
    "amount": 50000.00,
    "status": "draft",
    "created_at": "2026-05-19T14:32:00Z",
    "version": 1
  }
}
```

**Status codes:**
- `200 OK` — Record found
- `404 Not Found` — Record does not exist
- `403 Forbidden` — No read access

#### PATCH /{path}/{id}

Update a record (optimistic locking).

```bash
curl -X PATCH https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001 \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "status": "approved",
    "version": 1
  }'
```

Response (200 OK):

```json
{
  "data": {
    "id": "PO-00000001",
    "status": "approved",
    "version": 2,
    "updated_at": "2026-05-19T14:35:00Z",
    "updated_by": "anita.sharma"
  }
}
```

**Status codes:**
- `200 OK` — Updated
- `409 Conflict` — Version mismatch (optimistic lock failed)
- `400 Bad Request` — Validation failed
- `404 Not Found` — Record does not exist
- `403 Forbidden` — No write access

#### DELETE /{path}/{id}

Soft-delete a record (sets `deleted_at` without removing data).

```bash
curl -X DELETE https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001 \
  -H "Authorization: Bearer <token>"
```

Response (204 No Content).

**Status codes:**
- `204 No Content` — Deleted
- `404 Not Found` — Already deleted or doesn't exist

### Query DSL

#### POST /{path}/query

Execute a query with filtering, sorting, joining, and pagination.

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/query \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "where": {
      "and": [
        { "field": "status", "op": "eq", "value": "approved" },
        { "field": "amount", "op": "gte", "value": 50000 }
      ]
    },
    "select": ["id", "supplier_code", "amount"],
    "sort": [
      { "field": "amount", "direction": "desc" }
    ],
    "limit": 50
  }'
```

Response (200 OK):

```json
{
  "data": [
    { "id": "PO-00000001", "supplier_code": "TATA001", "amount": 75000 },
    { "id": "PO-00000002", "supplier_code": "ACC001", "amount": 60000 }
  ]
}
```

### Search

#### POST /{path}/search (Tier 3 only)

Full-text search against Typesense.

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/search \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "q": "TATA",
    "filter_by": "status:approved",
    "sort_by": "_text_match:desc,amount:desc",
    "limit": 20
  }'
```

Response (200 OK):

```json
{
  "data": [
    {
      "id": "PO-00000001",
      "supplier_code": "TATA001",
      "amount": 50000,
      "_text_match": 45
    }
  ],
  "search_stats": {
    "total_hits": 142,
    "took_ms": 12
  }
}
```

#### GET /{org}/search (Cross-schema search)

Search across all schemas in an organization (Phase 9).

```bash
curl -G https://api.velocity.acme.com/api/acme/search \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'q=TATA'
```

Response:

```json
{
  "data": [
    {
      "schema": "supply-chain/procurement/purchase-order/v1",
      "id": "PO-00000001",
      "fields": { "supplier_code": "TATA001", "amount": 50000 }
    },
    {
      "schema": "sourcing/supplier/v1",
      "id": "TATA001",
      "fields": { "name": "Tata Steel", "country": "India" }
    }
  ]
}
```

### Time Machine

#### GET /{path}/{id}/history

List all changes to a record.

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/history \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'limit=50' \
  --data-urlencode 'offset=0'
```

Response (200 OK):

```json
{
  "data": [
    {
      "event_id": 1,
      "timestamp": "2026-05-19T14:32:00Z",
      "actor": "ravi.kumar",
      "operation": "CREATE",
      "old_value": null,
      "new_value": { "id": "PO-00000001", "status": "draft", ... }
    },
    {
      "event_id": 2,
      "timestamp": "2026-05-19T14:35:00Z",
      "actor": "anita.sharma",
      "operation": "UPDATE",
      "old_value": { "status": "draft" },
      "new_value": { "status": "approved" }
    }
  ]
}
```

#### GET /{path}/{id}/history?at=<timestamp>

Get record state at a specific point in time.

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/history \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'at=2026-05-19T14:33:00Z'
```

Response (200 OK):

```json
{
  "data": {
    "id": "PO-00000001",
    "status": "draft",
    "created_at": "2026-05-19T14:32:00Z",
    "version": 1
  }
}
```

#### GET /{path}/{id}/diff

Diff a record between two points in time.

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/diff \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'from=2026-05-19T14:32:00Z' \
  --data-urlencode 'to=2026-05-19T14:35:00Z'
```

Response (200 OK):

```json
{
  "data": {
    "added": { "approval_date": "2026-05-19T14:35:00Z" },
    "changed": { "status": { "from": "draft", "to": "approved" } },
    "removed": {}
  }
}
```

#### POST /{path}/{id}/restore

Restore a record to its state at a past instant.

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/restore \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "at": "2026-05-19T14:33:00Z",
    "reason": "Incorrect approval; reverting per stakeholder request"
  }'
```

Response (201 Created):

```json
{
  "data": {
    "id": "PO-00000001",
    "status": "draft",
    "version": 3,
    "restored_at": "2026-05-19T14:40:00Z"
  }
}
```

#### GET /{path}/{id}/replay

Stream all events for a record (Server-Sent Events).

```bash
curl -N https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/replay \
  -H "Authorization: Bearer <token>"
```

Response (text/event-stream):

```
data: {"event_id": 1, "operation": "CREATE", ...}
data: {"event_id": 2, "operation": "UPDATE", ...}
```

#### POST /{path}/history/snapshot

Take a point-in-time snapshot of all records.

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/history/snapshot \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "at": "2026-05-19T14:00:00Z",
    "reason": "Monthly snapshot for compliance"
  }'
```

Response (202 Accepted):

```json
{
  "data": {
    "snapshot_id": "snap-abc123",
    "schema": "acme/supply-chain/procurement/purchase-order/v1",
    "timestamp": "2026-05-19T14:00:00Z",
    "status": "in_progress"
  }
}
```

### Archive (Phase 8+)

#### GET /{path}/{id}/archive

Get archived version of a record.

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/archive \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'version=42'
```

Response (200 OK):

```json
{
  "data": {
    "id": "PO-00000001",
    "supplier_code": "TATA001",
    "amount": 50000,
    "archived_at": "2026-05-10T00:00:00Z"
  }
}
```

#### POST /{path}/archive/query

Query archived records (S3 Parquet via warm-reader).

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/archive/query \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "where": { "field": "status", "op": "eq", "value": "shipped" },
    "select": ["id", "amount"],
    "limit": 100
  }'
```

#### POST /{path}/{id}/unarchive

Restore a record from archive back to hot storage.

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-00000001/unarchive \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{ "reason": "Customer dispute requires record review" }'
```

### Audit

#### POST /platform/audit/verify

Verify the integrity of an audit chain.

```bash
curl -X POST https://api.velocity.acme.com/api/platform/audit/verify \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "schema": "acme/supply-chain/procurement/purchase-order/v1",
    "id": "PO-00000001"
  }'
```

Response (200 OK):

```json
{
  "data": {
    "valid": true,
    "events": 42,
    "tampering_detected": 0
  }
}
```

#### GET /platform/audit

Query the audit log (by schema, actor, or date).

```bash
curl -G https://api.velocity.acme.com/api/platform/audit \
  -H "Authorization: Bearer <token>" \
  --data-urlencode 'schema=acme/supply-chain/procurement/purchase-order/v1' \
  --data-urlencode 'limit=50'
```

Response:

```json
{
  "data": [
    {
      "event_id": 1,
      "timestamp": "2026-05-19T14:32:00Z",
      "actor": "ravi.kumar",
      "schema": "acme/supply-chain/procurement/purchase-order/v1",
      "entity_id": "PO-00000001",
      "operation": "CREATE",
      "old_value": null,
      "new_value": { ... }
    }
  ]
}
```

### Auth

#### POST /auth/callback (OIDC)

OAuth2 callback endpoint. Automatically handled by the CLI and browsers.

#### POST /auth/logout

Revoke the current session (cookie-based auth).

```bash
curl -X POST https://api.velocity.acme.com/api/auth/logout \
  -H "Authorization: Bearer <token>"
```

Response: `204 No Content`.

## Rate Limiting

Rate limiting is per actor, not per IP. Limits:

- **Default:** 1000 requests per minute
- **Search:** 100 requests per minute
- **Archive query:** 50 requests per minute

Exceeded limits return `429 Too Many Requests` with headers:

```
X-RateLimit-Remaining: 0
X-RateLimit-Reset: 1663599120
```

## Error Codes

| Code | HTTP | Meaning |
|------|------|---------|
| `SCHEMA_NOT_FOUND` | 404 | Schema path does not exist |
| `ENTITY_NOT_FOUND` | 404 | Record not found |
| `VALIDATION_ERROR` | 400 | Field validation failed |
| `UNIQUE_CONSTRAINT_VIOLATION` | 409 | Unique field value already exists |
| `OPTIMISTIC_LOCK_FAILED` | 409 | Version mismatch on update |
| `IDEMPOTENCY_CONFLICT` | 422 | Request hash differs from stored |
| `AUTH_INVALID_TOKEN` | 401 | Token expired or invalid |
| `AUTH_INSUFFICIENT_SCOPES` | 401 | Token missing required scope |
| `RBAC_DENIED` | 403 | No permission for this operation |
| `RLS_POLICY_DENIED` | 403 | Row-level security rejected request |
| `CROSS_SCHEMA_ACCESS_DENIED` | 403 | No access to joined schema |
| `REVOCATION_UNAVAILABLE` | 503 | Redis down (revocation check failed) |
| `POSTGRES_UNAVAILABLE` | 503 | Database unavailable |
| `TYPESENSE_UNAVAILABLE` | 503 | Search service unavailable |
| `INTERNAL_ERROR` | 500 | Unexpected server error |

## Pagination

Two methods are supported:

### Offset/Limit (simple, but slow for large datasets)

```bash
curl -G https://api.velocity.acme.com/api/.../records \
  --data-urlencode 'limit=100' \
  --data-urlencode 'offset=200'
```

### Cursor (fast, required for queries returning > 1000 results)

```bash
curl -G https://api.velocity.acme.com/api/.../records \
  --data-urlencode 'limit=100' \
  --data-urlencode 'cursor=abc123'
```

Response includes `cursor` in pagination:

```json
{
  "pagination": {
    "limit": 100,
    "cursor": "next-cursor-token",
    "has_more": true
  }
}
```

Pass the cursor in the next request to continue.

## Examples

### Create a record, then query it

```bash
# Create
TOKEN=$(cat ~/.velocity/token)

curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"PO-001","status":"draft","amount":1000}'

# Query
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'filter[status]=draft'
```

### Idempotent create

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: create-po-2026-05-19-001" \
  -d '{"id":"PO-001",...}'

# If this request fails, retry with the same Idempotency-Key
# and get the cached response (no duplicate created)
```

### Time-machine restore

```bash
# Find when the record was in a good state
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN"

# Restore to that moment
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/restore \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "at": "2026-05-19T10:00:00Z",
    "reason": "Incorrect status; reverting to 'draft'"
  }'
```
