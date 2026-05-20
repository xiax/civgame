# Settlement Realism — Comprehensive Plan

**Status:** Implemented 2026-05-19. All seven recommended approaches shipped:
maturity gate (`should_seed_civic`) + spawn-UI radio, door connector helper
(`find_door_connector_target` wired into seed walled-house and runtime door
finalize), `yard_dimensions` + `yard_tile_role`, `field_tile_role` (carve +
seed paths), anchor-driven Village/Chiefdom road additions, defensible-core
palisade envelope with spine-aligned gateways. 837/837 tests pass; new tests
cover door connector adjacency/fallback, maturity gates, yard determinism,
field perimeter bias, and chiefdom anchor secondaries.

Replaces the prior Settlement Realism Fix Plan. The prior plan's symptoms and file references hold up against the code; this version sharpens the diagnoses, fills in concrete signatures and seed sources, and tightens scope where the prior was too generic.

## Context

Seeded starts represent societies already in progress (productive farms + yards from tick 0), but visually they read as stamped geometry: every house spokes a Bresenham road back to `home_tile` (wagon-wheel), yards are tiny 2×2/3×3 fertility-200 perfect blocks, agricultural plots are monolithic 16×16 golden slabs, road skeletons are pure geometric crosses, and palisades wrap the bed bounding box ignoring storage/civic/water anchors. The fix keeps gameplay invariants (productive, reachable, protected) while making layout read as lived-in growth.

## Diagnoses confirmed against code

| Claim | Verified at | Notes |
|---|---|---|
| Seeded doors always extend to `home` | `construction.rs:7947` | Unconditional `road_carve.0.push((faction_id, doormat_tile, home))`. No nearest-road check, unlike the runtime path. |
| Runtime doormat extension still pushes home as target | `construction.rs:6097–6106` | Has `road_within(doormat, 4)` short-circuit, but when it misses, target is still `home` (not nearest planned road). |
| Yards hardcoded 2×2 / 3×3 | `construction.rs:7763–7781` | `yard_side = if era ≥ Bronze { 3 } else { 2 }`. Same value for Hut (3×3 footprint) and Longhouse (5×3 footprint) — Longhouse is undersized. |
| 16×16 agricultural plots stamp uniform Cropland | `land.rs:751–782` | Every tile in the plot rect is added to `PlotIndex.ag_tiles` and flipped to `TileKind::Cropland` if Grass/soil. |
| Palisade is a bed-bbox rectangle | `construction.rs:2957–3040` | `find_palisade_site` reads only `BedMap`, ignores Granary/Shrine/Well/Campfire and `SettlementBrain.road_segments`. Gateways centred on `hx`/`hy`, not on actual spine endpoints. |
| Roads are pure geometric crosses | `organic_settlement.rs:1279–1422` (`build_road_network`) | Hamlet = spine only (OK). Village = adds perpendicular at pop≥12 unconditionally. Chiefdom = ±18 parallel secondaries through home regardless of demand. |
| `GameStartOptions` has no maturity | `game_state.rs:73–86` | Era + population + economy + lifestyle + seed_buildings. Seeded path already bypasses `civic_milestone_allows` via `seed_mode` short-circuit (`construction.rs:4151,4223,4252,4289,4321`). |

### Subtleties

1. **At seed time `brain.road_tiles` is populated but `TileKind::Road` is not yet stamped.** `kickoff_initial_survey_system` runs before `seed_starting_buildings_system`, so `SettlementBrain.road_tiles` is the planned-segment set; carved `TileKind::Road` tiles only appear once `road_carve_system` drains the queue in FixedUpdate. Door connectors at seed time must consult `brain.road_tiles` (planned).
2. **`ag_tiles` is the road-carve protection key, not `TileKind::Cropland`.** `road_carve_system` calls `tile_is_farm_protected` which checks `ag_tiles.contains(tile)`. Varying visible tiles inside a seeded plot is safe — leave all 256 tiles in `ag_tiles` and the tile-kind variation is purely cosmetic.
3. **Plant viability for varied tiles.** `plants::seed_target_tile_ok` accepts `Grass | is_soil_like`. Any mix of Grass + Cropland + soils stays plantable.
4. **The seed path already bypasses civic milestones globally.** `Founder/Established/Developed` doesn't add a new gate — it controls which of the existing `seed_mode ||` shortcircuits stay live.
5. **`culture_hash` is the canonical determinism seed** for layout jitter. Reuse for yard/field variation; do not introduce a parallel RNG.

