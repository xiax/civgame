# Accurate Game Speed, Pause, And Headroom

## Summary
- Make Bevy `Time<Virtual>` the single source of truth for simulation speed and pause.
- Ship visible HUD presets `0x`, `1x`, `2x`, `5x`, while structuring the system so `10x+` can be added deliberately.
- Add tick-cost telemetry so higher speeds are based on whether the current world is keeping up, not guesswork.

## Key Changes
- Add a `GameSpeed` resource plus `SpeedPreset` enum:
  - Presets: `Paused`, `Normal`, `Fast`, `VeryFast` mapping to `0.0`, `1.0`, `2.0`, `5.0`.
  - Store `last_unpaused` so pause resumes to the prior speed.
  - Sync to `Time<Virtual>::pause/unpause` and `set_relative_speed` in `PreUpdate`.
- Clean up `SimClock`:
  - Remove `speed` and unused `accum`.
  - Keep `tick`, bucketing, and `scale_factor()` for entity bucket compensation only.
- Remove double-speed application from movement and animals:
  - Movement uses fixed tick `delta` directly.
  - Needs/work/cooldowns/calendar/economy speed up naturally because more fixed ticks run per real second.
- Move simulation-coupled path request draining to `FixedUpdate` before movement so path budgets scale with sim time.
- Keep paused inspection usable:
  - Camera input uses `Time<Real>`.
  - Selection, panels, overlays, and issuing orders remain available.
  - Simulation command lifecycle does not advance until unpaused.
- Add `SimTimingDiagnostics`:
  - Track fixed ticks run per rendered frame, rolling average tick CPU time, worst recent tick, and whether the sim is falling behind.
  - Surface these in the debug panel near existing pathfinding diagnostics.

## Higher-Speed Guardrails
- Keep `5x` as the highest normal HUD preset for this pass.
- Define speed presets in one constant/table so adding `10x` later is a one-line enum/table/UI addition plus tests.
- Do not auto-clamp `5x`; if the machine cannot keep up, show diagnostics instead of silently changing behavior.
- Add a debug warning when average tick time exceeds the per-speed budget:
  - `5x` budget is about `10 ms/tick`.
  - `10x` future budget would be about `5 ms/tick`.
  - `20x` future budget would be about `2.5 ms/tick`.

## UI And Controls
- Update HUD speed buttons in [hud.rs](/Users/xiao1/civgame/src/ui/hud.rs) to write `GameSpeed`.
- Keep buttons: `⏸`, `1x`, `2x`, `5x`.
- Add keyboard controls guarded by egui focus:
  - `Space`: pause/resume.
  - `Digit1`: `1x`.
  - `Digit2`: `2x`.
  - `Digit3`: `5x`.

## Tests
- Paused speed: `SimClock.tick` and `Calendar.ticks_this_day` do not advance.
- Resume restores the previous non-paused preset.
- `2x` and `5x` run the expected number of fixed ticks under manual time.
- Movement at `2x` for one real frame matches two `1x` fixed ticks within tolerance.
- Needs/work progress are not double-scaled after removing `SimClock.speed`.
- Diagnostics record nonzero fixed tick timing after simulated updates.

## Assumptions
- No `10x` or `20x` player-facing preset in this pass.
- No save/load persistence for speed state.
- Keep Bevy’s current `Time<Virtual>::set_max_delta(50ms)` to avoid catch-up spirals after OS stalls.
- Update `AGENTS.md` with the new speed/pause contract after implementation.
