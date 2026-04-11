# Architecture

Code-derived data-flow reference. For canonical architecture, see [`lat.md/architecture.md`](../../lat.md/architecture.md).

## Runtime Structure

`main()` launches four concurrent tasks via `tokio::select!`:

```
tokio::select! {
    timer_loop           — 1s tick, checks light-off timers
    z2m_connection_loop  — WebSocket to Z2M, auto-reconnect
    dhw_tracking_loop    — 10s tick, polls ebusd, tracks DHW
    axum::serve          — HTTP server on port 3030
}
```

All tasks share state through `Arc<Mutex<T>>`. See [`lat.md/architecture.md`](../../lat.md/architecture.md) for the shared-state model.

## External Dependencies (runtime)

| System | Protocol | Direction | What for |
|--------|----------|-----------|----------|
| Z2M (emonpi:8080) | WebSocket | Bidirectional | Device state + commands |
| ebusd (localhost:8888) | Raw TCP | Request/response | Heat pump control |
| InfluxDB (localhost:8086) | HTTP | Read + write | DHW T1 temp, volume, remaining_litres |

For implicit contracts and endpoint details, see [`lat.md/interfaces.md`](../../lat.md/interfaces.md).

## Data Flow: Motion → Lights

```
Aqara sensor (Zigbee) → Z2M → WebSocket message
  → handle_z2m_message()
    → topic in MOTION_SENSORS? → illuminance ≤ threshold?
    → send ON to each MOTION_LIGHT via broadcast channel
    → set lights_off_at = now + 300s
```

For override suppression and illuminance gating rules, see [`lat.md/automations.md`](../../lat.md/automations.md).

## Data Flow: DHW Tracking

```
dhw_tracking_loop (every 10s):
  → is_charging(): ebusd HwcSFMode + Status01
  → charge start → snapshot t1, begin crossover detection
  → charge end → crossover? full : gap-based thermocline model
  → draw detection → volume subtraction + temperature caps
  → standby → effective_t1 decay at 0.25°C/h
```

For the full physics model, crossover rules, and draw-tracking caps, see [`lat.md/dhw.md`](../../lat.md/dhw.md).

## Data Flow: HTTP → Light Toggle

```
POST /api/lights/landing/toggle
  → read z2m_state["landing"]["state"]
  → send Z2mMessage via broadcast channel
  → return optimistic response (before Z2M confirms)
```

For the full API surface and proxy contracts, see [`lat.md/interfaces.md`](../../lat.md/interfaces.md).
