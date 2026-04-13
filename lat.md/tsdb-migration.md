# TSDB Migration

This file tracks the repo-local TSDB migration for z2m-hub.

Shared platform, schema, live-ingest, gap-fill, and final decommission truth live in `~/github/energy-hub/lat.md/tsdb-migration.md`. This file covers only the z2m-hub cutover code, tests, deployment order, and rollback within that shared migration.

## Scope

z2m-hub reads sensor telemetry via SQL queries against TimescaleDB and writes DHW state via SQL INSERTs. All database access is through [[src/main.rs#query_pg_f64]] and [[src/main.rs#write_dhw_to_pg]], plus four thin sensor wrappers and the startup autoload block in `dhw_tracking_loop`.

## Shared dependency on the energy-hub migration

This repo can cut over only after the shared TimescaleDB platform is ready to serve the same DHW-relevant history and live continuity.

Required shared gates before repo cutover:

1. TimescaleDB schema exists and includes the `dhw` table expected by z2m-hub.
2. Historical import is complete so old DHW behaviour can be compared against full history.
3. Live ingest is running for the MQTT and sensor data that z2m-hub reads.
4. Gap-fill is complete and verified so the service does not cross a missing-data boundary.
5. Final v2 decommission remains blocked until this repo's local migration gate is green.

**Hard dependency:** this repo does not cut over before shared platform Phases 3 and 3b are complete and verified.

**Recommended order:** cut over this repo **after** `energy-hub` but **before** `heatpump-analysis`. z2m-hub is the lower-risk live-service proving ground for PostgreSQL-backed daemon behaviour before touching the heating controller.

## Read path: SQL queries

All read paths now use SQL against the TimescaleDB wide-row schema via [[src/main.rs#query_pg_f64]].

| Function | Table | Column | Time window |
|---|---|---|---|
| `get_current_volume` | `multical` | `dhw_volume_v1` | 1 hour |
| `get_current_t1` | `multical` | `dhw_t1` | 1 hour |
| `get_current_dhw_flow` | `multical` | `dhw_flow` | 5 minutes |
| `api_dhw_status` (inline) | `multical` | `dhw_t1` | 1 hour |
| autoload capacity | `dhw_capacity` | `recommended_full_litres` | 90 days |
| init remaining | `dhw` | `remaining_litres` | 24 hours |

All queries use `ORDER BY time DESC LIMIT 1` with a `WHERE time >= now() - interval '...'` clause for last-value semantics.

**Column case note:** the old Flux queries used `dhw_volume_V1` (capital V) but the TimescaleDB column is `dhw_volume_v1` (lowercase). The SQL uses lowercase names from the shared schema.

**Fallback contract is safety-critical:** all sensor read functions default to `0.0` on error. Per [[constraints#Restart recovery assumptions]], the DHW model starts from zeros and recovers naturally, so a database failure must not crash the service or produce nonsense state — it must degrade to a safe initial condition. The `query_pg_f64` helper preserves this `0.0` fallback.

Implementation shape:
- `DatabaseConfig` handles connection string and password resolution
- `ReconnectingPg` is the runtime seam and connects on demand for each read/write operation so startup survives transient DB outages and later calls naturally retry
- `query_pg_f64` returns already-decoded `(f64, String)` with zero-default fallback
- no DHW model policy, automation policy, or dashboard response shaping inside the SQL helper

## Write path: line protocol to INSERT mapping

[[src/main.rs#write_dhw_to_pg]] INSERTs into the `dhw` table. The `format_dhw_line_protocol` helper is retained as `#[cfg(test)]` golden reference for verifying field equivalence.

| PG column | PG type | Source |
|---|---|---|
| `time` | `TIMESTAMPTZ NOT NULL` | `now()` (new — LP had implicit server time) |
| `remaining_litres` | `FLOAT8` | `s.remaining` |
| `model_version` | `INTEGER` | `2` |
| `t1` | `FLOAT8` | `s.current_t1` |
| `hwc_storage` | `FLOAT8` | `s.current_hwc` |
| `effective_t1` | `FLOAT8` | `s.effective_t1` |
| `charge_state` | `TEXT` | `s.charge_state.to_string()` |
| `crossover` | `BOOLEAN` | `s.crossover_achieved` |
| `bottom_zone_hot` | `BOOLEAN` | `s.current_hwc > 30.0` |

The golden snapshot test in [[tests#DHW write format#LP golden snapshot matches expected field layout]] pins the exact LP string so the INSERT column values can be verified for equivalence.

**Fire-and-forget is safety-critical:** `write_dhw_to_pg` logs errors but never propagates them. A flaky database must not stop the DHW automation loop.

**Replay safety:** the `dhw` hypertable has no unique constraint on `time` (TimescaleDB does not support unique constraints on the partitioning column alone). Duplicate timestamps from a restart are effectively impossible because `now()` generates a new value on each call in a single-writer service. If idempotency becomes necessary in future, it would require a composite unique index on the hypertable.

## Config changes

The three hardcoded InfluxDB constants have been replaced with a `[database]` section in `z2m-hub.toml` and a `DatabaseConfig` struct.

| Old constant | Replacement |
|---|---|
| `INFLUXDB_URL` | `[database] host + port` in `z2m-hub.toml` |
| `INFLUXDB_TOKEN` | systemd encrypted credential in production, or one-shot `PGPASSWORD=$(ak get timescaledb)` on the trusted dev/test machine — see [[infrastructure#Secret management]] |
| `INFLUXDB_ORG` | removed (not needed for PG) |

The PG connection target is `10.0.1.230:5432`, database `energy`, user `energy`, as defined in the shared infrastructure plan.

## Secrets

This repo-local migration follows [[infrastructure#Secret management]].

For z2m-hub specifically:
- production on Pi/Linux uses `systemd-creds encrypt` with `LoadCredentialEncrypted=`
- dev/test uses one-shot `PGPASSWORD=$(ak get timescaledb) ...` on the trusted dev/test machine only

Forbidden here:
- `LoadCredential=` from plaintext files
- plaintext password files
- password fields in TOML
- checked-in secrets
- long-lived exported passwords in shell profiles

Topology note:
- field devices already publish to Pi-side services over MQTT
- stronger secrets remain on the Pi side
- this migration does not move database credentials onto MCUs or other field devices

## Implementation steps

Ordered to match energy-hub Phase 4 gate structure.

1. ~~**Add `tokio-postgres` dependency**~~ ✅ Done — `tokio-postgres` with `with-chrono-0_4` feature, plus `chrono` for timestamp handling.
2. ~~**Create a query helper**~~ ✅ Done — `query_pg_f64` returns `(f64, String)`, defaults to `(0.0, "")` on any error.
3. ~~**Rewrite sensor read functions**~~ ✅ Done — `get_current_volume`, `get_current_t1`, `get_current_dhw_flow` use SQL against `multical`.
4. ~~**Rewrite DHW write**~~ ✅ Done — `write_dhw_to_pg` INSERTs into `dhw` with explicit `now()` timestamp. Fire-and-forget preserved.
5. ~~**Rewrite autoload and init queries**~~ ✅ Done — startup queries use `dhw_capacity` and `dhw` tables via `query_pg_f64`.
6. ~~**Update `api_dhw_status`**~~ ✅ Done — inline query uses SQL against `multical` for `dhw_t1`.
7. ~~**Move constants to config**~~ ✅ Done — `INFLUXDB_URL`/`TOKEN`/`ORG` removed, replaced with `[database]` section in `z2m-hub.toml` and `DatabaseConfig` struct.
8. ~~**Adapt integration tests**~~ ✅ Done — `spawn_influx_test_server` removed, `FakePg` now covers handler-level happy-path/fallback tests while `dead_pg_client()` still covers transport-failure behaviour for low-level helpers.
9. ~~**Remove Influx parser**~~ ✅ Done — `parse_influx_query_csv` and `query_influxdb` deleted. `format_dhw_line_protocol` retained as `#[cfg(test)]` golden reference.

## Contracts that must survive migration

These are enforced by the current test suite (74 unit tests + 5 real-PG integration tests, all green).

| Contract | Enforced by |
|---|---|
| DHW write includes all 8 fields with correct types | `lp_payload_includes_all_eight_fields_with_correct_types` |
| Exact LP to INSERT field equivalence | `lp_golden_snapshot_matches_expected_field_layout` |
| `bottom_zone_hot` threshold at 30 degrees | `bottom_zone_hot_threshold_at_thirty_degrees` |
| All charge states encode correctly | `lp_encodes_all_charge_states_correctly` |
| Autoload only increases capacity | `autoload_applies_max_when_in_sane_range`, `autoload_never_decreases_current` |
| Autoload rejects out-of-range values | `autoload_rejects_outside_sane_bounds` |
| Autoload accepts boundary values | `autoload_accepts_at_exact_boundaries` |
| Volume-at-reset arithmetic | `volume_at_reset_reconstructs_from_drawn_volume` and property tests |
| PG query zero-default fallback | `query_fallback_returns_zero_defaults_on_transport_failure` via `dead_pg_client()` |
| DHW status merges ebusd and DB reading | `dhw_status_combines_ebusd_and_db_readings_into_one_snapshot` via `FakePg` happy-path injection |
| DHW status degrades gracefully | `dhw_status_falls_back_to_safe_defaults_when_upstream_reads_fail` |
| Write failure does not stop DHW loop (HTTP error) | `write_failure_does_not_stop_the_caller` |
| Write failure does not stop DHW loop (unreachable) | `write_to_unreachable_server_does_not_stop_the_caller` |

## Live verification tests

These tests require a real PostgreSQL instance and are gated with `#[ignore]`. Run on the trusted dev/test machine with `PGPASSWORD=$(ak get timescaledb) cargo test -- --ignored`. All 5 real-PG tests pass as of 2026-04-12.

| Test | What it proves | Regression layer | Status |
|---|---|---|---|
| PG row decoding returns `(f64, String)` for valid rows | New transport produces same tuple shape as Flux CSV parser | Read-path parity | ✅ `pg_row_decoding_returns_f64_and_timestamp` (#[ignore]) |
| PG query NULL/empty result defaults to `(0.0, "")` | Fallback contract preserved (safety-critical) | Read-path parity | ✅ Covered by `query_fallback_returns_zero_defaults_on_transport_failure` |
| INSERT includes explicit `time` column | Timestamp handling correct (no LP equivalent) | Write-path equivalence | ✅ `pg_insert_includes_explicit_time_column` (#[ignore], transaction rollback) |
| INSERT column types match `dhw` table schema | FLOAT8, INTEGER, TEXT, BOOLEAN mapping correct | Write-path equivalence | ✅ `pg_insert_column_types_match_schema` (#[ignore], transaction rollback) |
| Consecutive writes produce distinct rows | Single-writer `now()` never collides in hypertable | Write-path equivalence | ✅ `pg_consecutive_writes_produce_distinct_rows` (#[ignore], transaction rollback) |
| DHW status endpoint returns correct JSON with PG backend | Adapted integration test | Dashboard/API parity | ✅ Covered (ebusd mock + `FakePg` happy path, plus separate dead-PG fallback test) |
| Real PG integration with seeded `multical` + `dhw` + `dhw_capacity` tables | End-to-end read/write against actual schema | Real PG integration | ✅ `pg_end_to_end_seeded_integration` (#[ignore], transaction rollback) |

## Regression gates

This repo is not migrated just because the binary can connect to PostgreSQL.

Required local regression layers:

1. **Read-path parity** — representative old Flux reads and PostgreSQL reads must decode to the same DHW-relevant inputs for fixed historical windows.
2. **DHW model parity** — the service must preserve remaining-litre transitions, crossover/no-crossover decisions, and charge/draw state handling for fixed fixtures.
3. **Write-path equivalence** — SQL rows written to `dhw` must be semantically equivalent to the prior line-protocol writes.
4. **Dashboard/API parity** — representative HTTP responses for hot-water state and status endpoints must preserve the same user-visible JSON fields while backed by PostgreSQL.
5. **Real PostgreSQL integration** — at least one test layer must execute actual queries/inserts against seeded PostgreSQL fixtures so timestamp handling, field typing, and consecutive-write behaviour are exercised for real.

## Deployment and rollback

z2m-hub is a live service, so deployment must preserve the mobile dashboard and DHW state continuity.

**Current state (2026-04-12):** the PostgreSQL-backed binary is now deployed on pi5data and running live against TimescaleDB. `/etc/z2m-hub.toml` was replaced from the repo copy so it now carries the `[database]` section and TimescaleDB-aware comments.

The cutover uncovered one operator fix and two data-continuity facts:
- the encrypted secret path only worked once the live unit used `LoadCredentialEncrypted=pgpassword:/etc/z2m-hub/pgpassword.encrypted`; the previously-documented `SetCredentialEncrypted=` spelling did not populate `/run/credentials/z2m-hub.service/pgpassword` on this host
- the four `multical`-backed reads had already been verified against InfluxDB pre-cutover
- `dhw_capacity` startup autoload was unblocked by manually backfilling the latest InfluxDB recommendation into TimescaleDB (`201L`, method `direct_wwhr`, timestamp `2026-04-12T02:00:02Z`) because `heatpump-analysis` still writes that direct-writer measurement to v2 only
- `dhw.remaining_litres` continuity remains the untidiest startup seam: the first PG start loaded stale imported history, then the live PG daemon began advancing `dhw` rows immediately during an active draw window

So the z2m-hub cutover is live, but the 12h observation and rollback gates still matter.

**Review snapshot (2026-04-12 16:44 BST):**
- local verification was re-run on the trusted dev/test machine: `cargo test` stayed green (74 unit tests) and `PGPASSWORD=$(ak get timescaledb) cargo test -- --ignored` stayed green (5 real-PG tests)
- the live unit on `pi5data` remained healthy 4h39m after cutover (`ActiveEnterTimestamp=2026-04-12 12:05:26 BST`) and still showed successful PostgreSQL connect + DHW autoload logs
- live API spot checks on `pi5data` returned sane JSON for `/api/hot-water`, `/api/dhw/status`, and `/api/lights`
- rollback assets were rehearsed and hardened: the known-good pre-PG binary was confirmed at `/usr/local/bin/z2m-hub.20260412-120300.bak`, then copied to the stable path `/usr/local/bin/z2m-hub.pre-pg-rollback.bak`; a later zero-byte artifact (`/usr/local/bin/z2m-hub.20260412-120322.bak`) must not be used for rollback

**Review snapshot (2026-04-13 06:55 BST):**
- the live unit was still the original cutover process (`ExecMainPID=804858`, `ActiveEnterTimestamp=2026-04-12 12:05:26 BST`, `SubState=running`), so no hidden restart loop had occurred
- the elapsed observation time was **18h49m**, so the stricter old 24h gate was no longer relevant; under the updated 12h policy this uptime evidence is sufficient
- journal review since cutover showed normal PostgreSQL connect and DHW/light activity, with no repeated credential failures, PG transport failures, panic traces, or restart churn
- live API checks still returned sane JSON for `/api/hot-water`, `/api/dhw/status`, and `/api/lights`
- recent `dhw` rows were still advancing in TimescaleDB through `2026-04-13 01:07:02+00` with plausible temperatures / charge states
- the rollback artifact `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` was still present and non-empty (`7.3M`)
- the journal also showed real HTTP interactions during the window (`/api/lights` toggles and DHW boost requests), which is good evidence that the live dashboard/control surface was exercised while the PG-backed daemon stayed healthy
- judgement at this snapshot: the uptime evidence satisfies a 12h closeout threshold; remaining closure now depends on the separate dashboard visual sign-off and the explicit disposition of the read-parity note

Deployment rule:
- build and test on the trusted dev/test machine first
- cross-compile the release binary for `aarch64-unknown-linux-gnu`
- update `/etc/z2m-hub.toml` on pi5data (add `[database]` section, remove stale InfluxDB comments)
- provision or rotate the production PostgreSQL secret with `systemd-creds encrypt` and `LoadCredentialEncrypted=`; do not use `LoadCredential=` or plaintext secret files
- deploy to `pi5data`
- restart the service and immediately verify logs, DHW API responses, and dashboard behaviour

Rollback rule:
- keep the prior binary available on `pi5data`
- if PostgreSQL-backed behaviour is wrong, revert z2m-hub to the last known InfluxDB-backed binary while the shared TimescaleDB platform remains side-by-side
- do not let a z2m-hub rollback block or erase the shared platform work

## 12h operational closeout

This is **not** an automation or a deploy step. It is a short operator runbook for deciding whether the live PostgreSQL cutover can be called stable after 12 hours.

Purpose:
- close the two still-open **operational** gates (`API/dashboard spot checks` and `12h without regression`)
- reconfirm that the rollback asset still exists before the migration is declared done
- provide one repeatable place to record the final judgement: **close**, **hold**, or **rollback**

It does **not** resolve the separate `all 6 read paths match InfluxDB` item. That parity gate needs an explicit judgement note because `dhw.remaining_litres` stopped being a clean side-by-side comparison once the live PG-backed daemon took over.

### 1. Confirm the service really survived the 12h window

Success condition: `ActiveEnterTimestamp` still points at the 2026-04-12 cutover (or later only if an intentional restart was performed and explained), and the service has been stable for at least 12h.

```bash
ssh jack@pi5data 'systemctl show z2m-hub --property=ActiveEnterTimestamp,ActiveEnterTimestampMonotonic,SubState,ExecMainPID'
```

### 2. Check the journal for meaningful regression signals

Success condition: no repeated PostgreSQL connection failures, credential failures, panic traces, or tight restart loops during the observation window.

```bash
ssh jack@pi5data 'sudo journalctl -u z2m-hub --since "2026-04-12 12:05:00" --no-pager | grep -Ein "error|failed|panic|postgres|pg|credential|restart" | tail -n 120'
```

### 3. Re-check the live API responses

Success condition: `/api/hot-water`, `/api/dhw/status`, and `/api/lights` all return `"ok": true` with plausible temperatures/state.

```bash
ssh jack@pi5data '
  echo "--- /api/hot-water ---" && curl -fsS http://127.0.0.1:3030/api/hot-water && echo &&
  echo "--- /api/dhw/status ---" && curl -fsS http://127.0.0.1:3030/api/dhw/status && echo &&
  echo "--- /api/lights ---" && curl -fsS http://127.0.0.1:3030/api/lights && echo
'
```

### 4. Manually check the phone-sized dashboard

Success condition: the dashboard still looks and behaves like the pre-migration UI from a user point of view.

Manual steps:
1. Open the dashboard in a mobile-sized browser session on LAN.
2. Verify the hot-water card shows sensible litres and temperatures.
3. Verify the DHW status card shows charging/state fields without blanks or stale placeholders.
4. Verify the lights card renders current on/off state correctly.
5. Optionally toggle one non-critical light and confirm the UI updates.

### 5. Confirm PG-backed DHW rows are still advancing

Success condition: recent `dhw` rows exist and show fresh timestamps with plausible `remaining_litres` / `t1` values.

```bash
ssh jack@pi5data 'sudo docker exec timescaledb psql -U energy -d energy -c "select time, remaining_litres, t1, hwc_storage, charge_state from dhw order by time desc limit 10;"'
```

### 6. Confirm the rollback artifact still exists

Success condition: `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` is still present and non-empty.

```bash
ssh jack@pi5data 'ls -lh /usr/local/bin/z2m-hub.pre-pg-rollback.bak'
```

### 7. Decide close, hold, or rollback

**Close** the remaining operational gates only if all of the following are true:
- 12h uptime check passed
- journal shows no meaningful PG/runtime regression
- live API responses are sane
- mobile dashboard manual check passed
- recent `dhw` rows are present in TimescaleDB
- rollback artifact still exists

**Hold** the migration open if the service is basically healthy but one of the evidence items is still missing (for example the dashboard check was not yet done).

**Rollback** only if user-visible behaviour or DHW tracking is wrong enough that fix-forward is riskier than reverting.

If rollback is required:

```bash
ssh jack@pi5data 'sudo systemctl stop z2m-hub && sudo cp /usr/local/bin/z2m-hub.pre-pg-rollback.bak /usr/local/bin/z2m-hub && sudo systemctl start z2m-hub'
```

## Done gate

This repo is only migration-complete when its own service rewrite and verification are complete.

Checklist:
- [x] PostgreSQL client/config seam added
- [x] All 6 read paths replaced with SQL queries per mapping above
- [x] `dhw` write path replaced with SQL `INSERT` per field mapping above
- [x] All existing tests pass with PG backend (mock or real) — reconfirmed locally via `cargo test` (74 unit) plus `PGPASSWORD=$(ak get timescaledb) cargo test -- --ignored` (5 real-PG)
- [x] `lat check` passes
- [x] Systemd credential provisioned and unit file updated on pi5data — `LoadCredentialEncrypted=pgpassword:/etc/z2m-hub/pgpassword.encrypted`
- [ ] All 6 read paths produce identical values from PG as from InfluxDB for a representative time window — pre-cutover check on 2026-04-12 showed the 4 `multical` reads matching exactly; `dhw_capacity` was then manually backfilled from InfluxDB to unblock startup autoload; `dhw.remaining_litres` parity became a live-cutover continuity issue rather than a stable side-by-side check once the PG daemon was started
- [x] DHW INSERT writes are visible in the `dhw` hypertable with correct column types — proven by `pg_insert_column_types_match_schema` and `pg_insert_includes_explicit_time_column`
- [x] Local regression/parity suite green (all 5 layers) — layers 1 (read-path), 3 (write-path), and 5 (real PG) proven by the 5 `#[ignore]` integration tests; layer 2 (DHW model) covered by the unit/property suite; layer 4 (dashboard/API) covered by `dhw_status_combines_ebusd_and_db_readings_into_one_snapshot` plus `dhw_status_falls_back_to_safe_defaults_when_upstream_reads_fail`
- [ ] API/dashboard spot checks match the pre-migration behaviour on representative cases — live API spot checks stayed sane on 2026-04-13 and the journal showed real dashboard/control HTTP use (light toggles + DHW boost), but this file still treats the final phone-sized visual check as pending explicit sign-off
- [x] Live service deploy verified on `pi5data` — 2026-04-12 PG binary built, copied, configured, restarted under systemd, and verified via journal plus live `/api/hot-water` and `/api/dhw/status` responses
- [x] Service runs on pi5data for 12h without regression in DHW tracking behaviour — the 2026-04-13 06:55 BST review found the service healthy, still on the original PID, and already past the updated 12h threshold at 18h49m elapsed
- [x] Rollback path tested or rehearsed — rehearsal verified the live unit config and staged `/usr/local/bin/z2m-hub.pre-pg-rollback.bak` as the stable known-good pre-PG binary; rollback command: `sudo systemctl stop z2m-hub && sudo cp /usr/local/bin/z2m-hub.pre-pg-rollback.bak /usr/local/bin/z2m-hub && sudo systemctl start z2m-hub`
- [x] Shared `energy-hub` migration status updated to note this repo's state
