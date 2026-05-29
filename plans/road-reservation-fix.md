# Build-Planner Road Reservation Fix

## Context

Roads are planned as centerlines but **carved as 2-tile-wide corridors**. Two gaps let non-road
construction and roads collide:

1. **Centerline-only guards.** `candidate_touches_planned_road` (`construction.rs`) and
   `populate_seed_reservation_system` (`seed_reservation.rs`) check only `SettlementBrain.road_tiles`
   (Bresenham centerline, endpoints excluded). The carver (`road_carve_system`) and
   `road_corridor_tiles_for_segments` stamp the centerline **plus** a perpendicular-widened tile per
   step. A footprint on the *widened lane* passes the guard then gets clipped by the carver.
2. **Road painted under furniture.** `road_carve_system` skips `BlueprintMap`/`BedMap`/`WellMap`/
   `Cropland`/farm-protected tiles but **not** general `StructureIndex` tiles — so a road can flip a
   workbench/granary tile to `TileKind::Road` while the entity still sits there.

**Goal:** treat the widened corridor (planned + queued + carved) as the first-class road reservation
all non-road construction avoids, and ensure roads + structures **never share a tile** — without
breaking the road or destroying the structure.

**Why not "tolerate a gap" under furniture.** The "roads tolerate 1-tile gaps" rule applies to the
*farm* case only: a skipped `Cropland`/farm tile **stays passable** (speed 0.9) so traversal is
preserved. Furniture is **impassable** — a gap there is a hard blockage, not cosmetic. So roads must
**route around** structures and stay continuous.

**Decided (road vs structure):** roads route around standing structures. In the straight-segment
architecture this means an *adaptive widen side* (change 2): centerlines run along clear parcel
frontage, so the realistic collision is the widened edge tile abutting a structure.

**Decided (wells):** wells stay **exempt**. The shipped well contract makes `road_carve_system`
detour around the 5×5 well disc (`in_well_footprint`); wells are aquifer-anchored and can't relocate.
The `brain: None` in the Well arm of `intent_site_clear` (`organic_settlement.rs`) is **kept**.

## Key Changes

1. **Carve-faithful guard.** Replace the `brain.road_tiles` test in `candidate_touches_planned_road`
   with a `reserved_road` predicate rejecting a footprint tile in any of: `brain.road_corridor_tiles`,
   a rasterized `RoadCarveQueue` segment (2-tile), or carved `TileKind::Road`. Apply the same filter
   in `settlement_project_selection_system` so corridor-overlapping top intents don't starve valid
   lower ones (add `SettlementMap`/`SettlementBrains`/`RoadCarveQueue`/`ChunkMap`; bundle into a
   SystemParam if the 16-arg ceiling bites).

2. **Roads route around structures.** New `road_widen_offset_avoiding(from, to, is_blocked)` returns
   the default perpendicular side unless blocked by a `StructureIndex`/`BlueprintMap` tile, else the
   opposite side. Thread it through every corridor producer: `road_corridor_tiles_for_segments` +
   `build_road_network` use the survey structure snapshot (`SurveyStructureSnapshot`); `road_carve_system`
   + the change-1 predicate + `rasterize_line_into` use live `StructureIndex`. `road_carve_system`
   also adds a `StructureIndex` early-skip in `try_write_road` as a pure anti-corruption backstop
   (never overwrite a building). Residual gap only when a structure sits squarely on an unavoidable
   centerline (degenerate legacy) — backstop skips that one tile.

3. **Seed reservation uses the corridor.** `populate_seed_reservation_system` reserves
   `brain.road_corridor_tiles` instead of `brain.road_tiles`. House/stranded relocation get corridor
   coverage for free via `SeedReservation.is_reserved`.

4. **Centralize widen.** `rasterize_line_into` calls `road_widen_offset` (drop its inline formula);
   reuse it for the queued-segment leg of the change-1 predicate so guard/reserve/carve stay lock-step.

5. **Seed backstops reject the corridor.** Thread `brain.road_corridor_tiles` into
   `seed_single_tile_clear`/`find_clear_seed_single_tile` (Option-tolerant for fixtures); palisade
   `single_tile_clear` switches its road check to the corridor set. House relocation covered via #3.
   **Wells excluded** — Well arm keeps `brain: None`.

6. **Doormats stay road-compatible.** Confirm doormat selection allows road tiles; only tighten so a
   doormat tile carrying a finished `StructureIndex` is blocked. Skip if already excluded.

## Critical Files

- `src/simulation/construction.rs` — `candidate_touches_planned_road`, `road_carve_system`,
  `seed_single_tile_clear`, `find_clear_seed_single_tile`.
- `src/simulation/organic_settlement.rs` — `settlement_project_selection_system`, `build_road_network`,
  `road_widen_offset` + new `road_widen_offset_avoiding`, `road_corridor_tiles_for_segments`,
  palisade `single_tile_clear`/`organic_palisade_site`, Well arm `brain: None` (unchanged).
- `src/simulation/survey_task.rs` — `SurveyStructureSnapshot`/`MapsSnapshot` (snapshot `is_blocked` view).
- `src/simulation/seed_reservation.rs` — `populate_seed_reservation_system`, `rasterize_line_into`.
- `src/simulation/settlement_bootstrap.rs` — house relocation conflict predicate.

## Reuse

`SettlementBrain.road_corridor_tiles`, `road_widen_offset`, `rasterize_line_into`,
`candidate_footprint_tiles`, `SeedReservation.is_reserved`.

## Test Plan (`cargo test --bin civgame`)

- Corridor guard rejects a furniture footprint on the widened lane (not centerline) + on a queued segment.
- Selection-time filter picks a valid lower-priority intent when the top one overlaps a corridor.
- Adaptive widen routes around a structure on the default side (widens opposite, stays 2-wide, continuous).
- Anti-corruption backstop: structure on a centerline tile is not overwritten (entity survives).
- Lock-step parity: snapshot-view and live-view widen choices match for the same structure set.
- Seed reservation: all `road_corridor_tiles` read `is_reserved`.
- Seed backstop: `find_clear_seed_single_tile` never returns a corridor tile.
- Well exemption regression: spine over a future well still places it; carve leaves the disc uncarved.
- Widen-rule parity: `rasterize_line_into` vs `road_corridor_tiles_for_segments` agree per segment.

## Verification (in-game)

`cargo run`, grow a settlement past Hamlet: no furniture on `Road` tiles, no `Road` under furniture;
wells still seat with roads visibly detouring around the disc.

## Out of Scope

Roads stay continuous and route around structures (never broken/never destroy a building) except the
anti-corruption skip for an unavoidable centerline structure (degenerate legacy). No destructive
cleanup of existing overlaps.

## Docs

Update `src/simulation/CLAUDE.md`: road reservation = widened corridor (planned + queued + carved);
roads route around structures via `road_widen_offset_avoiding` and never share a tile with one;
`road_carve_system` has a `StructureIndex` anti-corruption skip; wells remain exempt; scope the
"roads tolerate 1-tile gaps" note to passable farm tiles only; `populate_seed_reservation_system`
reserves `road_corridor_tiles`.
