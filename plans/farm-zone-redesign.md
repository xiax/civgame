# Full Farm-Zone Redesign

**STATUS: shipped.** A1–A3, B1–B3, C1–C4, D done. `cargo build` clean; full
suite 759 passed/0 failed (+3 new: `ag_belt_outside_footprint`,
`ag_belt_deterministic`, `farm_protected_for_ag_tiles_and_plants`;
`cropland_is_soil_speed`). Design fix during impl: ag-belt road-access radius
widened by half-block (`access_radius = block/2 + 8`) — probing from
`rect.center()` at radius 8 would have rejected every 16×16 block (edge is 8
from centre), zeroing all farms. Remaining: in-game visual check of belt
placement/color (manual — game needs SpawnSelect→Playing interaction).

**Follow-up (post-feedback "farms still next to base / cropland all over"):**
root cause was an incomplete overhaul — the belt was silently bypassed by
two near-home fallbacks in `compat_plan_from_brain` (district-broad +
legacy `build_settlement_plan` home-centred ag zone) whenever
`build_ag_belt` produced 0 parcels, which the fragile `best_fertile_tile`
Field-anchor gate (≥130, home-biased) made common. Fixes: (1)
`compat_plan_from_brain` emits Agricultural ONLY from belt/frontier
parcels — no district/legacy fallback; (2) `build_ag_belt` rewritten
self-anchored on a home-lattice (no Field/district dependency), `mean_fert
> 0` gate (no 0.40 floor), road access now a soft score not a hard gate,
scan reaches past the footprint; (3) `seed_starting_farms_system` sites
the starting plot on a brain belt parcel (no near-home offset). Farmstead
house-yards kept as visible `Cropland` per user. +1 regression test
`compat_plan_no_near_home_ag_fallback`; ag_belt tests updated to the
self-anchored model.

**Follow-up 2 ("farmer still farming in middle of settlement"):** the
*planting dispatch* was still core-bound — `chief_job_posting_system`'s
Farm fallback posted a `home_tile ±5` / `plot_id:None` job whenever no
farmer↔plot assignment existed (assignment refreshes only once/game-day
and needs a `Profession::Farmer`), so farmers claimed it and
`resolve_farm_scope`→Bootstrap planted underfoot. Fix4: deleted the
home±5 bootstrap; chief now posts an OPEN plot-scoped Farm job for the
nearest unassigned StateOwned Agricultural plot (`plot_id:Some`,
`assigned_farmer:None` → Communal scope, planting restricted to the belt
rect). No ag plot ⇒ no Farm posting. `chief_posts_farm_*` test rewritten
to assert plot-scoped (area = plot rect, never home±5).

## Context

**Symptom (user-confirmed):** crop plots appear inside / on the town road grid
instead of as distinct fields outside town.

**Why it happens:** `build_parcels_road_driven`
(`src/simulation/organic_settlement.rs:1613`) treats `DistrictKind::Agricultural`
exactly like Residential/Crafting — it sweeps every road tile, hard-fronts a
16×16 farm parcel on a cardinal edge (`parcel_rect_from_road`), and scores
`s * target * 1/(1+home_dist*0.05)` which **biases parcels toward the town
core**. With Agricultural target `((members+2)/3).clamp(2,24)` (≈7 for a 20-pop
village), ~7 large blocks get woven into the street grid. The
`SettlementAnchorKind::Field` → Agricultural `DistrictInfluence` (radius 10) is
computed but the road sweep ignores it.

Two related gaps complete the picture (user chose **full redesign** scope):
roads carve straight through whatever farm tiles exist (`write_road_tile` /
`road_carve_system` in `construction.rs` gate only on raw `TileKind`, blind to
`PlotIndex`/`PlantMap`), and farms have **zero tile-level visual** (`TileKind::Farmland`
was removed; farms = plain Grass + wheat entities; tiles render as a flat
per-kind `ColorMaterial`, no tile→sprite path — the unused `building_farm`
sprite is an *entity* sprite and cannot key tile rendering).

**Outcome:** Agricultural fields form a coherent, visible belt *outside* the
built-up footprint, off the road spine; road carving never destroys farm soil.

