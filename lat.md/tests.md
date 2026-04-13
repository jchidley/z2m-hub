---
lat:
  require-code-mention: true
---
# Tests

This file records durable test specifications for the highest-value DHW and motion-lighting invariants in z2m-hub.

Referenced test headings should stay plain and stable so `@lat:` comments in Rust tests do not break on cosmetic punctuation changes.

Keep headings in simple words, avoid inline code spans or syntax fragments in heading text, and put literal protocol or threshold examples in the paragraph body instead.

## DHW no crossover

These specs cover the gap-based completion model that runs when a DHW charge ends before crossover.

### Dissolved thermocline resets to full

A gap below the dissolved threshold must restore `remaining` to `full_litres` and mark the state as full because the thermocline has effectively disappeared.

### Sharp thermocline preserves prior remaining

A gap above the sharp threshold must keep the pre-charge remaining litres because the charge did not meaningfully homogenise the cylinder.

### Intermediate gap interpolates between prior and full

A gap between the two thresholds must interpolate smoothly from the prior remaining litres toward full rather than snapping to either extreme.

### Dissolved boundary stays on interpolation path

A gap exactly equal to `gap_dissolved` must not take the strict dissolved branch; it should still flow through the interpolation path and preserve the partial-state label.

### Sharp boundary keeps prior litres without full reset

A gap exactly equal to `gap_sharp` must not take the strict sharp branch; it should produce the unchanged litre result without claiming a full reset.

### Dissolved gap can recover from zero remaining

A dissolved-gap completion must refill the estimate even if prior remaining litres were already zero.

### Remaining stays within zero and full capacity

For sane configs and prior states, the no-crossover model must never produce negative litres or exceed the configured full capacity.

### Larger temperature gaps never increase remaining litres

With the same prior state and configuration, making the post-charge temperature gap larger must not increase the estimated remaining litres.

## DHW charge completion

These specs cover the branch that runs when a charge ends and decides whether crossover made the cylinder effectively full.

### Crossover completion restores full litres and full state

When a charge has achieved crossover, charge completion must restore `remaining` to `full_litres`, set the state to full, and snapshot the end-of-charge temperatures for later standby handling.

### Charge completion without crossover falls back to the gap model

When crossover was not achieved, charge completion must delegate to the no-crossover gap model instead of forcing the tank to full.

## DHW draw tracking

These specs cover the logic that subtracts drawn volume and then applies temperature-based safety caps during a draw.

### Volume draw alone reduces remaining litres

With no temperature crash signals, a draw must still reduce remaining litres according to the cumulative volume drawn since the last reset.

### Hwc storage crash caps remaining at the upper sensor volume

A first HwcStorage crash beyond the configured threshold must cap remaining litres at the configured volume above the HwcStorage sensor.

### A repeated Hwc storage crash does not reapply the cap logic

Once a HwcStorage crash has already been recorded for the active draw, later loop iterations must not keep re-triggering the first-crash cap branch.

### A moderate T1 drop caps remaining at twenty litres

A T1 drop above 0.5°C but not above 1.5°C must cap remaining litres at 20L to reflect a descending thermocline at draw-off height.

### A severe T1 drop forces remaining to zero

A T1 drop above 1.5°C must force remaining litres to zero because useful hot water is exhausted at draw-off level.

### A T1 drop exactly at one point five degrees stays on the twenty litre cap

A T1 drop exactly equal to 1.5°C must not take the strict zero-litres branch; it should remain on the weaker 20L cap path.

### A severe T1 drop overrides a Hwc crash cap

When both the HwcStorage crash and severe T1-drop conditions happen in the same draw update, the stronger zero-litres T1 rule must win.

## DHW standby decay

These specs cover how post-charge standby cooling adjusts effective top temperature without inventing or deleting volume.

### No charge end time leaves state unchanged

When there is no recorded charge end time, standby decay must be a no-op so startup and uninitialised states are not mutated accidentally.

### Two hour decay cools top temperature and marks standby

Two hours of standby should reduce `effective_t1` according to the configured decay rate and transition a non-charging state into standby.

### Cooling below reduced temperature marks standby

