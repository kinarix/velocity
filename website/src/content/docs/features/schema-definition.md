---
title: SchemaDefinition
description: The core CRD that drives everything
---

The `SchemaDefinition` is the canonical source of truth for a data entity in Velocity. It specifies:

- Data shape (fields, types, constraints)
- How data is accessed (authentication, authorization, row/field filtering)
- How data is stored (time machine, archive, search)
- How data is validated (CEL rules)
- How data is observed (SLO targets, dashboard config)

When you apply a SchemaDefinition, the platform automatically:
1. Provisions a Postgres table
2. Generates HTTP CRUD endpoints
3. Wires validation, search indexing, and audit logging
4. Creates Grafana dashboards
5. Sets up alerting rules

## Basic Example

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: purchase-order
  namespace: acme-supply-chain-procurement
spec:
  org: acme
  app: supply-chain
  domain: procurement
  object: purchase-order
  version: v1
  
  fields:
    - name: id
      type: string
      required: true
      unique: true
    
    - name: supplier_code
      type: string
      required: true
      filterable: true
    
    - name: amount
      type: number
      required: true
      filterable: true
      sortable: true
    
    - name: status
      type: enum
      enum: [draft, approved, shipped, delivered]
      required: true
      default: draft
    
    - name: created_at
      type: timestamp
      autoPopulated: true
```

## Field Types

All field types are strongly typed in the Postgres table and the REST API.

| Type | Postgres | JSON | Constraints |
|------|----------|------|-------------|
| `string` | `text` | string | `maxLength`, `minLength`, `pattern` |
| `integer` | `bigint` | number | `min`, `max` |
| `number` | `numeric(18,2)` | number | `min`, `max` |
| `boolean` | `boolean` | boolean | N/A |
| `timestamp` | `timestamptz` | RFC3339 | `autoPopulated`, `immutable` |
| `date` | `date` | ISO 8601 | N/A |
| `time` | `time` | HH:MM:SS | N/A |
| `json` | `jsonb` | object/array | `filterable` → GIN index. See [Nested JSON Objects](#nested-json-objects) below. |
| `enum` | `text` | string | `enum: [...]`, `default` |
| `uuid` | `uuid` | string | `autoPopulated` (generate if not provided) |
| `bytes` | `bytea` | base64 | `maxSize` |

### Examples

**String with validation:**

```yaml
- name: email
  type: string
  required: true
  unique: true
  pattern: '^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$'
  sensitivity: pii
```

**Number with range:**

```yaml
- name: discount_percent
  type: number
  required: true
  min: 0
  max: 100
```

**Enum with default:**

```yaml
- name: status
  type: enum
  enum: [draft, approved, shipped, delivered, archived]
  required: true
  default: draft
```

**JSON for nested structures:**

```yaml
- name: metadata
  type: json
  required: false
  filterable: true   # emits a GIN index for containment queries
```

See [Nested JSON Objects](#nested-json-objects) for the full shape and how to validate it.

**Auto-populated fields:**

```yaml
- name: id
  type: uuid
  autoPopulated: true  # Generates if not provided

- name: created_at
  type: timestamp
  autoPopulated: true  # Sets to now()

- name: created_by
  type: string
  autoPopulated: true  # Sets from JWT actor_id
```

## Nested JSON Objects

`type: json` is the escape hatch for structured data that doesn't fit into top-level scalar fields. The column is `jsonb`, so the payload can be an object, an array, or any depth of nesting. There is no separate "object" or "array" field type — they all ride on `json`.

### Shape

```yaml
- name: address
  type: json
  required: true
  filterable: true        # creates a GIN index on the column

- name: line_items
  type: json              # array of objects works fine
