# Route-Aware Organic Residential Planner

## Context
Residential placement in the organic settlement planner occasionally drops houses on lots whose *walked* path back to the faction center is much longer than the chebyshev distance it scored on, producing "stragglers" connected by a thin corridor through awkward terrain. The fix keeps the existing organic / pressure-driven model (no fixed layouts, no grid prescription) and adds a route-quality term to residential scoring, an axis choice for Longhouses, and a path-aware tiebreaker for door cardinals.

## Why not a single home-rooted Dijkstra
At `RUNTIME_MAX_EXPANSIONS = 4_000` an 8-connected flood from one source reaches ~radius 35 tiles (~50 m at 1.5 m/tile per `[[project_tile_scale]]`). Mature cities sprawl past that. Raising the cap to cover them would burn 1+ MB and ~5-10 ms per planning tick.

What we actually want to measure is *connection quality to the network people already walk on*, which is **road-graph distance + a small off-road spur**, not geodesic distance through wilderness.

## Key Files
- `src/simulation/placement_reachability.rs` (522 LOC) — `path_exists` @ 105, `tile_reachable_from_home` @ 202, caps `SEED_MAX_EXPANSIONS=12_000` / `RUNTIME_MAX_EXPANSIONS=4_000` / `DOORMAT_MAX_EXPANSIONS=1_500`. 8-connected with z-probe `[0,+1,-1]`. Tests @ 375–521.
- `src/simulation/organic_settlement.rs` (6116 LOC) — `pressure_to_intent` @ 3291, `choose_site_for_intent` @ 3447, residential scoring @ 3463–3506 + 3507–3533, `home_radial_fallback` @ 3572–3665, `OrganicBuildKind::Longhouse(WallMaterial)` @ 393, frontage gate @ 3474, frontage bonus +8 @ 3495, doormat-reservation read @ 3762, Longhouse footprint `(2,1)` hardcoded @ 3602–3604.
- `src/simulation/construction.rs` (8649 LOC) — `BuildIntent::Longhouse(WallMaterial)` @ 2768, hardcoded `plan_building(.., 2, 1, ..)` @ 3158, `plan_reachable_from_home` @ 2526, `pick_clear_door_cardinal_filtered` @ 4097, squared-distance fallback ranking @ 4130, `find_door_connector_target` @ 4185, `doormat_reaches_home` @ 4048. Tests @ 7942–8631.

## Algorithm

### 1. `RoadField` — bounded BFS along the road graph
In `placement_reachability.rs`:
```rust
pub struct RoadField {
    pub road_steps_to_home: AHashMap<(i32, i32), u16>, // road tile → step count
    pub home_road_tile: Option<(i32, i32)>,
}
pub fn road_field_from_home(chunk_map: &ChunkMap, brain: &SettlementBrain, home: (i32,i32,i32)) -> RoadField;
```
- Seeds = every carved `Road` tile **and** every planned tile in `brain.road_tiles` (so seed-mode works before roads carve).
- BFS along road-only adjacency. Cost is O(road tiles) — typically a few hundred even in large cities. Safety cap `MAX_ROAD_TILES = 8_000`.
- If `home` itself is road, that's the root; else nearest road within ring ≤ 8 (tiny radial search).

### 2. `nearest_road_cost` — bounded off-road A* per candidate
```rust
pub fn nearest_road_cost(
    chunk_map: &ChunkMap,
    field: &RoadField,
    from: (i32, i32, i32),
    max_steps: u16,  // 24 (~36 m); farther = too remote
) -> Option<(u16, (i32, i32))>;
```
- Reuses existing `expand_neighbors` (8-conn, z-probe, blocked-overlay) extracted from current A*.
- Worst case ≤ ~600 expansions per candidate.

### 3. `PathStats` + `path_stats`
```rust
pub struct PathStats {
    pub off_road_steps: u16, pub on_road_steps: u16, pub total_steps: u16,
    pub direct: u16, pub detour: i32, pub detour_ratio: f32,
    pub saturated: bool,
}
pub fn path_stats(chunk_map: &ChunkMap, field: &RoadField, candidate: (i32,i32,i32), home: (i32,i32)) -> Option<PathStats>;
```
- `off + on` → `total`. `direct = chebyshev(candidate.xy, home).max(1)`.
- `None` when off-road exceeds `max_steps` or the road tile reached is in a disconnected fragment (`u16::MAX`).
- **Empty-road fallback**: if `field.road_steps_to_home` is empty (very first seed ticks), use a single bounded A* `home → candidate` — same caps as legacy `path_exists`. Activates only until the first road tile exists.

### 4. Keep `path_exists` for non-planner callers (unchanged)
Construction's `find_door_connector_target` adjacency probes and tests stay on `path_exists`. The `RoadField` path is a planner optimisation, not a replacement.

### 5. Per-tick caching in the planner
In `settlement_morphology_system` (~1027), build **one** `RoadField` per faction per planning tick into a per-tick scratch resource (mirror `DoormatReservations`). Thread `&RoadField` through `pressure_to_intent` → `choose_site_for_intent` → `home_radial_fallback`.

**Cost ceiling per tick**: 1 road BFS + ≤ 32 candidates × ~600 local expansions ≈ ≤ 25K expansions. Independent of city radius.

### 6. `SiteChoice` (replaces `(score, tile)` for residentials)
```rust
struct SiteChoice {
    tile: (i32, i32),
    build_kind: OrganicBuildKind,
    door_dir: TileEdge,
    axis: HouseAxis,         // EastWest for Hut; both arms for Longhouse
    route_stats: PathStats,
    score: f32,
    is_last_resort: bool,
}
```
Per parcel, enumerate `(axis, cardinal)`: Hut = 1×4, Longhouse = 2×4. Gate each on existing checks (commons keepout, footprint clear, doormat reservation, blocked-overlay), then call `path_stats`. Keep best per parcel, then best across parcels. **Cap parcels evaluated per tick at top-32 by `suitability × band`** so the planner stays bounded when brain has many candidates.