## Recommended approach

### 1. Door connectors aim at the road network, not at home

Replace the radial `(doormat, home)` push in both seed and runtime with a shared connector helper that targets the nearest reachable planned/carved road. Keep `home` only as a last-resort frontier fallback (and only when no spine has been planned yet).

```rust
// construction.rs, near write_road_tile
enum DoorConnectorTarget {
    Road((i32, i32)),  // a carved Road tile or planned brain.road_tiles tile
    HomeFallback,      // no road in radius; carve toward home (legacy path)
    None,              // road is already adjacent; doormat alone is the connection
}

fn find_door_connector_target(
    chunk_map: &ChunkMap,
    brain: Option<&SettlementBrain>,
    doormat: (i32, i32),
    radius: i32, // default 12
) -> DoorConnectorTarget;
```

- Search chebyshev rings 1..=radius around `doormat`.
- Prefer carved `TileKind::Road`; fall back to `brain.road_tiles` (planned).
- Validate via `placement_reachability::path_exists` with a small expansion cap (≤ 200) — the agent-faithful walkability check the seeder already trusts. Rejects candidates blocked by Wall, pre-bridge Water, reserved doormat, or blueprint footprint.
- Within radius 1: any neighbour is already a Road → return `None`; no connector needed.
- No road within radius → `HomeFallback` for runtime; for seed mode, prefer `brain.road_tiles` even if it's far (planned spine is the lived-in path) and only fall back to home when `brain.road_tiles.is_empty()`.

Wire into both call sites:
- `seed_walled_house_at` (`construction.rs:7947`): replace the unconditional push with a switch on `find_door_connector_target(.., Some(brain), doormat_tile, 12)`. Thread `Option<&SettlementBrain>` through `seed_walled_house_at` (resolve via `SettlementBrains`).
- Door blueprint finalize (`construction.rs:6097–6106`): replace the `road_within(.., 4) → push (doormat, home)` branch with the same helper. The 4-cheb suppression goes away.

Why this fixes the wagon-wheel: today every door pushes a Bresenham line from `(doormat) → (home_tile)`; eight houses in different directions ⇒ eight radial spokes converging on home. With the new helper, each door drops a ≤12-tile dogleg onto the spine, which already exists in `brain.road_tiles`. Radials collapse to a tree of short feeders.

Critical files: `construction.rs` (helper, two call sites), `placement_reachability.rs` (reuse `path_exists`).

### 2. `StartSettlementMaturity` controls civic seeding density

Add a maturity dial to `GameStartOptions`; default `Established`.

```rust
// game_state.rs
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartSettlementMaturity { Founder, Established, Developed }
// GameStartOptions { ..., pub maturity: StartSettlementMaturity }
```

Plug into the existing seed-mode bypass (`construction.rs:4151 et al`):

```rust
// Before: seed_mode || civic_milestone_allows(kind, era, peak_pop)
// After:  should_seed_civic(kind, era, peak_pop, maturity, seed_mode)
```

`should_seed_civic`:
- `Founder` + `seed_mode` → fall back to `civic_milestone_allows` (re-impose era × pop gates → Neolithic-20 starts skip Market/Barracks/Monument).
- `Established` + `seed_mode` → current behavior verbatim (Granary/Shrine/Market/Barracks/Monument by era, milestone bypassed).
- `Developed` + `seed_mode` → `Established` plus explicit "always emit Monument + Barracks + Market when era ≥ Chalcolithic regardless of pop" override.

Paleolithic/Mesolithic ignore maturity — band-camp seeder doesn't emit civics.

Critical files: `game_state.rs`, `simulation/civic_milestones.rs` (new `should_seed_civic` next to `civic_milestone_allows`), the five `seed_mode ||` sites in `construction.rs`.

### 3. Yards: dwelling-aware dimensions, keep rect, vary roles

Yards stay rectangular (the `rect_reachable_from_home` check requires it) but **roles** inside the rect vary deterministically. Differentiate Hut from Longhouse.

