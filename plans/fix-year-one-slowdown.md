# Fix Year-One Winter Slowdown

## Summary
- Primary culprit: `[goal_update_system](/Users/xiao1/civgame/src/simulation/goals.rs:687)` precomputes household seasonal farm work every fixed tick, then scans every tile in every household Agricultural plot.
- In winter, `[FarmSeasonPhase::WinterDormant](/Users/xiao1/civgame/src/simulation/farm.rs:153)` never produces farm work, but the current loop still full-scans each plot because the winter match arm cannot break early.
- This matches the symptom: Spring/Summer/Autumn often break after finding one relevant tile, while Winter scans entire plots continuously.

## Key Changes
- In `[goals.rs](/Users/xiao1/civgame/src/simulation/goals.rs:687)`, keep recording `households_with_ag_plot`, but skip the seasonal tile scan entirely when `farm_season == FarmSeasonPhase::WinterDormant`.
- Move grain-seed availability calculation so it only runs during Spring planting checks, since Summer/Autumn/Winter do not need it.
- Leave broader policy-based skipping as a follow-up unless tests prove the household-vs-parent faction policy mapping is safe to optimize now.

## Test Plan
- Add a regression test around the farm-work precompute/helper proving winter returns no seasonal work without probing plot tiles.
- Add or preserve checks that Spring/Summer/Autumn still detect unprepared, plantable, and mature crop work.
- Run `cargo test --bin civgame`.
- Manually validate with the existing debug panel timing through year-one winter: fixed tick average/worst time should no longer climb just because winter begins.

## Assumptions
- The reported slowdown is sustained winter simulation cost, not only a one-frame hitch at the season transition.
- If a residual winter-onset hitch remains after this fix, investigate season-edge systems next, especially plant lifecycle cleanup and post-harvest loose ground-item churn.