## Approach

Three workstreams. Step 0: copy this plan to
`civgame/plans/farm-zone-redesign.md` (repo convention for design plans).

### A — Ag belt siting (`organic_settlement.rs`)

**A1.** In `build_parcels_road_driven` (:1613), remove `DistrictKind::Agricultural`
from `KIND_ORDER` (:1640). Non-ag parcels keep the road-driven sweep.
`build_parcels_frontier_driven` (:1557, camps/nomadic — no `PERM_SETTLEMENT`)
is left **unchanged** so camps still allocate ag via the frontier path.

**A2.** Add `fn build_ag_belt(faction, settlement, brain, chunk_map, occupied: &[TileRect], next_id: &mut u32, budget: usize) -> Vec<Parcel>`
near :1737. Call it from the `build_parcels` dispatcher (:1538) inside the
`PERM_SETTLEMENT && !road_tiles.is_empty()` branch: run the road sweep first,
collect its rects as `occupied`, then append the belt with continued `id`
numbering and the shared `MAX_PARCELS` (48) budget.

Algorithm (deterministic — no RNG, no ahash-iteration dependence; sorted vecs /
fixed ranges only):
- `ag_target = parcel_targets(...).get(Agricultural)`; return `vec![]` if
  `None`/0 (covers camps).
- Anchor a fixed 16×16 lattice on the Agricultural `DistrictInfluence` centre
  (from `best_fertile_tile`, pure `chunk_map` reads → seed-stable):
  `lattice origin = (ag_cx.div_euclid(16)*16, ag_cy.div_euclid(16)*16)`.
- `footprint` = union of non-ag `occupied` rects + civic disc (home, r=5) +
  Residential/Crafting `DistrictInfluence` rects, inflated by
  `BELT_CLEARANCE = 3`.
- For each 16×16 lattice block within `BELT_SCAN = 64` of the ag centre:
  reject if it overlaps `footprint`; reject if
  `!rect_clear_for_parcel(chunk_map, rect, &brain.road_tiles)` (:2742, already
  rejects road/wall/water/impassable); require
  `distance_to_road_network(chunk_map, brain, rect.center(), ACCESS_MAX=8)`
  (:2806) to resolve (short track, not full frontage); compute 5-sample mean
  fertility (centre+4 corners, mirror `compute_plot_value`); skip if
  `< AG_BELT_MIN_FERT (≈0.40 normalized)`.
- `score = fert*1.4 + water_bonus(river_distance_at center) + contiguity_bonus`
  where `contiguity_bonus = +0.15` per already-accepted ag block sharing an
  edge — recomputed greedily so the belt grows as a connected blob from the
  highest-fertility seed. **No `home_dist` term.**
- Sort `(score desc, tile_hash asc)` (`tile_hash` recipe at :1682), greedily
  accept non-overlapping until `ag_target` or budget.
- Emit `Parcel { shape: Rect(rect), frontage_edge: None, access_tile:
  Some(track), holder: State{owner_faction}, district_hint: Some(Agricultural),
  suitability }`. `frontage_edge: None` is already tolerated downstream
  (`compat_plan_from_brain` reads only `district_hint` + `rect`; today's
  frontier ag parcels already set it `None`).

Reuse: `rect_clear_for_parcel`, `distance_to_road_network`, `rects_overlap`,
`chunk_map.tile_fertility_at`/`river_distance_at`, `parcel_targets`, `cheb`,
`tile_hash`. 16×16 emission preserves the `plot_size_for(Agricultural)=Some((16,16))`
→ 1 plot / parcel 1:1 invariant.

### B — Road never carves farm tiles (`construction.rs`, `land.rs`)

**B1.** Add `pub fn tile_is_farm_protected(plot_index: &PlotIndex, plant_map:
&PlantMap, tile: (i32,i32)) -> bool` in `land.rs` (next to `plot_at`): true if
`plot_at` resolves to a Plot with `zone_kind == Agricultural`, or
`plant_map.0.contains_key(&tile)`. `PlotIndex.by_tile` is surface-only — matches
surface road carving.