```rust
fn yard_dimensions(era: Era, intent: &BuildIntent) -> (i32, i32) {
    match (intent, era) {
        (BuildIntent::Hut(_), Era::Neolithic)        => (3, 4),
        (BuildIntent::Hut(_), Era::Chalcolithic)     => (4, 4),
        (BuildIntent::Hut(_), _ /* Bronze+ */)       => (4, 5),
        (BuildIntent::Longhouse(_), Era::Neolithic)  => (4, 5),
        (BuildIntent::Longhouse(_), Era::Chalcolithic)=> (5, 5),
        (BuildIntent::Longhouse(_), _)               => (5, 6),
        _ => (3, 3),
    }
}
```

Wire into `construction.rs:7763–7781` — replace the `yard_side` constant with `yard_dimensions(era, intent)`.

Tile-role variation inside the yard (`seed_farmstead_yard`, `construction.rs:8024–8117`): after the current loop, run a deterministic role pass keyed on `(culture_hash, tx, ty)`:
- `roll < 0.55` → `Cropland` at fertility 200
- `roll < 0.75` → `Cropland` at fertility 100 (fallow)
- `roll < 0.92` → `Loam` (tilled work-yard, walkable)
- otherwise → leave underlying tile (Grass / soil edge)

Helper: `yard_tile_role(culture_hash, tile) -> YardTileRole`. Mix via splitmix64; no `fastrand` in helpers. All four roles are passable; `rect_reachable_from_home` continues to pass.

Critical files: `construction.rs` (`seed_farmstead_yard`, call site, new helper).

### 4. Productive but less-perfect 16×16 fields

Current `carve_plots_system` (`land.rs:751–782`) inserts every plot tile into `ag_tiles` (good — protection) and flips Grass/soil to `Cropland` (uniform). Add a parallel role pass keyed on `(culture_hash, faction_id, tx, ty)`:

```rust
enum FieldTileRole {
    Cropland,      // planted rows
    CroplandLow,   // recently harvested, lower fertility
    SoilFallow,    // tilled but not currently cropped — keep underlying soil kind
    GrassEdge,     // unimproved field edge
}
```

Distribution: 60% Cropland / 15% CroplandLow / 20% SoilFallow / 5% GrassEdge. Grass-edge concentrated on tiles where `|dx| == 7 || |dy| == 7` (the field perimeter) by biasing the roll +0.3 in that band — turns the field's silhouette from a perfect golden square into a softened blob with stubble at the edges.

Invariants preserved:
- Every tile stays in `PlotIndex.ag_tiles` → `tile_is_farm_protected` still true → `road_carve_system` still skips it.
- All four roles are `is_soil_like || Grass` → `plants::seed_target_tile_ok` keeps the tile plantable.
- Carving is idempotent across re-runs (deterministic seed).

Apply the same variation to the OnEnter `seed_starting_farms_system` seeded plot.

Critical files: `land.rs` (`carve_plots_system`, new `field_tile_role` helper), `simulation/farm.rs` (no change).

### 5. Anchor-driven road network (replace geometric extras)

Keep the spine; replace blind perpendicular and ±18 secondaries with anchor-demand-driven additions.

In `build_road_network` (`organic_settlement.rs:1279`):

- **Hamlet:** unchanged — single spine through `home` along `primary_axis`.
- **Village:** drop the unconditional perpendicular at `member_count >= 12`. Add a perpendicular only when (a) member_count ≥ 16, OR (b) a `WaterAccess`/`Field`/`Market` anchor exists off the spine axis (projection onto `perp` > 6 tiles). Endpoint of the perpendicular = projection of strongest off-spine anchor, not symmetric `±radius`.
- **Chiefdom:** drop the symmetric `±18` parallels through `home`. Take the top-3 unmet anchors (weight-sorted, not already covered by spine endpoints), emit `Secondary` segments from `home` toward those anchors. Cap at 3 extras.
- **ProtoUrban/Urban:** keep the current grid for now (out of scope).
- **Jitter:** apply ±1 tile jitter to every segment endpoint (except the home endpoint of primary spine) using `culture_hash` so perfect cardinals are broken.

Preserve all existing river-aware behavior (`trace_crosses_river`, `same_bank_bfs`, `has_bridges`).

Critical files: `organic_settlement.rs` (`build_road_network` only).

### 6. Defensible-core palisade envelope

Replace the bed-bbox in `find_palisade_site` (`construction.rs:2957`) with an envelope over all defended structures.

