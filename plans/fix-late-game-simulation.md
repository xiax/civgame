# Fix Late-Game Simulation Slowdown

## Summary
- The slowdown is coming from `SimulationSet::ParallelB`, not from the listed “suspect systems”. The current overlay only times a few hand-picked systems, while most `ParallelB` HTN/dispatcher work is invisible.
- The biggest structural issue is that `ParallelB` runs many serial dispatcher passes over the same non-dormant people every tick. Off-camera `Aggregate` LOD agents are treated almost like full agents there.
- A second bug makes bucketed work ineffective: new people set `SimClock.bucket_size = population.min(10_000)`, so nearly everyone stays active every tick as population grows.
- Growing `Blueprint` and `GroundItem` counts amplify this through repeated target scans, especially build/personal-blueprint lookup and loose-item fallback scans.

## Key Changes
- Add low-overhead timings for the missing `ParallelB` systems: `goal_dispatch`, `job_claim`, key HTN dispatchers, build/clear-obstacle, play, farm/craft, and tool prefetch. Show these in the Performance panel alongside per-LOD person counts and `SimClock { population, bucket_size }`.
- Fix `SimClock` registration in person spawn/reproduction so `bucket_size` stays capped at the intended active slice instead of growing to the full population.
- Add a shared dispatch gate:
  - `Full` LOD dispatches every tick.
  - `Aggregate` LOD dispatches only on its bucket tick.
  - `Dormant` stays skipped.
- Apply that gate to `goal_dispatch_system`, `goal_update_system` unemployed re-eval, and the `ParallelB` HTN dispatchers. This preserves responsiveness near the camera while making offscreen regions genuinely cheaper.
- Add a `PersonalBlueprintIndex` keyed by owner entity, and use it in build/clear-obstacle dispatch instead of scanning all `BlueprintMap` entries per build-goal worker.
- Replace loose-item fallback square scans with an indexed candidate lookup for public, non-storage `GroundItem`s by resource class/id. Keep `CurrentVision` as the first choice, but make the “freshly dropped item” fallback candidate-based rather than tile-sweep-based.

## Tests
- Add unit tests for `SimClock` slot allocation: spawning/materializing/reproduction past 250 population must not inflate `bucket_size` to full population.
- Add LOD dispatch-gate tests: `Full` always due, `Aggregate` only due on `clock.is_active(slot)`, `Dormant` never due.
- Add index tests for personal blueprints: insert/update/remove keeps owner lookup correct and build dispatch finds only the owner’s blueprint.
- Add regression coverage for loose-item indexing: storage tiles excluded, public loose items included, resource-specific lookup returns nearest reachable candidate.
- Run `cargo test --bin civgame`.

## Assumptions
- No new crates.
- Full camera-region simulation semantics stay unchanged.
- Off-camera `Aggregate` agents may react up to one bucket cycle later; that is the intended tradeoff for late-game performance.
- The immediate player workaround is to set Offscreen fidelity to `Minimal`, but the code fix should make `Balanced` viable again.
