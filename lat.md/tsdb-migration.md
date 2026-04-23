# TSDB Migration

This file tracks the remaining work needed before the broader InfluxDB-to-PostgreSQL migration can be called complete for z2m-hub and its shared pi5data platform.

z2m-hub itself already reads from and writes to PostgreSQL in production. Any InfluxDB mention that still matters in this repo should either be historical closeout evidence or an open migration action recorded here.

## Scope

The repo-local runtime cutover is done, so this file no longer tracks day-to-day service behaviour.

Current PostgreSQL-first behaviour lives in:
- [[architecture#Runtime structure]] for the runtime seam and ownership split
- [[interfaces#PostgreSQL read and write contract]] for the SQL read/write contract
- [[dhw#Capacity autoload and persistence]] and [[dhw#Restart recovery]] for startup/autoload behaviour
- [[infrastructure#Deployment and configuration]] and [[infrastructure#Secret management]] for config and credential handling
- [[tests#PostgreSQL interface]] and [[tests#Real PostgreSQL integration]] for durable verification specs

## z2m-hub repo-local status

The z2m-hub service migration is complete even though the shared platform migration still has final cleanup work.

Repo-local closeout evidence:
- `z2m-hub` is running on `pi5data` from the latest hardening deploy (`ExecMainPID=2695805`, `ActiveEnterTimestamp=Thu 2026-04-23 10:24:30 BST`, `SubState=running`)
- `/api/hot-water`, `/api/dhw/status`, and `/api/lights` returned sane JSON during the latest review
- `/api/hot-water` now has an explicit stale/unknown branch for missing Multical telemetry instead of presenting persisted litres as if they were live
- the journal closeout grep showed the expected startup behaviour during the latest deploy
- recent `dhw` rows were still present in PostgreSQL/TimescaleDB
- the rollback binary `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` was still present and non-empty (`7.3M`)
- the phone-sized dashboard behaviour was explicitly accepted, including the distinct unknown-water rendering

## Remaining shared migration actions

The overall migration is not complete until the remaining shared InfluxDB retirement work is finished outside this repo.

Authoritative cross-repo tracker: `~/github/energy-hub/lat.md/tsdb-migration.md`

Open actions that still affect migration completion:
1. `heatpump-analysis`: finish its repo-local PostgreSQL migration, `adaptive-heating-mvp` cutover, and verification.
2. pi5data Phase 5: retire Telegraf's `influxdb_v2` output.
3. pi5data Phase 5: remove the Grafana v2 datasource.
4. pi5data Phase 5: stop and remove the InfluxDB v2 container.
5. pi5data Phase 5: archive the v2 data volume.

z2m-hub has no remaining repo-local runtime dependency on InfluxDB. The remaining migration blockers are shared-platform completion plus the last external consumer (`heatpump-analysis`).

### Reviewed remaining Influx-only information in z2m-hub lat

A repo-wide grep for `influx` / `InfluxDB` now finds only `lat.md/tsdb-migration.md`, `lat.md/infrastructure.md`, and the `[[tsdb-migration]]` backlink in `lat.md/lat.md`.

| Location | Why it still exists | Required plan action |
|---|---|---|
| [[infrastructure#Hosts and roles]] | warns that any still-running InfluxDB v2 container on `pi5data` is temporary rather than part of the target architecture | after Phase 5, remove this temporary note so infrastructure only describes the steady state |
| [[lat#z2m-hub#Knowledge map]] | keeps the migration tracker discoverable from the graph entrypoint while closeout is still in progress | when `[[tsdb-migration]]` is retired or reduced, update this backlink so the graph does not point at a stale migration node |
| [[tsdb-migration#Historical read-parity note]] | keeps the old dual-read/parity sign-off as migration evidence | none; retain until broader migration closeout |
| `pi5data:/usr/local/bin/z2m-hub.pre-pg-rollback.bak` | preserves the last pre-PostgreSQL rollback binary during the shared rollback window | remove after Phase 5 sign-off and explicit rollback-window close |
| this file | acts as the repo-local closeout note while the shared migration is still open | retire or reduce to a backlink after shared closeout |

No other repo-local lat sections describe InfluxDB as current runtime behaviour. No additional repo-local migration blocker was found beyond the shared Phase 5 work, the existing rollback-window cleanup, and the post-Phase-5 doc cleanup now called out below.

### Reviewed repo-local Influx-shaped artifacts

The previous test-only line-protocol compatibility helper and its linked specs have now been removed.

z2m-hub no longer keeps repo-local Influx-shaped test artifacts. The remaining repo-local persistence evidence is PostgreSQL-first:
- `src/main.rs`: `dhw_write_row`
- [[tests#PostgreSQL interface]]
- [[tests#Real PostgreSQL integration]]

This means the remaining migration blockers are fully outside this repo: shared-platform Phase 5 cleanup, the external rollback artifact on `pi5data`, and the last external consumer tracked in `energy-hub`.

### Repo-local closeout actions after Phase 5

After the shared tracker closes and the rollback window is explicitly over, the remaining repo-local cleanup actions should be:
1. `pi5data`: remove `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` after the final maintenance-window sign-off.
2. `lat.md/infrastructure.md`: remove the temporary "if an InfluxDB v2 container still exists" note once Phase 5 has actually removed that container.
3. `lat.md/tsdb-migration.md`: retire this file or reduce it to a short backlink once Phase 5 is complete and no closure notes still need to live here.
4. `lat.md/lat.md`: if `[[tsdb-migration]]` stops being a first-class node, replace that knowledge-map entry with a shorter historical backlink or drop it entirely.

These are closeout actions, not current blockers. They belong in the migration plan so the last Influx-era rollback artefacts do not become permanent background clutter after the shared cutover is declared done.

Not deletion candidates: the PostgreSQL write-mapping tests, PostgreSQL write-failure tests, autoload bounds tests, startup reconstruction tests, and the live-PG verification specs. Those still describe the current PostgreSQL contract and restart-safety behaviour.

## Historical read-parity note

The old read-parity gate is closed and is kept here only as migration evidence.

Basis for that judgement:
- the 4 `multical`-backed reads were checked pre-cutover and matched InfluxDB exactly
- `dhw_capacity` startup continuity was satisfied by ensuring PostgreSQL had a sane recommendation row for autoload
- `dhw.remaining_litres` is no longer a meaningful dual-read parity target after cutover because z2m-hub itself became the PostgreSQL writer; the durable contract is restart recovery from persisted PG state, which now lives in [[dhw#Restart recovery]]

No further repo-local parity work is required unless the shared migration uncovers a data-quality issue outside z2m-hub.

## Current live evidence

The live PostgreSQL cutover still looks stable from the repo side.

Latest re-check during this review:
- service status shows the current hardening deploy on `pi5data` (`ExecMainPID=2695805`, `ActiveEnterTimestamp=Thu 2026-04-23 10:24:30 BST`, `SubState=running`)
- closeout grep showed a normal restart and clean startup, with no restart churn
- live API checks returned sane payloads for `/api/hot-water`, `/api/dhw/status`, and `/api/lights`
- `/api/hot-water` now supports both branches of the new contract: it was observed returning explicit stale/unknown values during the outage review, and after fresh telemetry resumed it returned live fields again (`multical_stale = false`, `remaining_litres = 201.0`, `timestamp = "2026-04-23T09:24:50+00:00"`)
- recent `dhw` rows were still present in PostgreSQL/TimescaleDB, with the latest row at `2026-04-23 08:52:49.577073+00`
- latest historical `multical` row in PostgreSQL was `2026-04-23 09:24:50+00`, showing fresh ingest had resumed by the end of the review
- rollback artifact remained available at `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` (`7.3M`)

## Verification commands

These are the repo-local commands worth re-running if the shared migration needs fresh confidence in z2m-hub's PostgreSQL path.

### Service status

Confirm the service is still running from the intended cutover and has not entered a restart loop.

```bash
ssh jack@pi5data 'systemctl show z2m-hub --property=ActiveEnterTimestamp,SubState,ExecMainPID'
```

### Journal regression scan

Check for PostgreSQL credential, transport, panic, or restart problems.

```bash
ssh jack@pi5data 'sudo journalctl -u z2m-hub --since "2026-04-12 12:05:00" --no-pager | grep -Ein "error|failed|panic|postgres|pg|credential|restart" | tail -n 120'
```

### Live API spot checks

Confirm the API surface still returns sane hot-water, DHW-status, and lights payloads. During a Multical outage, `/api/hot-water` should now show the explicit stale/unknown contract rather than fake live litres.

```bash
ssh jack@pi5data '
  echo "--- /api/hot-water ---" && curl -fsS http://127.0.0.1:3030/api/hot-water && echo &&
  echo "--- /api/dhw/status ---" && curl -fsS http://127.0.0.1:3030/api/dhw/status && echo &&
  echo "--- /api/lights ---" && curl -fsS http://127.0.0.1:3030/api/lights && echo
'
```

### Recent DHW rows

Confirm PostgreSQL-backed writes are still advancing.

```bash
ssh jack@pi5data 'sudo docker exec timescaledb psql -U energy -d energy -c "select time, remaining_litres, t1, hwc_storage, charge_state from dhw order by time desc limit 10;"'
```

### Rollback artifact

Confirm the pre-PG binary is still available until the shared migration reaches final decommission.

```bash
ssh jack@pi5data 'ls -lh /usr/local/bin/z2m-hub.pre-pg-rollback.bak'
```

## Completion rule

The z2m-hub repo-local migration is complete, but the broader InfluxDB-to-PostgreSQL migration is not complete until the shared tracker closes the remaining actions above.

Once the shared `energy-hub` migration marks Phase 5 done and no repo-local closure note is needed, this file can be retired or reduced to a short backlink.