```rust
fn find_palisade_site(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    granary_map: &GranaryMap,
    shrine_map: &ShrineMap,
    well_map: &WellMap,
    campfire_map: &CampfireMap,
    brain: Option<&SettlementBrain>,
    bp_map: &BlueprintMap,
    doormat: &DoormatReservations,
    camp_home: (i32, i32),
    buffer: i32,
) -> Option<(i32, i32)>;
```

- Compute the axis-aligned bbox over `BedMap ∪ GranaryMap ∪ ShrineMap ∪ WellMap ∪ CampfireMap` filtered to chebyshev ≤ 25 of `home`.
- Apply `buffer` outward (1 tile, today's default).
- **Gateways align with spine endpoints, not `hx`/`hy`.** For each wall edge (N/S/E/W), find the intersection point of the perimeter with the nearest planned/carved road segment from `brain.road_segments`; place a 3-tile gateway centred on that intersection. Fallback to `hx`/`hy` when no road crosses the edge.
- Skip Road tiles (existing guard preserved).
- Do **not** include `PlotIndex.ag_tiles` in the envelope — fields stay outside the wall by default. Exception: `StartSettlementMaturity::Developed` extends the envelope to include the nearest Agricultural plot per cardinal.

Caller: extend `BuildingMapsRO` with the four extra maps (chief was already reading most of these directly).

Critical files: `construction.rs` (`find_palisade_site`, `BuildingMapsRO`), `organic_settlement.rs` (caller).

### 7. Shared utilities and re-used existing code

- `placement_reachability::path_exists` — connector-target validation.
- `culture_hash` on `SettlementBrain` — determinism seed for yard role + field role + spine endpoint jitter.
- `BuildingMapsRO` — extend with `granary_map`/`shrine_map`/`well_map`/`campfire_map`.
- `SettlementBrain.road_tiles` / `road_segments` — planned-road surface for connectors and palisade gateways.
- `tile_is_farm_protected` — field role variation rides on it for free.

## Tests

Add to `simulation::test_fixture::onenter_era_seeding` and `organic_settlement` test modules:

- **Door connector — seed:** seed a Bronze-Established 20-pop start; every seeded house's `RoadCarveQueue` entry targets a tile in `brain.road_tiles` OR within chebyshev 1 of an existing road tile. No entry targets `home_tile` when `brain.road_tiles.len() > 4`.
- **Door connector — runtime:** finalize a runtime door blueprint with a planned spine 6 tiles away. Queued connector targets a spine tile, not `home_tile`.
- **No wagon-wheel:** 20 founders ⇒ ≤ 1 `RoadCarveQueue` entry with `home_tile` as the `to` argument.
- **Maturity:** `(Neolithic, 20, Founder)` skips Market/Barracks/Monument; `Established` matches current; `(Chalcolithic, 20, Developed)` emits Market+Barracks despite under-threshold pop.
- **Yards:** Hut-Neolithic stamps a 3×4 yard; Longhouse-Bronze stamps a 5×6 yard. Both rectangles pass `rect_reachable_from_home`. Tile roles deterministic across reruns.
- **Fields:** identical `culture_hash` ⇒ identical role mosaics. `ag_tiles.len() == 256` per seeded plot regardless of variation.
- **Palisade envelope:** Granary 8 tiles N and Well 6 E of bed cluster ⇒ perimeter extends to include both. Gateways align with spine intersection tiles.
- **Existing tests stay green:** `onenter_era_seeding::*`, `organic_settlement::*`, `tile_is_farm_protected` (`land.rs:1592`).

## Verification

- `cargo check` — clean.
- `cargo test --bin civgame` — full suite green; new tests pass.
- `cargo run` — start a Bronze-Established 20-pop player; verify visually:
  - House doors connect to spine via short doglegs, not radial spokes to base.
  - Yards distinguishable by dwelling type; field interiors visibly mottled (planted/fallow/soil/edge).
  - Palisade wraps homes + granary + shrine; 3-tile gateways align with the spine.
  - Try `Founder`, `Established`, `Developed` via spawn UI (small `bevy_egui` addition).
- Skip `--sandbox` verification per project convention.

## Out of scope

- Composite L-shape / U-shape footprints.
- ProtoUrban / Urban grid replacement.
- Per-faction architectural style differentiation.
- Spawn-select UI polish for maturity (minimal radio is enough).
