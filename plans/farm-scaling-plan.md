# Comprehensive Farm Scaling Plan

## Summary
- Treat the fixed winter slowdown as a symptom of a broader farm-work discovery problem: several systems independently scan plot rectangles to rediscover the same `{prepare, plant, harvest}` state.
- Replace repeated rectangle scans with a shared plot-member index plus cached farm-work availability.
- Preserve current gameplay behavior, with one correctness fix: farm systems must only target actual Agricultural member tiles, not empty holes inside a `Plot.rect` hull after partial retirement.

## Current Problems
- `FieldTileIndex` says Agricultural membership is per tile, but scanners still walk `Plot.rect`; after `FarmRetirements` drains, removed tiles inside the hull can be misread as unprepared farm capacity.
- Work discovery is duplicated across goal scoring, chief postings, posting expiry, and HTN dispatch.
- `carve_plots_system` can rebuild desired plot sets from unchanged plans every fixed tick.
- The current 60-tick farm precompute is much better than the original bug, but it is still a polling workaround instead of a durable scaling surface.

## Data Model Changes
- Extend `FieldTileIndex` with `by_plot: AHashMap<PlotId, AHashSet<(i32, i32)>>`.
- Add helpers on `FieldTileIndex`: `ensure_entry`, `remove_tile`, `plot_tiles`, `plot_has_members`, `plot_tile_count`, and `debug_assert_consistent`.
- Update production code to mutate field membership only through these helpers; keep `by_tile` readable for now to limit churn.
- Add `FarmWorkIndex` resource keyed by `PlotId`.
- Each `PlotWorkSnapshot` stores `unprepared`, `plantable`, `mature`, `member_tiles`, `holder`, `faction_id`, `rect`, and `updated_tick`.
- Add lookup maps inside `FarmWorkIndex`: `state_owned_by_faction: AHashMap<u32, Vec<PlotId>>` and `household_plots: AHashMap<u32, Vec<PlotId>>`.

## Refresh Strategy
- Add `DirtyFarmPlots(AHashSet<PlotId>)`.
- Mark plots dirty when field state changes: plot carve/add/remove, retirement drain, prepare completion, planting success, harvest completion, fallow recovery, and plant lifecycle stage/death changes on Agricultural tiles.
- Rebuild all plot snapshots on season change.
- Rebuild only dirty plots during normal ticks.
- Keep a low-frequency full audit, at most once per game day, as a safety net while the dirty hooks mature.
- Schedule refresh in `SimulationSet::Economy` after land carve/acquisition and before fieldwork expiry, farm assignment, and chief posting. `goal_update_system` can read the previous refreshed snapshot with at most one tick of staleness.

## Consumer Changes
- `goal_update_system`: remove its local farm tile scan/cache. For each household member, ask `FarmWorkIndex` whether the household owns any Agricultural plot and whether any owned plot has current-season work.
- Private Spring work uses current storage state: `unprepared > 0` is work, and `plantable > 0` is work only when household or parent storage has seed stock.
- `chief_job_posting_system`: rank plots using `FarmWorkIndex` counts instead of calling `plot_tile_counts` for every plot.
- `fieldwork_expiry_system`: shrink/drop postings from cached counts for `plot_id`; keep old rectangle fallback only for legacy/no-plot postings.
- `FarmScope`: carry `plot_id: Option<PlotId>` alongside the existing rect/area fallback.
- HTN plant/prepare/harvest dispatch: choose nearest live tile by iterating `FieldTileIndex::plot_tiles(plot_id)`, then applying live reservation, plant, nutrient, and reachability checks.
- Keep `JobProgress::FieldWork { area, plot_id, ... }` wire shape unchanged; `area` remains UI/back-compat, `plot_id` becomes the authoritative farm scope when present.

## Carve Optimization
- Add a small `PlotCarveCache` keyed by faction id.
- Compute a deterministic geometry hash from plan zones, zone kinds, rects, spine segments, and `culture_hash`.
- Make `carve_plots_system` skip factions whose geometry hash is unchanged and whose prior carve completed.
- When carving does run, use `FieldTileIndex::plot_tiles(pid)` instead of scanning all `by_tile` entries to find a plot’s current members.
- Use `FieldTileIndex::plot_has_members(pid)` in retirement drain instead of global `by_tile.values().any(...)`.

## Tests
- Unit: `FieldTileIndex` helper methods keep `by_tile` and `by_plot` consistent across insert, move, remove, and repeated ensure.
- Unit: plot-member scanners ignore tiles inside `Plot.rect` that are not in `FieldTileIndex::by_plot`.
- Regression: partial hard-conflict retirement drains a hole, and no Prepare/Plant/Harvest count targets the removed hole.
- Regression: chief posting uses cached counts and emits the same postings as old scanning for intact plots.
- Regression: fieldwork expiry shrinks/drops from `FarmWorkIndex` and still handles legacy no-plot postings.
- Regression: private household goal availability updates after plot transfer, seed stock changes, planting, harvest, and winter transition.
- Regression: unchanged settlement plans make `carve_plots_system` a no-op after the first carve.
- Run `cargo test --bin civgame`.

## Docs And Acceptance
- Update the farm/land sections in `src/simulation/CLAUDE.md`; update root `AGENTS.md` only if behavior visible to future agents changes materially.
- Acceptance: no repeated per-system full plot scans in normal farm operation, no phantom farm work on retired holes, unchanged gameplay for intact plots, and better Sim Timing stability as fields and households scale.
- No new crates; use existing `ahash` and Bevy resources/events.
