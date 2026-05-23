# Plan

Open items and current operational review notes for z2m-hub. Last data review: **2026-05-23 13:14 BST**.

## Latest Data Review

Most recent review of z2m-hub controller logs and PostgreSQL state.

Review window: production pull from the previous plan timestamp (**2026-04-24 00:31 BST**) through **2026-05-23 13:14 BST**. Evidence came from `pi5data` `z2m-hub` journal entries plus TimescaleDB `multical`, `dhw`, and `dhw_capacity` rows.

Evidence:
- Controller logs show motion lighting remained active: **251** dark-enough motion ON sequences, **703** timer resets while lights were already on, **126** timer expiries, **125** manual-off suppressions, and **6533** too-bright skips.
- PostgreSQL-backed DHW state stayed live: `dhw` had **87827** rows from **2026-04-24 07:33:40 BST** through **2026-05-23 13:14:20 BST**; `remaining_litres` ranged from **0L** to **201L**.
- `multical` had **239856** rows across the review window, from **2026-04-24 00:31:10 BST** through **2026-05-23 13:14:10 BST**, with T1 between **31.5°C** and **45.9°C** and flow up to **780L/h**.
- `dhw_capacity` still recommends **201L** from the last capacity row at **2026-04-12 03:00:02 BST**.
- DHW logs showed **74** crossover completions to **201L**, **616** no-crossover charge endings, and **25** HwcStorage crash caps, including normal caps to **148L** after draws.
- Impossible `HwcStorageTemp=0.0` is still present: **303** controller decisions used zero HwcStorage values and **8000** persisted `dhw` rows in the review window had `hwc_storage=0`.
- PostgreSQL was mostly working but not perfectly continuous: logs contain **5292** connection errors between **2026-04-25 00:47 BST** and **2026-05-19 17:58 BST**. Later TimescaleDB rows confirm recovery.

Status changes:
- **PostgreSQL-first runtime:** remains **Complete / monitor for interruptions**. The service is writing and reading PostgreSQL data, but the repeated connection-error bursts should be treated as an operational reliability issue rather than a migration blocker.
- **Motion lighting automation:** changed to **Problematic / Investigating** after focused log review. Data still shows lux gating, timer resets, and timer expiries, but many motion ON sequences are cancelled by an `OFF` report within milliseconds, which is too fast to be a real manual override.
- **DHW tracking model:** remains **Progressing**. It repeatedly recovers to **201L / full** after successful charges and tracks draw/crash caps, but bad HwcStorage inputs and unexpectedly frequent `dhw` writes mean it is not settled.
- **Reject impossible HwcStorage readings:** remains **Open** and is more urgent. Zero HwcStorage values now appear in both controller decisions and persisted rows, including some crossover completions.

New issues:
- **Open: Investigate PostgreSQL connection-error bursts** — repeated connect failures occurred over the review window despite later recovery and continued writes.
- **Open: Review DHW write frequency** — `dhw` received **87827** rows since the previous review, including apparent 10-second writes during active charge periods, which conflicts with the intended state-change-boundary persistence model.

## Active Work

These items are the live unresolved z2m-hub questions.

### Complete / Monitor: PostgreSQL-first Runtime

The repo-local runtime cutover is complete and production is writing/reading through PostgreSQL-backed paths.

The 2026-05-23 review confirms live TimescaleDB data through the review timestamp, but also found repeated PostgreSQL connection-error bursts. Treat the migration as complete while monitoring and investigating runtime availability interruptions.

### Problematic / Investigating: Motion Lighting Automation

The hall and landing motion automation is live, but production behaviour is patchy.

A focused 2026-05-23 review of motion-related logs from **2026-05-20 06:38 BST** to **2026-05-23 13:16 BST** found **36** motion-triggered ON sequences, **18** timer expiries, and **18** apparent manual-off cancellations. The cancellation pattern is suspicious: **17** of the 18 happened within about **0.07-0.18s** of the ON command, far too quickly for a human override. This likely means a stale or failed Zigbee `OFF` state report is being interpreted as manual OFF, cancelling the shared timer and suppressing retriggering. If one light did turn on before the false cancellation, it can then remain on without an automation timer.

Timer-driven sessions that were not cancelled generally did turn off, but occupancy resets legitimately extended some ON periods beyond 300s; the longest reviewed timer-driven period was about **17 minutes**. Lux gating may also feel patchy around the threshold: hall motion often skipped at **16 lx** because the configured threshold is `<= 15 lx`.

### Progressing: DHW Tracking Model

The DHW model is live, persists state to PostgreSQL, and recovers useful volume after charge completion.

The 2026-05-23 review showed repeated full-cylinder recoveries to **201L**, no-crossover interpolation paths, and HwcStorage crash caps. Continue validating charge/draw transitions, especially where impossible HwcStorage readings or unusually frequent persistence may distort the model.

### Open: Reject Impossible HwcStorageTemp Readings

The eBUS storage-temperature input can briefly report impossible values during charging.

The 2026-05-23 review found this is ongoing: **303** controller decisions used `HwcS=0.0`, **8000** persisted `dhw` rows had `hwc_storage=0`, and some crossover completions logged `HwcS=0.0`. The input filter should mark physically impossible storage readings as stale so they do not influence thermocline classification, persistence, or dashboard confidence.

### Open: Investigate PostgreSQL Connection-Error Bursts

The service should recover from transient database failures, but repeated connection errors need operational review.

Logs since the previous review contain **5292** `PostgreSQL connect error` entries, first seen **2026-04-25 00:47 BST** and last seen **2026-05-19 17:58 BST**. Later `dhw` and `multical` rows prove recovery, but the cause and user-visible impact are unknown.

### Open: Review DHW Write Frequency

DHW persistence should happen on meaningful state-change boundaries, not every polling tick.

The review found **87827** `dhw` rows over about 29 days, with recent rows arriving every 10 seconds during a charge state. Confirm whether current write triggers are intentionally capturing changing sensor fields or whether the persistence gate is too broad.

### Progressing: Distinguish False Zigbee OFF Reports from Manual Override

Manual-off suppression should only trigger when a user actually cancels an active motion-light period.

The focused motion review found repeated `Manual OFF` logs within milliseconds of sending motion ON commands. The local fix now ignores an `OFF` report from a motion light during the first two seconds after automation sends that light an `ON`, keeps the timer armed, and retries `ON` for that light. Deploy and review production logs before closing this item.

## Backlog

Lower-priority work that is not blocking production.

1. **Open: Reduce or retire `lat.md/tsdb-migration.md`** once the closure note no longer needs to be a first-class node.
2. **Open: Keep plan/review docs current** now that this repo has a structured operational review file.
