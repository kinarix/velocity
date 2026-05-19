---
title: Search
description: Tier 1, 2, and 3 search with real-time indexing
---

Velocity supports three search tiers, each with different performance and cost characteristics.

## Tier 1: Trigram Search (Default)

**Speed:** < 5ms | **Coverage:** 90 days hot | **Cost:** Free (built-in)

Trigram search (3-character substring matching) is enabled by default on all string fields. Fast for quick lookups.

```bash
# List with substring filter
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'filter[supplier_code]=TATA'  # Matches TATA001, TATA_INC, etc.
```

### Use Case

Quick lookups (e.g., "Find all POs from supplier code containing TATA").

### Limitations

- No relevance ranking
- No typo tolerance
- Case-insensitive substring only

## Tier 2: Postgres Full-Text Search (FTS)

**Speed:** 50-100ms | **Coverage:** 90 days hot | **Cost:** Free (Postgres built-in)

Full-text search with ranking, phrase queries, and language-specific stemming.

Enable in schema:

```yaml
spec:
  search:
    tier: 2
    fields: [supplier_code, notes]
    language: english
```

Query:

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "where": {
      "field": "_search",
      "op": "match",
      "value": "supplier OR office"
    },
    "sort": [
      { "field": "_rank", "direction": "desc" }
    ],
    "limit": 50
  }'
```

Response includes `_rank` (relevance score 0-1):

```json
{
  "data": [
    {"id":"PO-001","supplier_code":"TATA001","_rank":0.95},
    {"id":"PO-002","supplier_code":"ACC001","_rank":0.75}
  ]
}
```

### Features

- **Phrase queries:** `"office supplies"` (exact phrase)
- **Boolean:** `supplier & (office | stationery)`
- **Ranking:** Results sorted by relevance
- **Stemming:** `running`, `runs`, `ran` all match `run`
- **Stop words:** Common words (the, and) ignored

### Language Support

- English, German, French, Spanish, Portuguese, Italian, Dutch, Swedish, Norwegian, Danish, Russian, Chinese, Japanese
- Configure via `language: english` in schema

### Use Case

Full-text search for documents, descriptions, notes fields.

## Tier 3: Typesense (Real-time)

**Speed:** < 20ms | **Coverage:** 5 years (via archive) | **Cost:** ~$50-100/month (hosted)

Real-time, typo-tolerant search with faceting and sorting. Built for production search UIs.

Enable in schema:

```yaml
spec:
  search:
    tier: 3
    fields:
      - name: supplier_code
        searchable: true
        facet: true
      - name: notes
        searchable: true
      - name: status
        facet: true
```

### Indexing (via CDC)

Every CREATE/UPDATE is sent to Typesense via the outbox CDC pattern:

```
Your write ──> Postgres main table ──> Postgres outbox table
                  │                         │
                  └─────────────────> CDC worker ──────> Typesense
```

This is transactional: the main table and outbox update in the same transaction.

### Query

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/search \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "q": "tata",
    "filter_by": "status:approved",
    "facet_by": "status",
    "sort_by": "_text_match:desc,amount:desc",
    "limit": 20,
    "typo_tolerance": true
  }'
```

Response:

```json
{
  "data": [
    {
      "id": "PO-001",
      "supplier_code": "TATA001",
      "status": "approved",
      "amount": 50000,
      "_text_match": 45
    }
  ],
  "facets": {
    "status": [
      {"count": 142, "value": "approved"},
      {"count": 58, "value": "draft"}
    ]
  },
  "search_stats": {
    "total_hits": 200,
    "query_time_ms": 12,
    "out_of": 2500
  }
}
```

### Features

- **Typo tolerance:** `tata` matches `TATA`, `TATA001`
- **Faceting:** Drill down by status, category, etc.
- **Real-time indexing:** Updates within seconds
- **Prefix search:** Type-as-you-search suggestions
- **Geo search:** Distance queries (deferred)
- **Synonym support:** Map `PO` → `purchase order`

### Cost & Limits

- **Typesense Cloud:** $50/month (25M documents), $100/month (100M)
- **Self-hosted:** Free software, hosting cost
- **Per-field indexing:** Only searchable fields are indexed (smaller index)

