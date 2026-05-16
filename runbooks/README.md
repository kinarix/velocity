# Runbooks

Operational guides for on-call. Each runbook follows the structure described
in [`docs/operations.md`](../docs/operations.md):

1. **Symptom** — what the alert fires on; what the user observes.
2. **Severity** — page / ticket / FYI.
3. **Triage** — five-minute decision tree.
4. **Mitigation** — actions to restore service.
5. **Root cause** — diagnostic steps once mitigated.
6. **Postmortem** — link to the post-incident doc.

Keep runbooks short and prescriptive. If you find yourself writing prose,
move it to `docs/`.

## Index

| Runbook | Trigger |
|---|---|
| [postgres-failover.md](postgres-failover.md) | Primary Postgres unreachable / patroni failover |
| [operator-stuck-reconcile.md](operator-stuck-reconcile.md) | A CRD stuck in `Pending` / `Failed` for > 5 min |
| [webhook-down.md](webhook-down.md) | Validating webhook 5xx or timeout; CRD applies blocked |
| [audit-chain-break.md](audit-chain-break.md) | `audit_chain_state.prev_hash` no longer matches latest row (ADR-005) |
| [rls-bypass-detected.md](rls-bypass-detected.md) | `velocity_api` reports `rolbypassrls=true` at startup (ADR-007) |

> Phase 0 ships skeletons for the first two. Remaining runbooks are stubbed
> against the alerts they will pair with in later phases.