Once standby cooling drops below the reduced-temperature threshold, the state must become standby even if the elapsed-time rule alone would already do so later.

### Short standby keeps full state

A recent charge that is still above the reduced-temperature threshold must preserve the full state rather than switching to standby early.

### Decay never overwrites active charging states

The elapsed-time standby rule must not overwrite active charging labels, because that would misreport a live charge as idle.

### Effective top temperature never rises during standby

Standby decay must be monotonic: later elapsed times may keep or lower `effective_t1`, but they must never increase it.

## Motion lighting automation

These specs cover the shared-timer hall and landing motion automation described in the automation docs.

### Dark motion turns on both motion lights and arms the timer

A dark occupancy event from either motion sensor must turn on both motion-linked lights, schedule the shared off timer, and cache the device payload for dashboard reads.

### Motion at the darkness threshold still triggers the lights

The darkness gate is inclusive at the configured threshold, so a sensor report exactly on the limit must still activate the motion lights.

### Motion during an active timer refreshes the deadline

When either motion sensor fires while the shared timer is already active, the automation must extend the off deadline instead of sending duplicate ON commands or leaving the old expiry in place.

### Bright motion only refreshes cached lux and does not switch lights

A bright occupancy event must update cached illuminance while the timer is idle but avoid turning the motion lights on.

### Manual off cancels the timer and suppresses retriggering

When a motion-linked light reports `OFF` during an active timer, the automation must clear the timer and suppress fresh motion triggers for one timeout window.

### Active suppression blocks dark motion retriggering

While suppression is still active, dark occupancy events must not turn the motion lights back on.

### Expired suppression is cleared before a fresh dark motion trigger

Once the suppression deadline has passed, the next dark occupancy event must clear the stale suppression marker and behave like a normal trigger again.

### Timer expiry off does not create manual suppression

When the shared timer expires, the timer loop must clear `lights_off_at` before publishing OFF commands so the later Zigbee OFF confirmations are not mistaken for a user manual override and do not arm suppression.

## HTTP API

These specs cover the lightweight handler logic that translates cached state into dashboard responses and optimistic Zigbee commands.

### Light toggle uses cached ON state to send OFF

When the cached Zigbee state says a light is already on, the toggle handler must publish an OFF command and return OFF in the optimistic response.

### Light toggle assumes OFF when cache is missing

When no retained Zigbee state exists for a known light, the toggle handler must treat it as off and optimistically send ON.

### Unknown light commands fail without publishing Zigbee traffic

Handlers for light control must reject names outside the configured light list and avoid sending any Zigbee command when they do.

### Light on and off publish the requested state for known lights

For a configured light name, the explicit on and off handlers must publish exactly one Zigbee command to `{light}/set` and return the same optimistic state in the HTTP response.

### Lights state reports missing cache entries as off

The dashboard lights-state endpoint must include every configured light and mark ones without cached state as off rather than omitting them or treating them as on.

### Hot water endpoint returns the current DHW snapshot

The hot-water endpoint must mirror the in-memory DHW snapshot fields needed by the dashboard, including litres, temperatures, charge state, and crossover flag.

### DHW status combines ebusd and database readings into one snapshot

The live DHW-status endpoint must merge ebusd mode/status reads with the latest PostgreSQL `dhw_t1` reading into one dashboard JSON snapshot.

The happy-path contract should be proven without needing a live database by injecting a fake PostgreSQL seam that returns a non-zero `dhw_t1` value.

### DHW status falls back to safe defaults when upstream reads fail

If ebusd commands or the PostgreSQL `dhw_t1` query fail or return malformed values, the live DHW-status endpoint must still return `{ "ok": true }` with defaulted dashboard fields rather than failing the whole request.

### DHW boost returns ok true only for done

The one-shot DHW boost endpoint must return `{ "ok": true }` only when ebusd replies with `done` to `write -c 700 HwcSFMode load`.

### DHW boost unexpected replies include ok false and the reply text

If ebusd accepts the boost command but returns an unexpected reply string, the endpoint must return `{ "ok": false, "error": ... }` carrying that reply so the dashboard gets a stable failure shape.

### Retained slashless Zigbee topics are cached for dashboard decisions