**B2 (single chokepoint).** Thread `Res<PlotIndex>` + `Res<PlantMap>` into
`road_carve_system` (`construction.rs:5168`) and `write_road_tile` (:5132); add
`&& !tile_is_farm_protected(...)` before each `set_tile(... Road ...)`. These
two functions are the **only** Road-writing sites (grep-verified: writes at
:5148, :5208, plus the two doormat `write_road_tile` calls at :5941, :7721 —
all funnel through these two functions). Every `RoadCarveQueue` producer
(doormat ×2, spine drain `settlement.rs:1095`, `maybe_queue_desire_path`,
`survey_task.rs:264`) is drained only by `road_carve_system`. Guarding the two
functions = complete coverage; no per-producer rewrites (satisfies "no
half-measures": every path is covered at the chokepoint). Thread the two `Res<>`
into the doormat-finalize systems (~`construction.rs:5415`/`7335`) for the
direct `write_road_tile` calls. Roads already tolerate gaps — the Bresenham
loop `continue`s on non-writable tiles with no reroute; a 1-tile Grass/Cropland
break is passable.

**B3.** In `maybe_queue_desire_path` (`organic_settlement.rs:2828`) also skip
when the heat `tile` falls inside any `brain.parcels` Agricultural rect (cheap
filter+`contains`) so desire paths don't even target farm centres. No spine
geometry change — B2 is the protection chokepoint.

### C — Farm visual: new `TileKind::Cropland = 24`

Chosen over a plot-overlay layer: tiles render flat per-kind so a new variant
is the minimal render cost, road protection becomes a cheap streaming-safe
`TileKind` check, and an overlay would leave road/fertility/plant code
farm-blind — the half-measure the project rule forbids. Discriminant 24 is free
(current max 23 = `Bridge`); no renumber.

**C1. `src/world/tile.rs`:** add `Cropland = 24` after `Bridge` (:38). Add to
`is_soil_like` (:111) — this auto-makes `seed_target_tile_ok`,
`find_nearest_unplanted_*`, `seed_farmstead_yard` accept it (they branch on
`is_soil_like()`). Add `TileKind::Cropland => 1.3` to `soil_fertility_mult`
(:136). Leave `is_passable`/`is_water_like`/`is_stone_like` defaults.

**C2. `src/pathfinding/tile_cost.rs`:** `tile_speed_multiplier` add
`TileKind::Cropland => 0.9` (generic-soil speed). Cover the new variant in the
`tile_cost` test set.

**C3. Render.** `color_map.rs:5 tile_color` is an **exhaustive match (no
wildcard)** — `cargo check` will surface every exhaustive `TileKind` match that
must gain a `Cropland` arm (use this as the "every site" checklist). Pick a
color that is clearly distinct from Grass (`.35,.65,.25`), Road (`.55,.45,.35`),
**and Loam (`.42,.30,.18`)** — the field must read as a block, so a warmer,
more saturated tilled tone, e.g. `Color::srgb(0.52, 0.41, 0.16)` (golden-earth),
not the near-Loam value; finalize during the `cargo run` visual check. Add
`TileKind::Cropland` to `chunk_streaming.rs:289 RENDERABLE_KINDS` (soil group)
so `setup_tile_materials` builds its per-bucket material; verify
`FogTileMaterials` walks the same set (add there too if it has its own list).

**C4. Stamp at carve.** In `carve_plots_system` (`land.rs:588`), in the
`for (kind, rect) in zones` loop (:677), when `kind == Agricultural`, walk each
plot rect and set every `Grass | is_soil_like` tile (skip Road/Water/Wall) to
`Cropland`, emitting `TileChangedEvent`. Change `chunk_map: Res<ChunkMap>` →
`ResMut<ChunkMap>` and add the event writer. Idempotent across `culture_hash`
re-carves (Cropland is `is_soil_like` → no-op churn). Stale-plot teardown does
**not** revert Cropland (abandoned fields stay tilled — cheap, avoids
thrashing). Also stamp in `seed_starting_farms_system` (`farm.rs`, OnEnter) and
`seed_farmstead_yard` (`construction.rs:7799`, already has mut chunk access) so
seeded farms are visible from tick 0. Plant scatter (`seed_target_tile_ok`,
`plants.rs:690`) auto-accepts Cropland via `is_soil_like` — no change.

### D — Doc updates (required)

- Root `CLAUDE.md`: tile palette "23 variants" → 24; add Cropland to soils
  (tilled farm soil, stamped by `carve_plots_system`, road-protected,
  `soil_fertility_mult` 1.3, speed 0.9); drop/qualify "Farmland is removed".
- `src/simulation/CLAUDE.md`: under Organic settlement / Farming — ag belt
  (`build_ag_belt`, off-spine, fertility+contiguity+access, no home bias,
  deterministic); under Construction — road carving skips Agricultural-plot /
  planted tiles via `tile_is_farm_protected`. Fix stale "scatter onto
  `Grass | Farmland`" → "`Grass | soil_like` (incl. Cropland)".
- `src/world/CLAUDE.md`: add Cropland to `is_soil_like` / topsoil notes.
- `src/pathfinding/CLAUDE.md`: speed table — Cropland 0.9.

## Critical files

- `src/simulation/organic_settlement.rs` — A1/A2/A3, B3
- `src/simulation/construction.rs` — B2 (`road_carve_system`, `write_road_tile`,
  doormat finalize), C4 (`seed_farmstead_yard`)
- `src/simulation/land.rs` — B1 (`tile_is_farm_protected`), C4
  (`carve_plots_system`)
- `src/world/tile.rs` — C1
- `src/pathfinding/tile_cost.rs` — C2
- `src/rendering/color_map.rs` + `src/world/chunk_streaming.rs` — C3
- `src/simulation/farm.rs` — C4 seed stamp

## Risks / edge cases

- **Re-survey churn:** lattice anchored on `best_fertile_tile` (stable per
  world) keeps blocks in place across replans; re-stamp idempotent. If
  residential growth's footprint later overlaps a belt block, the block drops
  and `chief_farm_plot_assignment_system` stale-release frees farmers — accepted
  for v1.
- **Determinism:** no RNG, explicit sort, no HashMap-order use; `layout_hash`
  folds `parcels.len()` unchanged; `home_pick_seed` untouched.
- **Camps/nomadic:** untouched (frontier path; `build_ag_belt` early-returns
  with no Agricultural target).
- **MAX_PARCELS (48):** shared cap, non-ag first then belt; non-ag sum < 40 at
  24-pop so belt rarely starved.
- **1:1 plot invariant:** 16×16 emission → exactly 1 plot/parcel; existing
  `plot_size_for_zone_kinds` / `parcel_suitability_prefers_fertile_fields` tests
  unaffected.
- **Pre-existing road in belt:** `rect_clear_for_parcel` rejects the block
  (intended) — no corruption.

## Verification

1. `cargo check` (drives the exhaustive-match checklist for C3),
   then `cargo test --bin civgame` — existing
   `plot_size_for_zone_kinds`, `parcel_suitability_prefers_fertile_fields`,
   `home_pick_seed_is_process_stable`, `tile_cost` tests pass.
2. New/extended tests:
   - `organic_settlement::tests::ag_belt_outside_footprint` — flat fertile
     world + a known occupied residential rect; assert every Agricultural
     parcel rect does not overlap the footprint and has `access_tile.is_some()`,
     `frontage_edge.is_none()`.
   - Determinism: `build_parcels` twice on same brain/seed → identical ag rect
     set (order-insensitive).
   - `land`: carve test asserts Agricultural plot tiles become
     `TileKind::Cropland`, and a `road_carve_system` Bresenham through a plot
     leaves those tiles `Cropland` (not `Road`).
3. `cargo run` (NOT `--sandbox`), Neolithic 20-pop village:
   - Crop fields render as a contiguous distinct-color belt **outside** the
     house/road cluster, not striped through the grid; finalize the Cropland
     color here for legibility.
   - Roads stop at / route around the belt (1-tile gaps OK).
   - Spawn a nomadic faction → camps still get frontier ag (no regression).
   - Run several in-game days: belt stable across replans; farmers plant inside
     belt plots.
