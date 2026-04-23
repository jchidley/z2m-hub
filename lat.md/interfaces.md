# Interfaces

This file defines the external protocols and endpoint contracts that z2m-hub depends on at runtime.

## External interfaces

z2m-hub is defined by a small set of LAN protocols: HTTP for the dashboard, WebSocket for Zigbee, raw TCP for eBUS, and PostgreSQL wire protocol for TimescaleDB.

## HTTP API

The embedded dashboard and any LAN clients use the axum routes registered by the server.

Key endpoints are:

- `GET /` for the embedded mobile dashboard
- `GET /api/hot-water` for the current DHW estimate
- `POST /api/dhw/boost` for a one-shot DHW charge request
- `GET /api/dhw/status` for a live ebusd and sensor snapshot
- `GET /api/lights` plus `POST /api/lights/{name}/{on|off|toggle}` for light state and commands
- `GET/POST /api/heating/...` for proxy calls to heating-mvp

The light toggle API is optimistic: it returns the intended new state before Zigbee2MQTT confirms the device state.

`GET /api/hot-water` now has an explicit stale/unknown branch. When the required Multical-backed volume/T1 snapshot is fresh, it returns the live DHW estimate fields. When that telemetry is stale or missing, it returns `multical_stale = true`, preserves still-live eBUS-backed `hwc_storage`, switches `charge_state` to `"unknown"`, nulls the Multical-derived fields (`remaining_litres`, `effective_t1`, `t1`, `crossover_achieved`), and carries the latest known Multical timestamp when one can still be found in PostgreSQL history.

## Zigbee2MQTT WebSocket protocol

Zigbee device control uses the Zigbee2MQTT WebSocket API at `ws://emonpi:8080/api`.

The service exchanges JSON messages shaped like `{"topic": "<topic>", "payload": <object>}`. Zigbee2MQTT is expected to replay retained device state on connect, accept command topics as `<device>/set`, and keep bridge metadata on `bridge/...` topics.

## eBUS request contract

Heat-pump control uses one TCP connection per command to ebusd on `localhost:8888`.

The server writes `command + "\n"`, shuts down the write side, and reads until EOF. The code assumes ebusd remains line-based and request/response oriented. Commands used today include `read -f -c 700 HwcSFMode`, `read -f -c hmu Status01`, `read -f -c 700 HwcStorageTemp`, and `write -c 700 HwcSFMode load`.

## PostgreSQL read and write contract

TimescaleDB on `10.0.1.230:5432` is both the source of DHW telemetry and the persistence layer for the current estimate.

The service queries `multical`, `dhw`, and `dhw_capacity` tables via `tokio-postgres`, using `ORDER BY time DESC LIMIT 1` for last-value semantics. Reads return `(f64, String)` with a `(0.0, "")` zero-default on any error. The DHW stale/unknown path also queries the latest historical Multical row without a recency window when it needs a last-known timestamp for the dashboard stale notice. Writes INSERT into the `dhw` table with an explicit `now()` timestamp, with a pure `dhw_write_row` helper mapping `DhwState` into the eight typed payload columns before the SQL call. The loop does not persist every 10-second polling tick: it writes on DHW state-change boundaries that request persistence (`charge` end, draw volume advance, and draw end), so fresh `multical` rows may continue while `dhw` stays unchanged during a steady no-draw/no-completion period. The runtime PostgreSQL seam connects on demand per read/write operation rather than holding one long-lived shared client, so startup does not fail just because TimescaleDB is briefly unavailable and later calls naturally retry after a disconnect. The code depends on the shared TimescaleDB schema column naming and on the Multical volume register staying monotonic.

**TimescaleDB hypertable constraint:** the `dhw` table is a hypertable partitioned by `time`. TimescaleDB does not support unique constraints on the partitioning column alone, so `ON CONFLICT (time)` is not available. The write path uses plain INSERT — duplicate timestamps are impossible in practice because `now()` is evaluated server-side for each single-writer call.

## Heating MVP proxy

Heating mode changes are delegated to a separate service rather than implemented in this binary.

`HEATING_MVP_URL` points at `http://127.0.0.1:3031`. z2m-hub forwards status, mode changes, away payloads, and emergency kill requests to that service and relays its JSON response back to the dashboard.