When Zigbee2MQTT delivers a non-bridge topic without a slash, z2m-hub must cache that payload in `z2m_state` so later dashboard reads and toggle decisions can use it immediately.

### Bridge and nested Zigbee topics are not cached as device state

Bridge topics and slash-containing topics such as command acknowledgements must not be inserted into the retained device-state cache because they are not dashboard device snapshots.

## DHW write format

These specs pin the line-protocol field set and type encoding as a golden reference so the PostgreSQL INSERT can be verified for field equivalence.

### LP golden snapshot matches expected field layout

The exact LP string from `test_state()` defaults must match a pinned golden value so the migration can map each LP field to its INSERT column.

### LP payload includes all eight fields with correct types

The formatted line must contain all eight DHW fields with correct LP type encoding: floats, integer suffix, quoted string, and booleans.

### Bottom zone hot threshold at thirty degrees

`bottom_zone_hot` must be `true` when `current_hwc > 30.0` and `false` at or below 30.0, preserving the inline business rule during migration.

### LP encodes all charge states correctly

Every `DhwChargeState` variant (`full`, `partial`, `standby`, `charging_below`, `charging_uniform`) must appear as a quoted string in the line protocol.

### Write failure does not stop the caller

When the PostgreSQL connection has failed, the write function must log the error and return normally so the DHW tracking loop continues.

### Write to unreachable server does not stop the caller

When the PostgreSQL server is unreachable (dead connection), the write function must log the transport error and return normally so the DHW tracking loop continues.

## DHW autoload

These specs cover the pure bounds logic that decides whether a database-recommended capacity value should upgrade the runtime full_litres.

### Autoload applies max of config and recommended when in sane range

When the recommended value is within `[full_litres_min, full_litres_max]`, the result must be `max(current, recommended)` so capacity can only increase.

### Autoload rejects values outside sane bounds

When the recommended value is above `full_litres_max` or below `full_litres_min`, the function must return `None` so the caller ignores it.

### Autoload accepts values at exact boundaries

Recommended values exactly equal to `full_litres_min` or `full_litres_max` must be accepted, not rejected.

### Autoload never decreases current capacity

For any in-range recommended value, the autoload result must be greater than or equal to the current value, ensuring capacity can only increase at startup.

## DHW startup

These specs cover the pure arithmetic that reconstructs Multical volume-register state on restart from persisted remaining litres.

### Volume at reset reconstructs from drawn volume

Given full_litres, remaining, and the current Multical register, `volume_at_reset` must equal `volume_now - (full_litres - remaining)` so the draw tracker resumes correctly.

### Volume at reset at full capacity gives current volume

When remaining equals full capacity (nothing drawn), `volume_at_reset` must equal the current register reading.

### Volume at reset clamps negative drawn to zero

When remaining exceeds full capacity (defensive edge case), the already-drawn amount must be clamped to zero so `volume_at_reset` never exceeds the current register.

### Volume at reset increases with register reading

For fixed full_litres and remaining, a higher Multical register reading must always produce a higher or equal volume_at_reset.

### Volume at reset increases with remaining litres

For fixed full_litres and volume_now, higher remaining litres (less drawn) must produce a higher or equal volume_at_reset.

## Heating proxy

These specs cover the thin JSON relay contract between z2m-hub and the separate heating-mvp service.

### Heating proxy passes success JSON through unchanged

When the upstream heating service returns valid JSON, z2m-hub must relay that JSON body unchanged rather than rewrapping it.

### Heating mode style errors include ok false

For heating mode, away, and kill actions, local JSON or transport errors must be wrapped as `{ "ok": false, "error": ... }` so the dashboard gets a stable failure shape.

### Heating status style errors omit ok false

For the heating status read path, local JSON or transport errors must be wrapped as `{ "error": ... }` without adding an optimistic `ok` field.

### Heating status calls upstream status with GET

The heating status wrapper must call the upstream `/status` endpoint with HTTP GET and relay the returned JSON unchanged.

### Heating mode and kill call their upstream POST endpoints

The heating mode wrapper must POST to `/mode/{mode}`, and the kill wrapper must POST to `/kill`, so dashboard actions hit the intended upstream control routes.

### Heating away forwards request JSON body unchanged

