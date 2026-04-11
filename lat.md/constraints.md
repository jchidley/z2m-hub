# Constraints

This file captures the operational boundaries that should shape code changes and deployment choices.

## Operational constraints

A few constraints are structural enough that code changes should treat them as boundaries rather than convenience choices.

## LAN-only protocols

All runtime integrations are intentionally plain LAN protocols with no TLS layer.

The service uses `ws://` for Zigbee2MQTT, `http://` for InfluxDB and heating-mvp, and raw TCP for ebusd. This avoids OpenSSL and certificate complexity in a private LAN deployment and keeps ARM cross-compilation simpler.

## No subprocess integration

Runtime integrations must not shell out to local commands for core control paths.

The project previously moved away from subprocess-based ebusd access because `tokio::process` pulled in a GLIBC requirement newer than the target Pi host. Use direct TCP or HTTP integrations instead of spawning local helpers from the Rust service.

## Single-binary deployment

The server, UI, automations, and protocol clients are expected to remain deployable as one binary.

The dashboard is embedded directly and the main process owns every route and background loop. This constraint is about operational simplicity rather than purity.

## Small-screen dashboard target

The dashboard must remain usable on a 320px-wide phone without any internet dependency.

The UI is tuned for an iPhone SE-sized viewport and polls the local API for state. Avoid design changes that assume wide layouts, external assets, or cloud services.

## Restart recovery assumptions

The service accepts in-memory state loss on restart only because each subsystem has a recovery path.

Zigbee device state is rebuilt from retained WebSocket messages, DHW remaining litres are reloaded from InfluxDB, and motion timers simply disappear if the process restarts mid-window. New features should define an equally clear recovery story.
