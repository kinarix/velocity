# Postgres failover

**Symptom**
- Operator + API logs: `postgres error: connection refused` or `terminating connection due to administrator command`
- `velocity-operator` and `velocity-api` `/readyz` returning 503
- `pg_isready` against the primary times out

**Severity**: page

## Triage (5 min)

1. Check Patroni cluster state (Phase 5+: managed Postgres):
   ```bash
   patronictl -c /etc/patroni.yml list
   ```
   If a sync replica is `Running` and `Leader = false`, failover is in progress.
2. Look for paged-out OOM kills, full disks, or stuck WAL replication:
   ```bash
   kubectl logs -n velocity-data sts/postgres-primary --previous | tail -200
   df -h /var/lib/postgresql/data    # on the host or in-pod
   ```
3. If the primary host is healthy but Patroni isn't running, `systemctl status patroni` (or its container equivalent).

## Mitigation

| Situation | Action |
|---|---|
| Patroni already promoted a replica | Wait for `velocity-operator` to reconnect (≤ 30s). Verify with `make db-verify-rls`. |
| No replica is in sync | **Stop here.** Page DB lead. Promoting an out-of-sync replica is a data-loss decision. |
| Disk full on primary | Truncate WAL not yet archived only if you have a known-good base backup. Otherwise grow disk + restart. |
| Connection storms (max_connections exceeded) | Scale `velocity-api` to 1 replica temporarily; raise `max_connections` after the storm clears. |

## Root cause

Once mitigated, capture:

- `pg_stat_activity` snapshot at incident time
- Patroni event log (`patronictl history`)
- last successful WAL position (`pg_last_wal_receive_lsn()` on replicas)
- timeline of operator/API readiness probe failures

## Postmortem

Link the incident doc here.

---

> **Phase 0 caveat:** in local dev (docker-compose), there is no replica.
> "Failover" means restarting the container and verifying that the platform
> schema and audit chain are intact. See [audit-chain-break.md](audit-chain-break.md).