```

The corresponding payload is whatever JSON you send:

```json
{
  "address": {
    "line1": "1 Industrial Estate",
    "city": "Bengaluru",
    "geo": { "lat": 12.97, "lon": 77.59 }
  },
  "line_items": [
    { "sku": "TATA001-A", "qty": 10, "unit_price": 199.00 },
    { "sku": "TATA001-B", "qty":  4, "unit_price":  49.50 }
  ]
}
```

### What you get out of the box

- **Storage:** native Postgres `jsonb` — binary, deduplicated keys, indexable.
- **Indexing:** `indexed: true` or `filterable: true` produces a `GIN` index on the column. Containment queries (`@>`, `?`, `?&`) use it; equality queries on deep keys do not.
- **Time machine:** every change is captured in the history table the same way scalar fields are. Diffs include the full nested structure.
- **Audit:** the full document goes into the audit chain like any other field.
- **Masking:** the entire field is masked or unmasked as a unit. There is no per-key masking inside a `json` value — if any part of the document is sensitive, mark the whole field `sensitivity: pii` (or stronger) and rely on field-level access to gate it.

### Validating nested structure with CEL

Per-key types and required-ness are not declared on the schema; you express them as CEL rules over the parent record. CEL can traverse nested fields with dotted access:

```yaml
spec:
  validation:
    rules:
      - rule: "has(self.address.line1) && size(self.address.line1) > 0"
        message: "address.line1 is required"

      - rule: "self.address.geo.lat >= -90.0 && self.address.geo.lat <= 90.0"
        message: "address.geo.lat must be a valid latitude"

      - rule: "size(self.line_items) > 0"
        message: "at least one line item required"

      - rule: "self.line_items.all(li, li.qty > 0)"
        message: "all line items must have positive qty"
```

Each rule still runs under the per-rule timeout (default 10 ms). Keep the structure shallow enough that traversal stays cheap.

### Querying

The query DSL filters by the column as a whole. The most useful form is JSONB containment:

```http
POST /api/acme/supply-chain/procurement/purchase-order/v1/query
Content-Type: application/json

{
  "where": { "field": "address", "op": "contains", "value": { "city": "Bengaluru" } },
  "limit": 50
}
```

The GIN index handles this. Filtering on a single deep key (`address.city = "Bengaluru"` as a flat predicate) is not yet supported in the DSL — use containment, or denormalise the key you query against into a top-level field.

### What it isn't

- Not a sub-schema. Velocity does not (today) typecheck nested fields against a declared shape — `string` vs `integer`, `required`, `pattern`, `min`/`max` apply only to top-level scalar fields.
- Not a place to hide foreign keys. Use a top-level `ref` field for cross-schema relations; the operator validates only top-level refs.
- Not unique-indexable. You can put a unique constraint on a top-level scalar; you cannot ask Postgres to enforce uniqueness across a key inside `json`.

If you find yourself wanting strict typing on nested fields, lift them to the top level. JSON is for genuinely variable shapes (per-tenant metadata, third-party payload pass-through, denormalised sub-records).

## Constraints

### Unique Constraints

```yaml
spec:
  fields:
    - name: id
      type: string
      unique: true  # Single-field unique
  
  uniqueConstraints:
    - fields: [supplier_code, fiscal_year]  # Composite unique
      name: supplier_fiscal_year_uk
```

The operator generates Postgres unique indexes with `WHERE deleted_at IS NULL` to allow soft-deletes.

### Check Constraints

Express via validation rules (CEL):

```yaml
spec:
  validation:
    rules:
      - rule: "self.amount > 0"
        message: "Amount must be positive"
      
      - rule: "self.end_date > self.start_date"
        message: "End date must be after start date"
```

### Foreign Keys (Deferred)

Cross-schema references via the `refs` field are planned but deferred to Phase 10. Currently, you can denormalize or use application logic.

## Validation Rules (CEL)

CEL expressions are evaluated with a 10ms timeout. The expression has access to `self` (the record).

```yaml
spec:
  validation:
    rules:
      - rule: "self.amount > 0 && self.currency in ['USD', 'EUR', 'GBP']"
        message: "Invalid amount or currency"
        maxExecutionMs: 10
      
      - rule: "self.status == 'approved' ? self.approved_by != null : true"
        message: "Approved records must have approved_by set"
