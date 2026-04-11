# DHW

This file describes how z2m-hub models the current state of domestic hot water in the cylinder.

## DHW tracking model

z2m-hub estimates remaining usable hot water by combining ebusd state, InfluxDB sensor history, and a physics-based charge/draw model.

## Inputs and polling

The DHW loop polls all required signals every 10 seconds.

The model reads charge state from eBUS, `HwcStorageTemp` from eBUS, and T1, draw flow, and cumulative volume from InfluxDB. The shared state for this model lives in `DhwState`, while the highest-value state transitions now sit in small pure charge-completion and draw-tracking helpers that the loop calls after polling.

## Charge detection

A charge is active when either the Vaillant SF mode is `load` or `Status01` reports pump state `hwc`.

On a charge start, the loop snapshots `t1_at_charge_start` and begins watching for crossover. While charging, the state is `charging_below` until crossover happens and `charging_uniform` after it does.

## Crossover rule

A charge is considered fully successful when `HwcStorageTemp` reaches the T1 value measured at charge start.

Once `hwc_now >= t1_at_charge_start`, the cycle is marked as having achieved crossover. On charge completion with crossover, remaining litres are reset to `full_litres`, the state becomes `full`, and the end-of-charge T1/HwcStorage snapshot becomes the baseline for later standby decay.

## No-crossover completion model

A charge that ends before crossover is interpreted through the temperature gap between T1 and HwcStorage.

The model compares `gap = t1_now - hwc_now` against config thresholds:

- `gap < gap_dissolved` means the thermocline is effectively dissolved, so treat the cylinder as full
- `gap > gap_sharp` means the thermocline stayed sharp, so keep the previous remaining litres
- gaps between those thresholds interpolate between unchanged and full
- the equality boundaries stay on the interpolation path, so exact `gap_dissolved` and `gap_sharp` do not take the strict outer branches

This preserves the physical intuition that some charges add useful energy without fully homogenising the cylinder.

## Draw tracking

Water draws reduce remaining litres even if a charge is happening at the same time.

The loop always applies draw tracking when flow exceeds `draw_flow_min`. It subtracts drawn volume from the last reset point and then applies temperature-based caps:

- a first `HwcStorageTemp` crash beyond `hwc_crash_threshold` caps remaining at `vol_above_hwc` and sets a one-draw crash flag so the first-crash branch is not re-applied on every later tick
- a T1 drop above 0.5°C caps remaining at 20L
- a T1 drop above 1.5°C sets remaining to zero
- exact T1 drops of 0.5°C or 1.5°C stay on the weaker branch because both thresholds are strict `>` checks
- if both the HwcStorage crash and severe T1-drop conditions appear in the same update, the zero-litre T1 rule wins

This protects against overestimating usable water late in a shower sequence. The async loop still owns sensor polling and persistence, but the litre/cap calculations themselves are factored so they can be unit-tested without InfluxDB or eBUS fixtures.

## Standby decay

The model cools the effective top temperature over time without deleting litres from the tank.

`effective_t1` falls using `t1_decay_rate` from config. When the effective top temperature drops below `reduced_t1`, the UI should treat the water as cooler, but the litres estimate still reflects water volume rather than comfort temperature.

## Capacity autoload and persistence

Configured capacity can be upgraded at startup from a recommended InfluxDB value and the live estimate is written back after updates.

`z2m-hub.toml` provides defaults and sane bounds. At startup, the service loads `recommended_full_litres` from InfluxDB, takes the max of config and recommended values when the recommendation is sane, and writes the current estimate back to the `dhw` measurement during operation.
