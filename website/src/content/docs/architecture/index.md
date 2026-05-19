---
title: Architecture
description: Velocity's database, stored procedures, security model, and migrations.
---

This section documents the **inside** of Velocity — what the platform owns in Postgres, how it enforces security at the database layer, and how the schema evolves over time. Most of the day-to-day docs describe the CRD surface and REST API; this section is for the operator who needs to know what Velocity is doing to their database.

## What's here

- **[Database schema](/architecture/database/)** — every table in the `platform` schema, what it stores, how it's partitioned and indexed.
- **[Stored procedures](/architecture/stored-procedures/)** — `audit_insert()` and `audit_verify_window()`, the only sanctioned writers/verifiers for the audit chain.
- **[RLS and grants](/architecture/rls-and-grants/)** — the two-role model (`velocity_api`, `velocity_operator`), `SET LOCAL ROLE` per transaction, per-tenant grants.
- **[Migrations](/architecture/migrations/)** — index of the six migrations that bring a cluster from empty to current.

## How the pieces fit

```
                        ┌─────────────────────────────┐
   Kubernetes API ─────►│  velocity-operator          │
   (CRDs)               │  • mirrors CRDs into        │
                        │    platform.schema_defs     │
                        │  • provisions per-tenant    │
                        │    schemas + tables         │
                        │  • partition manager        │
                        │  • anomaly scanner          │
                        └──────────────┬──────────────┘
                                       │  velocity_operator role
                                       │  (NOSUPERUSER, NOBYPASSRLS,
                                       │   CREATEROLE for tenants)
                                       ▼
                        ┌─────────────────────────────┐
   REST clients ───────►│  velocity-api               │
                        │  • SET LOCAL ROLE <domain>  │
                        │  • parameterised queries    │
                        │  • audit_insert() only      │
                        └──────────────┬──────────────┘
                                       │  velocity_api role
                                       │  (NOSUPERUSER, NOBYPASSRLS)
                                       ▼
                        ┌─────────────────────────────┐
                        │  Postgres                   │
                        │  • platform.* (this dir)    │
                        │  • {org}_{app}_{domain}.*   │
                        │    (provisioned per tenant) │
                        └─────────────────────────────┘
```

Three things are load-bearing across every page in this section:

1. **`velocity_api` is `NOBYPASSRLS`** — verified at startup ([ADR-007](/adrs/#adr-007)). If it ever has `BYPASSRLS=true`, the API server refuses to start. RLS is therefore an actual backstop, not a paper one.
2. **Direct `INSERT` into `platform.audit_log` is revoked.** The chain is only writable through [`audit_insert()`](/architecture/stored-procedures/#audit_insert).
3. **Idempotent migrations.** Every file under `migrations/` is safe to re-apply. Re-running is the recovery primitive when state drift is suspected.