```

### Available CEL Functions

- Math: `abs`, `ceil`, `floor`, `min`, `max`
- String: `size`, `contains`, `startsWith`, `endsWith`, `split`, `matches` (regex)
- Type: `type`, `has`
- Date: `now`, `timestamp`

### Constraints on CEL

- Maximum 10 KB per rule
- Maximum nesting depth: 10
- Execution timeout: 10ms (configurable per rule)
- No external function calls or imports

## Search Configuration

Configure full-text search by tier:

```yaml
spec:
  search:
    tier: 3  # 1 (trigram), 2 (FTS), 3 (Typesense)
    fields: [supplier_code, notes]  # Indexed fields
    language: english
```

### Tier 1: Trigram Search (Default)

- **Speed:** Fast, O(1) in most cases
- **Coverage:** 90 days hot only
- **Use case:** Quick substring matches
- **Limitation:** Case-insensitive, no typo-tolerance

### Tier 2: Postgres Full-Text Search

- **Speed:** Moderate, O(n log n)
- **Coverage:** 90 days hot only
- **Use case:** Phrase searches, relevance ranking
- **Limitation:** Language-specific, no typo-tolerance

### Tier 3: Typesense (Real-time)

- **Speed:** Very fast, real-time indexing
- **Coverage:** Up to 5 years (with archive tier-up)
- **Use case:** Production search UI
- **Feature:** Typo tolerance, faceting, sorting

When search tier changes, the operator performs a blue-green index swap (zero downtime).

## Time Machine Configuration

All schemas are time-machine-enabled by default. Customize retention:

```yaml
spec:
  timeMachine:
    enabled: true
    hotRetention: 90d  # Hot storage in Postgres history table
    warmRetention: 5y  # Warm storage in S3 Parquet (optional)
    coldRetention: none  # Glacier/offline (deferred)
    snapshotRetention: 7d
```

### Storage Tiers

- **Hot (0-90 days):** Postgres partitioned table, instant query
- **Warm (90-5 years):** S3 Parquet, queryable via warm-reader (DataFusion)
- **Cold (5+ years):** Glacier, restore required

### History Semantics

Every mutation creates a history entry:

```sql
INSERT INTO purchase_order_v1_history (
  entity_id, operation, old_value, new_value, actor, timestamp
) VALUES (...)
```

Operations are immutable. You cannot delete from history or edit audit logs.

## Archive Configuration

Define automatic archive policies:

```yaml
spec:
  archive:
    enabled: true
    policies:
      - name: age-based
        trigger:
          type: age
          days: 90  # Archive records after 90 days (not deleted)
        destination:
          type: s3
          bucket: velocity-archives
          prefix: acme/supply-chain/procurement
        format: parquet
```

### Trigger Types

- **`age`** (implemented): Archive records older than N days
- **`size`** (implemented): Archive when table reaches N GB
- **`cel`** (deferred): Custom CEL expression (e.g., status == 'archived')

### Destinations

- **S3 Parquet** (implemented): Compressed columnar format, queryable via warm-reader
- **Postgres cold table** (deferred): Alternative to S3

## Row-Level Security (RLS)

Define row filters that apply per actor:

```yaml
spec:
  access:
    rowFilter:
      - role: region-manager
        condition: "region = current_setting('app.current_region')"
      
      - role: procurement-reader
        condition: "status != 'draft'"  # Can't see draft POs
```

The operator generates RLS policies that Postgres enforces for every query.

## Field-Level Access

```yaml
spec:
  access:
    fieldAccess:
      - field: cost_basis
        read: [finance-reader, finance-admin]
        write: [finance-admin]
      
      - field: supplier_secret
        read: [procurement-admin]
        write: [procurement-admin]
        masking:
          strategy: redacted
          visibleChars: 0
```

### Masking Strategies

- **`partial`:** Show first/last N characters
- **`hash`:** Show SHA256(value)
- **`range`:** Show only min-max (for numbers)
- **`redacted`:** Fully redacted (`***`)

## Versioning

Schemas can evolve safely. Breaking changes require approval:

```yaml
metadata:
  annotations:
    velocity.sh/breaking-change: "approved"
    
spec:
  version: v2  # Increment when breaking
  
  # Migration steps
  migrations:
    - type: AddColumn
      column:
        name: sku
        type: string
        required: false
    
    - type: AddConstraint
      constraint:
        name: sku_unique
        type: unique
        fields: [sku]
    
    - type: DropColumn  # Requires approval annotation
      column: deprecated_field
      reason: "Unused since Q1 2026"
