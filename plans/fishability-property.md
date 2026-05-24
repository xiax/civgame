# Fix: model fishability as a property of water tiles

## Context

Worker on `GatherFood` (chief Stockpile job #6487, method `FishForStorage`) stuck pathing to tile `(10483, 14585)` — a **dug well** in the player settlement — and failing repeatedly (`FailedTarget` ×5). No real river or lake within `FISHING_SEARCH_RADIUS = 14`.

The drink pipeline needs well shafts to read as fresh water, so well construction projects the shaft cell as `TileKind::Water` (`world/CLAUDE.md` → "Well shafts project as `Water`, never `River`"; `world/water_runtime.rs:586`). The fishing pipeline naively treats any `River|Marsh|Water` tile as fishable:

- `fishing.rs:341-366` `nearest_fishable_water` — bare `TileKind` match + `has_stand_tile`.
- `fishing.rs:109-119` `habitat_at` — same `TileKind` match → `FishHabitat::Lake` at execute time.
- Callers: `htn.rs:4274` `htn_acquire_food_dispatch_system`, `htn.rs:5658` `htn_stockpile_food_dispatch_system`. `FishForStorageMethod` precondition gates only on `fish_spot_tile.is_some()` (`htn.rs:2501`).

`MethodHistory` decays (TTL 600). With no real water in range and explore fallback at 0.3, the worker oscillates: try fish → fail → bias down → forage/explore → bias decays → try fish again. A dug well is ~1.5 m across — the same naive match would also mis-classify dug pits, tiny dam pools, future spring features, etc. Fix the rule, not the offender.

## Approach

Replace the bare `TileKind` match with a **fishability predicate** that requires the tile to belong to a connected `River|Marsh|Water` component of at least `MIN_FISHABLE_WATER_TILES` cells (bounded BFS with early-out at the threshold, blocked by `Dam`). Wells (1 cell), puddles, and tiny dam pools fail; rivers, lakes, marshes, real impoundments pass. No `WellMap` plumbing; no signature changes upstream.

### Changes

**`src/simulation/fishing.rs`**

1. Add tunable near the other constants:
   ```rust
   pub const MIN_FISHABLE_WATER_TILES: usize = 8;
   ```
2. Add private helper `tile_supports_fishery(chunk_map, tile) -> bool`: BFS over `River|Marsh|Water` neighbours (4-conn), blocked by `Dam` and non-water, early-out as soon as visited set reaches `MIN_FISHABLE_WATER_TILES`. `Bridge` is neither water nor walked through (banks on either side of a bridged river still easily clear the threshold).
3. Replace the `matches!(..., TileKind::River | Marsh | Water)` arm in `nearest_fishable_water` (line ~350) with `tile_supports_fishery(chunk_map, tile)`. `has_stand_tile` check unchanged.
4. In `habitat_at` (line ~109), call `tile_supports_fishery` first; return `None` if false. Salinity/habitat branching unchanged.

**No caller changes.** `nearest_fishable_water` and `habitat_at` already take `&ChunkMap`; HTN dispatchers + `fish_task_system` need no new params.

**`src/simulation/CLAUDE.md` → Fishing section**

5. Update one-liners for `habitat_at` / `nearest_fishable_water`: "tile must belong to a connected water component of ≥ `MIN_FISHABLE_WATER_TILES` cells; wells, dug pits, tiny pools fail by size."

**`src/world/CLAUDE.md`** — append to the well-shaft note: "fishing sees the `Water` projection but `fishing::tile_supports_fishery` excludes it by component-size."

### Tests (`fishing.rs::tests`)

- `single_water_tile_is_not_fishable` — one `Water` tile in a sea of `Grass`; both `nearest_fishable_water` and `habitat_at` return `None`.
- `large_lake_is_fishable` — 4×4 `Water` block; both return Some.
- `river_strip_is_fishable` — 10×1 `River` line; passes.
- `well_shaft_pattern_is_not_fishable` — replicate the 5×5 well projection (1 `Water` centre + lining); fails.

## Verification

1. `cargo check` — no API change, should be clean.
2. `cargo test --bin civgame` — existing fishing tests + new tests above pass.
3. `cargo run` — load a settlement with a dug well and no nearby river/lake. Worker on `GatherFood` no longer picks `FishForStorage` for the well; inspector shows another concrete method or `ExploreForFood`. Pan to a real river: fishing still works there.

## Out of scope

- No `FishStock` init changes — non-fishable tiles never reach harvest.
- No `MethodHistory` / scoring changes — once dispatch is correct, explore-fallback handles the no-water case.
- No `WellMap` filter — property check subsumes it.