### Blue-Green Reindex

When search tier changes or fields are updated, the operator performs a blue-green swap (zero downtime):

1. Build new index in shadow collection
2. Verify it's consistent with main
3. Swap aliases atomically
4. Old index stays active until swap completes

## Cross-Schema Search

Search across all schemas in an org:

```bash
curl -G https://api.velocity.acme.com/api/acme/search \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'q=TATA' \
  --data-urlencode 'limit=50'
```

Response returns matches from all searchable schemas:

```json
{
  "data": [
    {
      "schema": "supply-chain/procurement/purchase-order/v1",
      "id": "PO-001",
      "fields": {"supplier_code":"TATA001","amount":50000}
    },
    {
      "schema": "sourcing/supplier/v1",
      "id": "TATA001",
      "fields": {"name":"Tata Steel","country":"India"}
    }
  ]
}
```

RLS is enforced per schema (you only see results you have read access to).

## Choosing a Tier

| Requirement | Tier | Rationale |
|-------------|------|-----------|
| Quick substring match | 1 | Zero cost, fast |
| Full-text search, relevance | 2 | Postgres built-in, good for 90 days hot |
| Production UI, typos, facets | 3 | Best UX, but requires Typesense |
| Archive search (>90 days) | 3 only | Warm-reader doesn't support FTS; use archive/query |

## Configuration Examples

### Tier 1 (Default)

```yaml
spec:
  search:
    tier: 1
```

No configuration needed. All string fields are trigram-searchable.

### Tier 2 (FTS)

```yaml
spec:
  search:
    tier: 2
    fields: [supplier_code, notes, description]
    language: english
```

### Tier 3 (Typesense)

```yaml
spec:
  search:
    tier: 3
    fields:
      - name: supplier_code
        searchable: true
        facet: true
      
      - name: notes
        searchable: true
        facet: false
      
      - name: status
        searchable: false
        facet: true
      
      - name: amount
        searchable: false
        facet: false
```

## Monitoring

### Check Index Health

```bash
# Typesense collection stats
kubectl exec -n typesense deployment/typesense -- \
  curl localhost:8108/collections/acme_supply_chain_procurement_purchase_order_v1 | jq .
```

Output:

```json
{
  "num_documents": 15042,
  "num_sequences": 15043,
  "data_size_bytes": 1234567,
  "created_at": 1684489920
}
```

### Monitor CDC Lag

If Typesense is out of sync with Postgres:

```sql
SELECT COUNT(*) as outbox_lag FROM acme_supply_chain_procurement.purchase_order_v1_outbox
WHERE published_at IS NULL;
```

If lag grows unbounded, restart the CDC worker:

```bash
kubectl rollout restart deployment/velocity-api -n velocity-system
```

### Metrics

```
velocity_search_queries_total{tier="3", schema="..."} 15042
velocity_search_latency_seconds{tier="3"} 0.012
velocity_cdc_lag_records{schema="..."} 0
velocity_typesense_documents{schema="..."} 15042
```

Alert on high CDC lag:

```yaml
alert: CDCLagHigh
expr: velocity_cdc_lag_records > 1000
for: 5m
annotations:
  summary: "CDC lag > 1000 records for {{ $labels.schema }}"
```

## Migration Between Tiers

### Tier 1 → Tier 2

```yaml
# Before
spec:
  search:
    tier: 1

# After
spec:
  search:
    tier: 2
    fields: [supplier_code, notes]
```

Apply. The operator provisions the FTS index; existing Postgres data is auto-indexed.

### Tier 2 → Tier 3

```yaml
spec:
  search:
    tier: 3
```

Apply. The operator:
1. Provisions Typesense collection
2. Exports all Postgres records to Typesense (background job)
3. Enables CDC worker
4. Switches traffic to Typesense

During index population, search continues using Tier 2.

## Deferred Features

- **Geo search:** Distance queries (Phase 10)
- **Synonym management:** Custom synonym maps (Phase 10)
- **Search analytics:** Popular queries, click-through rates (Phase 10)
- **Warm-tier FTS:** Full-text search on archived records (Phase 4 revision)
