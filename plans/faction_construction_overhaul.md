# Faction Construction Overhaul — Door + Road + Organic Layouts

## Status: shipped 2026-05-12

Fixes three door-related correctness bugs and adds organic random layouts on top of the existing plot/frontage scaffolding.

## What changed

- **Door direction is sourced from `Plot.frontage_edge`** (or `TileEdge::toward(centre, home)` fallback). Threaded through `BuildCandidate.door_dir → BuildIntent → Blueprint.door_dir → Door.dir`. `entrance_cell_for_edge` picks the centre cell of the chosen side, never a corner. Replaces the old "closest perimeter tile to home" rule in `plan_building`, `seed_walled_house_at`, `seed_perimeter`, `seed_perimeter_rect`.
- **`Door { dir, doormat_tile }`** — new fields. Doormat = 1-tile cardinal-outside neighbour. Every door finalisation writes its doormat to `TileKind::Road` directly (`write_road_tile`) and pushes a Bresenham extension `(doormat → home)` onto `RoadCarveQueue` so doors connect to the spine.
- **`DoormatReservations` resource** (`simulation/doormat.rs`) keyed by tile. Honoured by `is_clear_footprint`, `plot_rect_vacant`, `find_palisade_site`, `find_clear_tile_in_zone`, `find_unfilled_civic_zone_tile`, `find_bed_tile_around_hearth`, `seed_farmstead_yard`, and `seed_walled_house_at`'s preflight. Lives on `BuildingMapsRO` (read-only SystemParam bundle).
- **`Door::on_remove` hook** registered in `SimulationPlugin::build` alongside the `JobEscrow` hook — drops the doormat entry when a door despawns.
- **Organic layout randomness** — `SettlementPlan.culture_hash` doubles as the `fastrand::Rng::with_seed`. `jitter_zones` adds ±1 tile offset to non-civic / non-defense zone rects. `generate_candidates` rolls Hut-vs-Longhouse weighted (forced Longhouse when `bed_deficit ≥ 4`, else 60/40).
- **Composite footprints** — `BuildIntent::CompositeHouse { shape, rotation, wall_material }` wired through `plan_composite_building` which walks `building_template::shape_tiles` and classifies cells as Wall / Door (chosen frontage cardinal, closest to home) / Bed. Chalcolithic+ residential rolls 10% LShape when `bed_deficit ∈ [2,4)`. UShape plumbing ready but not yet auto-emitted.

## Files touched

- `src/simulation/construction.rs` — `Door`, `Blueprint`, `BuildCandidate`, `BuildIntent::CompositeHouse`, `plan_building`, `plan_composite_building`, `seed_walled_house_at`, `seed_perimeter`, `seed_perimeter_rect`, `entrance_cell_for_edge`, `write_road_tile`. `is_clear_footprint` and friends now take `&DoormatReservations`. `BuildingMapsRO` extended with `doormat`.
- `src/simulation/doormat.rs` — new module: `DoormatReservations` resource, `DoormatEntry`, `release_doormat_on_door_remove` hook.
- `src/simulation/land.rs` — `TileEdge::delta()` + `TileEdge::toward()` helpers.
- `src/simulation/settlement.rs` — `jitter_zones` post-pass in `build_settlement_plan`.
- `src/simulation/terraform.rs` — `PendingFootprint.wall_plan` carries `Option<TileEdge>` per tile so deferred door blueprints retain their direction.
- `src/simulation/mod.rs` — module declaration + resource registration + `Door` `on_remove` hook.
- `CLAUDE.md` + `src/simulation/CLAUDE.md` — updated.

## Tests

503 tests pass (497 baseline + 6 new):
- `land::tests::tile_edge_delta_cardinal_directions`
- `land::tests::tile_edge_toward_picks_dominant_axis`
- `doormat::tests::is_reserved_returns_true_for_inserted_tile`
- `construction::tests::entrance_cell_picks_centre_of_chosen_side`
- `construction::tests::entrance_cell_never_corner_for_3x3`
- `construction::tests::entrance_cell_longhouse_centres_along_long_side`

## Verification recipe

1. `cargo test --bin civgame` — passes.
2. `cargo run` — start a Neolithic Mixed faction with `player_population = 20`. Visually verify huts arranged along carved roads, doors face the road, no door blocked by neighbour wall / palisade / yard. Two separately-launched games (different `faction_id`) produce visibly different layouts.
3. Chalcolithic start — verify palisade gateways no longer collide with hut doors, occasional LShape farmstead appears in residential rows.

## Round 3 — courtyard pockets + thoroughfares + gateway choke (post-screenshot 2)

User reported: houses in dead-end courtyards (door opens into a sealed pocket), buildings placed across road corridors, only one exit through the palisade.

- **Doormat reachability BFS** (`doormat_reaches_home`, 1500-node bounded). `pick_clear_door_cardinal` only accepts a cardinal whose doormat can BFS-reach the faction home through passable terrain. Sealed courtyards fail; the door tries another cardinal or the build aborts.
- **Roads protected from new construction.** Added `TileKind::Road` rejection to `is_clear_footprint`, `is_clear_shape`, `pick_seed_house_anchor`, `seed_walled_house_at` preflight, `find_palisade_site`, `seed_perimeter`, `seed_perimeter_rect`. Thoroughfares stay open.
- **3-tile palisade gateways** on each cardinal axis (was 1 tile). `find_palisade_site`, `seed_perimeter`, `seed_perimeter_rect` all widen the gap to `gateway_half = 1` (so the gateway spans `[centre - 1, centre + 1]`). Multi-agent traffic flows naturally instead of choking through a single tile.

## Round 2 — door blockage + road sprawl fixes (post-screenshot)

User reported visible issues: roads pave the entire settlement, several doors are still blocked, some homes have no path to the faction centre.

- **Per-door Bresenham extension is gated.** `road_carve_system` only enqueues the doormat→home line when no existing `TileKind::Road` sits within 4 chebyshev of the doormat (`road_within` helper). Doormat tile itself is still always written to Road. This stops 20+ overlapping spokes from paving every interior tile when a Bronze Age village has many houses.
- **`pick_clear_door_cardinal` selects a door cardinal whose doormat is actually clear.** Tries the preferred cardinal (from `frontage_edge` or `toward(home)`); if that doormat is Wall / Stone / Blueprint / Bed / impassable / reserved by another door, falls back to the next-best cardinal ranked by chebyshev to home. Returns `None` if *every* cardinal is blocked — caller aborts the build (returns false from `seed_walled_house_at`, returns early from `plan_building`/`plan_composite_building`) rather than placing an unreachable door.
- **`doormat_tile_clear` is the single source of truth** for "can this tile serve as a doormat?" — passable, not Wall/Stone, not blueprinted/beded, not reserved.
- **Seed-loop continues on failed anchor** instead of breaking. Failed centre tile gets stamped into `used` so `pick_seed_house_anchor` won't return the same spot, and the seeder moves on to the next anchor. Previously a single blocked door would abort all subsequent house placement.

## Deferred

- UShape courtyard houses (plumbing ready, no chief auto-emission).
- Full LShape footprint reachability (`is_clear_shape` shape-aware variant) — current code uses a Rect bounding-box `find_footprint_in_zone` for the LShape candidate and lets `plan_composite_building` overlay the actual mask. Some 3×3 corners may sit on impassable terrain in edge cases; revisit if it becomes a problem.
- Game-start synchronous planner + plot integration. Achieved goal (door+road connection at seed time) through doormat+carve instead; deeper refactor wasn't worth the schedule complexity.