```

**Safe operations** (applied automatically):
- Add column (with default if required)
- Add index
- Add constraint (if existing data satisfies it)
- Rename column (with operator assistance)

**Breaking operations** (require annotation):
- Drop column
- Change column type
- Change column nullability (NULL → NOT NULL without default)

## Status and Conditions

The SchemaDefinition has a status subresource:

```yaml
status:
  phase: Ready  # Provisioning | Ready | Failed | Upgrading
  conditions:
    - type: Provisioned
      status: "True"
      reason: "TableCreated"
    - type: HistoryTableReady
      status: "True"
    - type: SearchIndexReady
      status: "True"
    - type: RLSPoliciesApplied
      status: "True"
```

Check the status:

```bash
kubectl get schemadefs -A
kubectl describe schemadefs purchase-order -n acme-supply-chain-procurement
```

## Complete Example

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: purchase-order
  namespace: acme-supply-chain-procurement
  labels:
    velocity.sh/org: acme
    velocity.sh/app: supply-chain
    velocity.sh/domain: procurement
spec:
  org: acme
  app: supply-chain
  domain: procurement
  object: purchase-order
  version: v1
  displayName: "Purchase Order"
  description: "Supplier purchase order lifecycle"
  
  fields:
    - name: id
      type: string
      required: true
      unique: true
      description: "PO identifier (format: PO-XXXXXXXX)"
    
    - name: supplier_code
      type: string
      required: true
      filterable: true
      sortable: true
    
    - name: amount
      type: number
      required: true
      min: 0
      filterable: true
      sortable: true
    
    - name: currency
      type: enum
      enum: [USD, EUR, GBP, INR]
      required: true
      default: USD
    
    - name: status
      type: enum
      enum: [draft, approved, shipped, delivered, archived]
      required: true
      default: draft
      filterable: true
      sortable: true
    
    - name: approved_by
      type: string
      required: false
      filterable: true
      sensitivity: pii
    
    - name: approval_date
      type: timestamp
      required: false
      filterable: true
      sortable: true
    
    - name: notes
      type: string
      required: false
      maxLength: 1000
    
    - name: metadata
      type: json          # nested objects/arrays welcome — validate with CEL
      required: false
      filterable: true    # GIN index for containment queries
    
    - name: created_at
      type: timestamp
      autoPopulated: true
      sortable: true
    
    - name: created_by
      type: string
      autoPopulated: true
    
    - name: updated_at
      type: timestamp
      autoPopulated: true
    
    - name: version
      type: integer
      autoPopulated: true
  
  uniqueConstraints:
    - fields: [id]
  
  validation:
    rules:
      - rule: "self.amount > 0"
        message: "Amount must be positive"
      
      - rule: "self.status == 'approved' ? self.approved_by != null && self.approval_date != null : true"
        message: "Approved records must have approved_by and approval_date"
      
      - rule: "self.status in ['delivered', 'archived'] ? self.approval_date != null : true"
        message: "Delivered/archived records must have approval_date"
  
  search:
    tier: 2
    fields: [supplier_code, notes]
    language: english
  
  access:
    roles:
      create: [procurement-writer]
      read: [procurement-reader, procurement-writer]
      update: [procurement-writer]
      delete: [procurement-admin]
    
    rowFilter:
      - role: region-manager
        condition: "region = current_setting('app.current_region')"
    
    fieldAccess:
      - field: cost_basis
        read: [finance-reader]
        write: [finance-admin]
  
  timeMachine:
    enabled: true
    hotRetention: 90d
    warmRetention: 5y
  
  archive:
    enabled: true
    policies:
      - name: age-based
        trigger:
          type: age
          days: 90
        destination:
          type: s3
          bucket: velocity-archives
          prefix: acme/supply-chain
        format: parquet
        requiredApproval: true
```

Apply it:

```sh
kubectl apply -f purchase-order-schema.yaml
velocity status
```

The platform provisions the table, routes, indexes, and dashboards automatically.
