# Automations

This file records the current automation rules that z2m-hub applies to named Zigbee devices.

## Motion lighting automation

Motion events can switch on the hall and landing lights when the relevant sensor reports darkness.

Current behaviour:

- Sensors: `landing_motion` and `hall_motion`
- Triggered lights: `landing` and `hall`
- Darkness threshold: `<= 15 lx` per sensor
- Auto-off delay: 300 seconds
- Re-trigger: any new motion while active resets the timer

The app deliberately treats both sensors as feeding one shared timer for the motion-linked lights.

## Dashboard light set

The dashboard exposes a superset of controllable lights that is not identical to the motion set.

The dashboard controls `landing`, `hall`, and `top_landing`, while motion automation only controls `landing` and `hall`. This split means `top_landing` has a dashboard toggle but is never triggered by motion.

## Illuminance gating

Illuminance is sampled only while the motion lights are off so the automation does not learn its own light output.

The cached lux value updates only when the motion timer is idle. This avoids the switched-on lights raising measured lux by roughly 5–6 lx and preventing later triggers during the same occupancy period.

## Manual override suppression

A manual OFF during an active motion timer cancels the automation and suppresses retriggering for one full timeout window.

When a motion-linked light reports `OFF` while the timer is active, the timer is cleared and suppression is set for another 300 seconds. Motion events during that window are ignored so the automation does not fight the user.

## Timer-driven off behaviour

Automatic switch-off uses a dedicated loop rather than per-event spawned tasks.

The timer loop ticks every second, clears the timer before sending OFF commands, and then turns off every motion light. Clearing the timer first prevents the later Zigbee confirmation from being mistaken for a manual override.
