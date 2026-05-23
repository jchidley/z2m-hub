# Reviews

Historical z2m-hub data reviews live here when a newer review replaces them in [[plan]].

## 2026-04-24 00:31 BST Data Review

This was the first structured repo-local review of z2m-hub controller logs and PostgreSQL state.

Review window: no previous repo-local `lat.md/plan.md` timestamp existed, so this was the first structured data review. The production pull used recent `z2m-hub` logs plus TimescaleDB state through **2026-04-24 00:31 BST**.

Evidence:
- `z2m-hub` was active on `pi5data`; logs showed motion-light automation turning lights on/off, resetting timers, and respecting manual-off suppression.
- DHW tracking wrote **76** PostgreSQL rows between **2026-04-23 19:56:50 BST** and **2026-04-24 00:00:20 BST**.
- DHW `remaining_litres` ranged from **141L** to **201L**; the evening charge recovered to `full` with `remaining_litres=201L`, `t1=44.14°C`, and `hwc_storage=43.5°C`.
- The same charge path also showed impossible `HwcStorageTemp=0.0` inputs in several no-crossover decisions before valid storage readings returned.

Status changes:
- **PostgreSQL-first runtime:** **Complete**. Production logs and TimescaleDB rows confirmed live PostgreSQL-backed operation.
- **Motion lighting automation:** **Operational**. Recent logs showed darkness gating, shared timer reset, timer expiry, and manual-off suppression working.
- **DHW tracking model:** **Progressing**. The model recovered to a full-cylinder estimate after the evening charge, but impossible storage-temperature readings had to be filtered before this path could be considered settled.

New issue:
- **Open: Reject impossible HwcStorageTemp readings** — `HwcStorageTemp=0.0` appeared during an active charge while T1 remained around 43-44°C. Treat these as stale/bad input rather than sharp-thermocline evidence.
