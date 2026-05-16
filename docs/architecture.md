# Architecture (placeholder)

This document is **a stub** that will be filled out as components land.
The complete authoritative descriptions live in:

- [`design.md`](design.md) — the long-form architecture: components, dataflow, CRDs, DB conventions, API contracts.
- [`decisions.md`](decisions.md) — ADRs 001–010 (foundational decisions that the rest of the codebase depends on).
- [`phases.md`](phases.md) — phased delivery plan.
- [`operations.md`](operations.md) — backup, restore, failover, runbooks.

When you land Phase 1 (API server + dynamic routing), replace this stub with:

1. A top-down diagram (data plane → control plane → developer plane).
2. The 30-second pitch (what Velocity is, what problem it solves).
3. The component table — owners, language, runtime, key invariants.
4. Cross-references to the long-form docs above so casual readers can drill in.

Until then, start with [`design.md`](design.md) §1 and [`decisions.md`](decisions.md).