### 7. Scoring — named constants
```rust
const ROUTE_BASE_BONUS:                f32 = 45.0;
const ROUTE_DETOUR_PENALTY_PER_TILE:   f32 = 2.0;
const ROUTE_DETOUR_RATIO_KNEE:         f32 = 1.35;
const ROUTE_DETOUR_RATIO_PENALTY:      f32 = 35.0;
const ROUTE_DETOUR_RATIO_LAST_RESORT:  f32 = 2.75;
const ROUTE_SATURATED_PENALTY:         f32 = 80.0;
```
```
score = suitability × 100 × band
      + frontage_bonus           // existing +8
      + spread / material adj    // unchanged
      + route_score
route_score = ROUTE_BASE_BONUS
            - detour × ROUTE_DETOUR_PENALTY_PER_TILE
            - max(0, ratio - ROUTE_DETOUR_RATIO_KNEE) × ROUTE_DETOUR_RATIO_PENALTY
            - (saturated ? ROUTE_SATURATED_PENALTY : 0)
```
Lands in 0–45 — meaningful next to suitability×100×band (50–300) and frontage (+8), still dominated by material scarcity (−200/−600). If `ratio > LAST_RESORT` and any other `SiteChoice` has `ratio ≤ knee`, mark `is_last_resort` and only pick if nothing else passes.

Note `detour` and `ratio` use `total_steps` (off + on), so a lot 15 tiles off the road scores worse than a lot 2 tiles off even with equal on-road portions.

**Tie-break (deterministic)**: when two `SiteChoice` score within `0.5`, prefer lower `(tile.0, tile.1)` lexicographically.

### 8. Seed-mode `home_radial_fallback` (3572)
`road_field_from_home` already seeds from `brain.road_tiles`, so the same `path_stats` works at seed time. If even the planned spine is empty, §3 fallback handles it. Don't early-return on first ring hit — collect ring's best, then compare against `max_r/2` further rings so a clearly cleaner outer candidate can beat a barely-passing inner one.

### 9. Construction-side (`construction.rs`)
- **Longhouse axis**: `OrganicBuildKind::Longhouse(WallMaterial)` → `Longhouse { wall_material, axis: HouseAxis }`. Same for `BuildIntent::Longhouse`. Grep both enum names and update every match arm — no silent `_ =>` defaults.
- `plan_building` @ 3158: `(half_w, half_h) = match axis { EastWest => (2,1), NorthSouth => (1,2) }`.
- **Door cardinal fallback** @ 4130: replace squared-distance ranking with `path_stats(..., doormat_3d, home).total_steps`. Fall through to the old squared-distance rule only when no `RoadField` is available (seed-time stamping before any roads).
- **`find_door_connector_target`** @ 4185: rank road candidates by `chebyshev(doormat → road) + road_field.road_steps_to_home[road]`, ascending. A farther road on the main flow now beats a near road that requires backtracking.
- `plan_reachable_from_home` @ 2526: unchanged — final authoritative gate.

## Migration Notes
- `OrganicBuildKind::Longhouse` + `BuildIntent::Longhouse` rename is hard-typed; `cargo check` will catch every site.
- Planner output isn't serialized; no save-compat concern.
- Multiplayer determinism: planner runs server-side only; tie-break rule keeps placement reproducible even if planning ever migrates client-side.

## Tests (`cargo test --bin civgame`)
- `placement_reachability::road_field_from_home`: straight road, T-junction, disconnected fragment absent, planned-spine-only seed.
- `placement_reachability::nearest_road_cost`: adjacent (1, road), far (None), wall-sealed (None).
- `placement_reachability::path_stats`: 2-off+10-on case, 5-off+20-on flagged last-resort, empty-roads fallback matches legacy `path_exists`.
- `organic_settlement` (fixture `flat_map()` + one wall column):
  - two equal-chebyshev parcels, only one direct → direct wins.
  - multi-frontage parcel: blocked cardinal loses to clear cardinal.
  - Longhouse rotates to NorthSouth when only that axis produces a clean route to the road.
  - tie-break: identical scores → lexicographic winner stable across runs.
  - radial fallback: ring 4 with ratio 1.0 beats ring 3 with ratio 2.5.
- `construction`: door fallback picks lowest `total_steps` cardinal; `find_door_connector_target` picks lower combined-cost road.

## Manual Verification
- `cargo run` Settled start → first three growth cycles: no Hut placed where doormat→home route exceeds chebyshev × 2.0 if a cleaner same-radius lot exists.
- `cargo run` riverside start → Longhouse rotates to align long axis with road toward commons.
- Skip `--sandbox` per `[[feedback_testing]]`.

## Documentation
- Add to `src/simulation/CLAUDE.md` settlement section: "residential candidate scoring is route-aware via a per-tick `RoadField` (road-graph distance to home) + bounded off-road A* per candidate; cost independent of city radius."
- Top-level `CLAUDE.md` unchanged — new APIs are subsystem-internal.

## Acceptance
- No new house has `total_steps > chebyshev × 2.0` when a same-radius alternative exists.
- Growth still emerges from pressures / parcels / anchors / traffic / roads.
- Planner never emits a residential intent whose doormat fails `plan_reachable_from_home`.
- Startup seeding produces the expected bed count.
- Non-residential placement unchanged except for shared helper refactors.
- Per-tick planner cost ceiling holds for cities of any radius.
- `cargo test --bin civgame` green including new tests.
