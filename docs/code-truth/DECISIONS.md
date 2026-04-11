# Decisions

Code-derived decision log. For canonical constraints, see [`lat.md/constraints.md`](../../lat.md/constraints.md).

## Structural (don't touch without good reason)

These decisions are also captured as constraints in [`lat.md/constraints.md`](../../lat.md/constraints.md):

- **No MQTT dependency** — Z2M via WebSocket, ebusd via TCP. MQTT is Telegraf's concern.
- **No TLS** — LAN-only. `reqwest` uses `rustls` to avoid OpenSSL cross-compilation.
- **No `tokio::process`** — caused GLIBC_2.39 dependency; use direct TCP/HTTP instead.
- **Single-file architecture** — everything in `src/main.rs` with section labels.
- **Embedded HTML dashboard** — `HOME_PAGE` const, no build step, single binary.
- **InfluxDB token hardcoded** — acceptable for LAN-only deployment.
- **LIGHTS vs MOTION_LIGHTS split** — see [`lat.md/automations.md`](../../lat.md/automations.md).

## Pragmatic (could change)

Behaviour details are canonical in `lat.md/`:

- **Illuminance frozen while lights on** — see [`lat.md/automations.md`](../../lat.md/automations.md)
- **5-minute motion timeout** — see [`lat.md/automations.md`](../../lat.md/automations.md)
- **Manual off cancels and suppresses** — see [`lat.md/automations.md`](../../lat.md/automations.md)
- **DHW boost and tracking model** — see [`lat.md/dhw.md`](../../lat.md/dhw.md)
- **Draw tracking runs during charging** — see [`lat.md/dhw.md`](../../lat.md/dhw.md)
- **Optimistic light toggle** — see [`lat.md/interfaces.md`](../../lat.md/interfaces.md)
- **Device state lost on restart** — see [`lat.md/constraints.md`](../../lat.md/constraints.md)

## Open Questions

- **Per-sensor off-timers?** Currently both motion sensors share one timer. If landing_motion triggers, then hall_motion retriggers 30s later, the timer resets to 5min from the hall event. Seems fine but could be surprising.
- **Dashboard for more devices?** Temperature sensors, kitchen switch, smart plugs are tracked by Z2M and logged by Telegraf but not exposed in the dashboard.
- **MQTT for emon data?** Currently reads T1 and volume from InfluxDB (Flux query, CSV parse). Subscribing to `emon/multical/dhw_t1` via MQTT would give real-time data without query overhead. But adds MQTT as a dependency.
- **top_landing motion link?** Currently manual-only. Could be linked to a future top_landing_motion sensor with its own illuminance threshold.
