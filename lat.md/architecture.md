# Architecture

This file describes the runtime shape, shared state, and ownership boundaries of the z2m-hub process.

## Runtime structure

z2m-hub runs as one Rust process that serves HTTP while maintaining Zigbee and DHW control loops.

### Single binary service

The application is one Rust binary that owns routing, UI, automations, and all protocol clients.

`main()` launches four concurrent responsibilities with `tokio::select!`:

- HTTP server on port 3030
- Zigbee2MQTT WebSocket connection management
- motion-light timer loop
- DHW tracking loop

This keeps deployment simple: one binary, one config file, and no local sidecar processes beyond the external systems it talks to.

### Shared state model

The runtime shares mutable state across loops through a small number of mutex-protected state objects.

Key shared state is:

- `AutomationState` for `lights_off_at`, `suppressed_until`, and cached illuminance
- `DhwState` for remaining litres, charge tracking, and cached temperatures
- `z2m_state` as a topic → payload cache for retained device state
- `broadcast::Sender<Z2mMessage>` as the command bus from APIs and automations to the WebSocket writer

The DHW loop now keeps more of its state transitions in small pure helpers such as charge completion and draw tracking, so the async polling shell stays thin while the high-value litre/temperature rules are unit-testable in isolation.

Some interface glue also uses small pure helpers for deterministic parsing and response shaping, such as heating-proxy JSON wrapping, so the LAN client shells remain thin and auditable without introducing subprocesses or duplicate policy logic.

The PostgreSQL persistence layer also follows this pattern: `ReconnectingPg` is the thin runtime seam that connects on demand, [[src/main.rs#query_pg_f64]] handles all read queries with zero-default fallback, `dhw_write_row` is the pure helper that maps `DhwState` into the `dhw` insert payload, [[src/main.rs#write_dhw_to_pg]] handles fire-and-forget writes, [[src/main.rs#apply_standby_decay_for_elapsed_hours]] keeps standby-decay thresholds deterministic and unit-testable, [[src/main.rs#apply_autoload]] decides startup capacity upgrades, [[src/main.rs#reconstruct_volume_at_reset]] recovers volume-register state on restart, `apply_startup_recovery` hydrates `DhwState` from persisted litres plus live startup readings before the polling loop begins, and `apply_live_dhw_tick` owns the per-tick charge/draw transition policy while the async loop stays responsible for sensor polling and PostgreSQL side effects.

The design assumes low enough contention that coarse mutexes are acceptable.

### Routing and UI ownership

HTTP routes and the mobile UI are served directly from the same process that owns automation state.

The root page is an embedded HTML string and the API surface includes hot-water state, DHW boost, lighting control, and a heating proxy. There is no separate frontend build, static asset pipeline, or SPA.

### Zigbee command path

All Zigbee commands flow through one broadcast channel and one active WebSocket writer.

HTTP handlers and automation logic both publish `Z2mMessage` values onto the same channel. The WebSocket loop serialises those messages to Zigbee2MQTT, which keeps device control and automation output on one consistent path.

### State cache ownership

Retained and live Zigbee device payloads are cached centrally so the dashboard can make immediate decisions.

The service stores any non-bridge topic without a slash into `z2m_state`. The light APIs read this cache to decide current state, which is why the app depends on Zigbee2MQTT replaying retained state when the WebSocket connects.

## Heating integration boundary

Heating controls are proxied rather than reimplemented inside z2m-hub.

z2m-hub owns the dashboard surface for heating controls, but the heating policy engine lives in a separate local service. See [[interfaces#Heating MVP proxy]].