The heating away wrapper must POST to `/mode/away` and forward the dashboard JSON body unchanged so the upstream service receives the requested away window payload intact.

## Config loading

These specs cover the startup fallback contract for `/etc/z2m-hub.toml` and the DHW defaults it feeds into runtime state.

### Missing config file falls back to built in defaults

When the TOML file is absent, config loading must return the built-in DHW defaults rather than failing startup.

### Partial config uses serde defaults for sane bounds

When the TOML file sets only the required DHW fields, omitted sane-bound fields must still pick up their serde default values.

### Invalid config falls back to built in defaults

When the TOML file exists but cannot be parsed into the expected schema, config loading must fall back to the built-in defaults.

## Password resolution

These specs verify PostgreSQL password resolution in `DatabaseConfig`.

For Pi/Linux services, production uses systemd encrypted credentials; dev/test may use one-shot `PGPASSWORD` injection from `ak` on the trusted dev/test machine. Secrets are never stored in TOML.

### Systemd credential is used when available

When `$CREDENTIALS_DIRECTORY/pgpassword` exists, its content must be used as the password. This is the production path via `systemd-creds encrypt`.

### Connection string includes resolved password

When a password is resolved from either supported source, `to_connection_string()` must include it as a `password=` parameter.

### Connection string omits password when none resolved

When no credential directory is set and no `PGPASSWORD` env var exists, `to_connection_string()` must not include a `password=` parameter.

The test must clear any ambient `PGPASSWORD` first so a developer shell or CI environment cannot create a false failure.

## PostgreSQL interface

These specs cover the fail-safe query/read contract outside the live database integration tests.

### Query fallback returns zero defaults on transport failure

When the PostgreSQL transport has died after connect, `query_pg_f64` must return `(0.0, "")` rather than propagating an error or panicking. This preserves the safety-critical fallback contract used by the DHW model and dashboard handlers.

## eBUS interface

These specs cover the small parsing rules z2m-hub applies to ebusd text responses before combining them with sensor data.

### Status01 hwc suffix marks charging

A Status01 response ending in `;hwc` must be recognised as active DHW charging even when the sfmode string is not `load`.

### Sfmode load marks charging without hwc suffix

The DHW status snapshot must still report charging when `HwcSFMode` is `load` even if the Status01 pump-state suffix is something else.

### Malformed Status01 falls back to zero return temperature

If Status01 does not contain a parseable return-temperature field, the parsed return temperature must fall back to `0.0` rather than failing the handler.

## Real PostgreSQL integration

These specs require a live TimescaleDB instance and are gated with `#[ignore]`. Run them on the trusted dev/test machine with one-shot `PGPASSWORD=$(ak get timescaledb) cargo test -- --ignored`.

All write-path live-PG tests must run inside a transaction and roll it back before finishing. They exist to verify typing and timestamp behaviour against the real schema, not to leave marker rows in the production `dhw` hypertable.

### Row decoding returns f64 and timestamp string

A `query_pg_f64` call against real `multical` must return a plausible temperature and a non-empty RFC3339 timestamp string.

This proves the new PG transport produces the same `(f64, String)` tuple shape as the old Flux CSV parser.

### INSERT includes explicit time column

After `write_dhw_to_pg`, the transaction-local `dhw` row must have a `time` value within the last few seconds, proving the `now()` timestamp is written explicitly rather than relying on server-side defaults.

### INSERT column types match dhw table schema

After `write_dhw_to_pg`, all nine columns of the transaction-local `dhw` row must decode to their expected Rust types (TIMESTAMPTZ, FLOAT8, INTEGER, TEXT, BOOLEAN) with values matching the input `DhwState`.

### Consecutive writes produce distinct rows

Two `write_dhw_to_pg` calls with different state must each produce distinct rows in the transaction view of `dhw`.

This proves `now()` is sufficiently unique for the single-writer service model, and both rows must be readable with correct values before rollback.

### End-to-end read and write against seeded tables

All three tables (`multical`, `dhw`, `dhw_capacity`) must be readable via `query_pg_f64`, and a transaction-local `write_dhw_to_pg` followed by a read-back must round-trip the `remaining_litres` value exactly without persisting the test row.
