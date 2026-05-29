# Early-Game Performance Fix Plan

## Summary
Fix the year-1, ~20-population slowdown first, then add a user-facing offscreen fidelity toggle for later-game scaling. The first patch should make performance visible in-game, remove obvious per-tick scans/allocation churn, and prevent over-time growth in jobs/items/storage/path work from quietly compounding.

## Key Changes
- Add a lightweight “Performance” debug panel section showing live counts and timings: people by LOD, ground items, job postings, blueprints, loaded chunks, focus points, path queue/failures, storage recomputes, vision recomputes, settlement survey snapshots, and coarse `SimulationSet` timings.
- Optimize early-game hot paths:
  - Cache goal-system snapshots that are currently rebuilt every fixed tick, including population counts, household farm ownership/work availability, and nearby tameable-animal availability.
  - Replace vision-system per-tick allocation/sort/cap selection with a cursor/budgeted scheduler, and only recompute vision for dirty, moved, lookout, or due-bucket agents.
  - Reduce storage/resource-demand churn by making faction storage recomputes more targeted and surfacing why a full sweep happened.
  - Rework loose-stockpile posting to use spatial lookups or bounded incremental scans instead of repeatedly sweeping full 65x65 home/market areas.
  - Move settlement-plan projection behind its stale/dirty gate, and add survey backoff when no relevant terrain/member/building/road state changed.
- Add `PerformanceSettings` with an in-game UI toggle for offscreen fidelity:
  - `Balanced` default: fully simulate camera/current settlement, keep discovered regions remembered but cheaper.
  - `All Live`: preserves current every-settled-region focus behavior.
  - `Minimal`: strongest performance mode for offscreen regions.
- Keep changes ECS-native, deterministic, and crate-free.

## Tests
- Add focused tests for cache invalidation in goal/farm/household state, vision scheduler budgeting, loose-item/job-posting bounds, and offscreen focus-mode point counts.
- Add a deterministic perf smoke scenario for a 20-pop year-1 settlement that asserts bounded counts for path queue, postings, ground items, loaded chunks, and focus points.
- Verify with `cargo test --bin civgame` and `cargo check`.
- Manual acceptance: in the debug panel, average/p99 fixed tick time should stabilize instead of climbing, with no monotonic growth in ground items, postings, path queue, or storage full sweeps during the reported scenario.

## Assumptions
- Primary target is the user’s current case: year 1, ~20 population, workers still near the starting settlement.
- Offscreen-region fidelity is a runtime preference, so the implementation should expose a toggle rather than hard-removing current behavior.
- Behavior docs should be updated in the matching repo guidance file when simulation behavior changes.
