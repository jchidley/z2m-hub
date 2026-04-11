# Interfaces

This file defines the external protocols and endpoint contracts that z2m-hub depends on at runtime.

## External interfaces

z2m-hub is defined by a small set of LAN protocols: HTTP for the dashboard, WebSocket for Zigbee, raw TCP for eBUS, and HTTP for InfluxDB.

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

## Zigbee2MQTT WebSocket protocol

Zigbee device control uses the Zigbee2MQTT WebSocket API at `ws://emonpi:8080/api`.

The service exchanges JSON messages shaped like `{"topic": "<topic>", "payload": <object>}`. Zigbee2MQTT is expected to replay retained device state on connect, accept command topics as `<device>/set`, and keep bridge metadata on `bridge/...` topics.

## eBUS request contract

Heat-pump control uses one TCP connection per command to ebusd on `localhost:8888`.

The server writes `command + "\n"`, shuts down the write side, and reads until EOF. The code assumes ebusd remains line-based and request/response oriented. Commands used today include `read -f -c 700 HwcSFMode`, `read -f -c hmu Status01`, `read -f -c 700 HwcStorageTemp`, and `write -c 700 HwcSFMode load`.

## InfluxDB read and write contract

InfluxDB is both the source of DHW telemetry and the persistence layer for the current estimate.

The service sends Flux queries to `http://localhost:8086/api/v2/query`, parses CSV looking for `_value` and `_time`, and writes line protocol back to the `energy` bucket. The code depends on the current column naming and on the Multical volume register staying monotonic.

## Heating MVP proxy

Heating mode changes are delegated to a separate service rather than implemented in this binary.

`HEATING_MVP_URL` points at `http://127.0.0.1:3031`. z2m-hub forwards status, mode changes, away payloads, and emergency kill requests to that service and relays its JSON response back to the dashboard.
