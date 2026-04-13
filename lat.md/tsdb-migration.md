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
- `z2m-hub` is still the original PostgreSQL cutover process on `pi5data` (`ExecMainPID=804858`, `ActiveEnterTimestamp=Sun 2026-04-12 12:05:26 BST`, `SubState=running`)
- `/api/hot-water`, `/api/dhw/status`, and `/api/lights` returned sane JSON during the final review
- the journal closeout grep showed normal PostgreSQL startup and no restart churn
- recent `dhw` rows were still present in PostgreSQL/TimescaleDB
- the rollback binary `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` was still present and non-empty (`7.3M`)
- the phone-sized dashboard behaviour was explicitly accepted

## Remaining shared migration actions

The overall migration is not complete until the remaining shared InfluxDB retirement work is finished outside this repo.

Authoritative cross-repo tracker: `~/github/energy-hub/lat.md/tsdb-migration.md`

Open actions that still affect migration completion:
1. `heatpump-analysis`: finish its repo-local PostgreSQL migration, `adaptive-heating-mvp` cutover, and verification.
2. `energy-hub`: move the remaining operator tooling (`ct-step-replay.py`, `ct-step-calibrate.py`, `ct-delta-profile.py`, and `tesla-octopus-regression.py`) off the legacy InfluxDB APIs and refresh its operator docs / verification evidence.
3. pi5data Phase 5: retire Telegraf's `influxdb_v2` output.
4. pi5data Phase 5: remove the Grafana v2 datasource.
5. pi5data Phase 5: stop and remove the InfluxDB v2 container.
6. pi5data Phase 5: archive the v2 data volume.

z2m-hub has no remaining repo-local dependency on InfluxDB. The remaining work is shared-platform completion and final decommission.

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
- service status still shows the original cutover start time on `pi5data` (`ExecMainPID=804858`, `ActiveEnterTimestamp=Sun 2026-04-12 12:05:26 BST`, `SubState=running`)
- closeout grep returned only the expected PostgreSQL connect log line from cutover, with no repeated credential/runtime failures
- live API checks returned sane `{"ok":true}` payloads for `/api/hot-water`, `/api/dhw/status`, and `/api/lights`
- recent `dhw` rows were still present in PostgreSQL/TimescaleDB, with the latest row at `2026-04-13 06:30:52.332111+00`
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

Confirm the API surface still returns sane hot-water, DHW-status, and lights payloads.

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
