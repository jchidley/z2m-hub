# PostgreSQL Migration

This file tracks the repo-local migration from InfluxDB/Flux to PostgreSQL/TimescaleDB for z2m-hub.

Shared platform, schema, live-ingest, gap-fill, and final decommission truth live in `~/github/energy-hub/lat.md/timescaledb-migration.md`. This file covers only the code, tests, deployment order, and rollback for z2m-hub.

## Scope

z2m-hub is a direct writer to InfluxDB — it reads sensor telemetry via Flux queries and writes DHW state via line protocol. All InfluxDB access is in [[src/main.rs#query_influxdb]] and [[src/main.rs#write_dhw_to_influxdb]], plus four thin sensor wrappers and the startup autoload block in `dhw_tracking_loop`.

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

## Read path: Flux to SQL query mapping

Each Flux query becomes a SQL query against the TimescaleDB wide-row schema. The table and column names come from the shared schema in energy-hub's migration plan.

| Function | Current Flux filter | Target table | Target column | Time window |
|---|---|---|---|---|
| `get_current_volume` | `emon / dhw_volume_V1` | `multical` | `dhw_volume_v1` | 1 hour |
| `get_current_t1` | `emon / dhw_t1` | `multical` | `dhw_t1` | 1 hour |
| `get_current_dhw_flow` | `emon / dhw_flow` | `multical` | `dhw_flow` | 5 minutes |
| `api_dhw_status` (inline) | `emon / dhw_t1` | `multical` | `dhw_t1` | 1 hour |
| autoload capacity | `dhw_capacity / recommended_full_litres` | `dhw_capacity` | `recommended_full_litres` | 90 days |
| init remaining | `dhw / remaining_litres` | `dhw` | `remaining_litres` | 24 hours |

All queries use `last()` semantics — the SQL equivalent is `ORDER BY time DESC LIMIT 1` with a `WHERE time >= now() - interval '...'` clause.

**Column case gotcha:** the Flux queries use `dhw_volume_V1` (capital V) but the TimescaleDB column is `dhw_volume_v1` (lowercase). PostgreSQL lowercases unquoted identifiers, so the SQL must use the lowercase names from the shared schema, not the Flux field names.

**Fallback contract is safety-critical:** all sensor read functions default to `0.0` on error. Per [[constraints#Restart recovery assumptions]], the DHW model starts from zeros and recovers naturally, so a database failure must not crash the service or produce nonsense state — it must degrade to a safe initial condition. The PG query helper must preserve this `0.0` fallback.

Preferred shape for the replacement:
- one small PostgreSQL config and connection helper
- one query layer that returns already-decoded DHW/history inputs
- no DHW model policy, automation policy, or dashboard response shaping inside the SQL helper

## Write path: line protocol to INSERT mapping

The [[src/main.rs#format_dhw_line_protocol]] helper produces the LP payload. During migration this becomes an INSERT into the `dhw` table.

| LP field | LP type | PG column | PG type |
|---|---|---|---|
| `remaining_litres` | float (`{:.1}`) | `remaining_litres` | `FLOAT8` |
| `model_version` | integer (`2i`) | `model_version` | `INTEGER` |
| `t1` | float (`{:.2}`) | `t1` | `FLOAT8` |
| `hwc_storage` | float (`{:.2}`) | `hwc_storage` | `FLOAT8` |
| `effective_t1` | float (`{:.2}`) | `effective_t1` | `FLOAT8` |
| `charge_state` | string (`"..."`) | `charge_state` | `TEXT` |
| `crossover` | boolean | `crossover` | `BOOLEAN` |
| `bottom_zone_hot` | boolean | `bottom_zone_hot` | `BOOLEAN` |

The golden snapshot test in [[tests#DHW write format#LP golden snapshot matches expected field layout]] pins the exact LP string. The INSERT must produce identical column values.

**Timestamp handling:** the current LP write has no explicit timestamp — InfluxDB assigns server time automatically. The PG INSERT must supply an explicit `time TIMESTAMPTZ NOT NULL` value, either `now()` or `CURRENT_TIMESTAMP`. This is a new column in the INSERT that has no LP equivalent.

**Fire-and-forget is safety-critical:** `write_dhw_to_influxdb` logs errors but never propagates them. A flaky database must not stop the DHW automation loop. The PG write helper must preserve this contract.

If replay safety requires uniqueness or `ON CONFLICT` logic, keep it in the write helper rather than in the DHW model itself.

## Config changes

Three hardcoded constants must be replaced with a config-driven PG connection string.

| Constant | Current value | Replacement |
|---|---|---|
| `INFLUXDB_URL` | `http://localhost:8086` | `PG_URL` or `[database]` section in `z2m-hub.toml` |
| `INFLUXDB_TOKEN` | plaintext bearer token | PG password via env var or config file |
| `INFLUXDB_ORG` | `home` | not needed for PG |

The PG connection target is `10.0.1.230:5432`, database `energy`, user `energy`, as defined in the shared infrastructure plan.

## Implementation steps

Ordered to match energy-hub Phase 4 gate structure.

1. **Add `tokio-postgres` dependency** — add crate, create a connection pool or single connection at startup.
2. **Create a query helper** — replace `query_influxdb` with a PG equivalent returning `(f64, String)` for backwards compatibility. Default to `(0.0, "")` on error to preserve the fallback contract.
3. **Rewrite sensor read functions** — update `get_current_volume`, `get_current_t1`, `get_current_dhw_flow` to use SQL queries per the mapping above.
4. **Rewrite DHW write** — replace `write_dhw_to_influxdb` with an INSERT into the `dhw` table. Keep the fire-and-forget contract (log errors, don't propagate).
5. **Rewrite autoload and init queries** — update the two startup queries in `dhw_tracking_loop` to SQL.
6. **Update `api_dhw_status`** — replace the inline Flux query with the PG equivalent.
7. **Move constants to config** — replace `INFLUXDB_URL`/`TOKEN`/`ORG` with a PG connection string in `z2m-hub.toml`.
8. **Adapt integration tests** — replace `spawn_influx_test_server` with a PG mock or local PG fixture.
9. **Remove Influx parser** — once all callers use PG, delete `parse_influx_query_csv` and `query_influxdb`.

## Contracts that must survive migration

These are enforced by the pre-migration regression test suite.

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
| Parser zero-default fallback | `influx_parser_returns_zero_defaults_for_*` (contract transfers to PG helper) |
| DHW status merges ebusd and DB reading | `dhw_status_combines_ebusd_and_influx_readings_into_one_snapshot` |
| DHW status degrades gracefully | `dhw_status_falls_back_to_safe_defaults_when_upstream_reads_fail` |
| Write failure does not stop DHW loop (HTTP error) | `write_failure_does_not_stop_the_caller` |
| Write failure does not stop DHW loop (unreachable) | `write_to_unreachable_server_does_not_stop_the_caller` |

## Tests to write during migration

These tests require PG code to exist and cannot be written pre-migration.

| Test | What it proves | Regression layer |
|---|---|---|
| PG row decoding returns `(f64, String)` for valid rows | New transport produces same tuple shape as Flux CSV parser | Read-path parity |
| PG query NULL/empty result defaults to `(0.0, "")` | Fallback contract preserved (safety-critical) | Read-path parity |
| INSERT includes explicit `time` column | Timestamp handling correct (no LP equivalent) | Write-path equivalence |
| INSERT column types match `dhw` table schema | FLOAT8, INTEGER, TEXT, BOOLEAN mapping correct | Write-path equivalence |
| INSERT `ON CONFLICT` handles duplicate timestamps | Restart mid-cycle does not lose state | Write-path equivalence |
| DHW status endpoint returns correct JSON with PG backend | Adapted integration test | Dashboard/API parity |
| Real PG integration with seeded `multical` + `dhw` + `dhw_capacity` tables | End-to-end read/write against actual schema | Real PG integration |

## Regression gates

This repo is not migrated just because the binary can connect to PostgreSQL.

Required local regression layers:

1. **Read-path parity** — representative old Flux reads and PostgreSQL reads must decode to the same DHW-relevant inputs for fixed historical windows.
2. **DHW model parity** — the service must preserve remaining-litre transitions, crossover/no-crossover decisions, and charge/draw state handling for fixed fixtures.
3. **Write-path equivalence** — SQL rows written to `dhw` must be semantically equivalent to the prior line-protocol writes.
4. **Dashboard/API parity** — representative HTTP responses for hot-water state and status endpoints must preserve the same user-visible JSON fields while backed by PostgreSQL.
5. **Real PostgreSQL integration** — at least one test layer must execute actual queries/inserts against seeded PostgreSQL fixtures so timestamp handling, field typing, and any conflict behaviour are exercised for real.

## Deployment and rollback

z2m-hub is a live service, so deployment must preserve the mobile dashboard and DHW state continuity.

Deployment rule:
- build and test on dev first
- cross-compile the release binary for `aarch64-unknown-linux-gnu`
- deploy to `pi5data`
- restart the service and immediately verify logs, DHW API responses, and dashboard behaviour

Rollback rule:
- keep the prior binary available on `pi5data`
- if PostgreSQL-backed behaviour is wrong, revert z2m-hub to the last known InfluxDB-backed binary while the shared TimescaleDB platform remains side-by-side
- do not let a z2m-hub rollback block or erase the shared platform work

## Done gate

This repo is only migration-complete when its own service rewrite and verification are complete.

Checklist:
- [ ] PostgreSQL client/config seam added
- [ ] All 6 read paths replaced with SQL queries per mapping above
- [ ] `dhw` write path replaced with SQL `INSERT` per field mapping above
- [ ] All 6 read paths produce identical values from PG as from InfluxDB for a representative time window
- [ ] DHW INSERT writes are visible in the `dhw` hypertable with correct column types
- [ ] Local regression/parity suite green (all 5 layers)
- [ ] All existing tests pass with PG backend (mock or real)
- [ ] API/dashboard spot checks match the pre-migration behaviour on representative cases
- [ ] `lat check` passes
- [ ] Live service deploy verified on `pi5data`
- [ ] Service runs on pi5data for 24h without regression in DHW tracking behaviour
- [ ] Rollback path tested or rehearsed
- [ ] Shared `energy-hub` migration status updated to note this repo's state
