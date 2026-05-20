# AGENTS.md

Guidance for Codex working in this repository.

## Commands

```bash
cargo run                  # Run the game
cargo run -- --sandbox     # Sandbox mode (5×5 chunk map, one of every entity)
cargo build --release      # Optimized build
cargo check                # Fast type check
cargo test --bin civgame   # Run tests (binary crate — `cargo test` alone errors)
```

## Game-start options

`GameStartOptions` (resource in `game_state.rs`) drives the spawn-select screen and is read once by `spawn_population` + `seed_starting_buildings_system`:
- `era: Era` — every spawned member starts with all techs through this era Aware+Learned (`PersonKnowledge::seeded_through_era`); structures and walls scale up accordingly.
- `player_population: u32` — group size for `group_idx == 0` (player faction). Other factions stay at hardcoded `GROUP_SIZE=20`.
- `economy: EconomyPreset` — `Subsistence` (empty policy map = all-communist), `Mixed` (`mixed()` on non-staples; chief still allocates food/wood/stone), `Market` (`capitalist()` on every catalog resource). Applied per-faction in `spawn_population` via `policy::apply_preset`.
- `lifestyle: Lifestyle` — `Settled` (default) or `Nomadic`. Only the player faction reads it; AI factions stay Settled.
- `seed_buildings: bool` — sandbox sets false to skip pre-built seeding.

`spawn_population` sets `FactionData.chief_entity` and inserts `FactionChief` on the first spawned member.

## Architecture

CivGame is a Dwarf Fortress-style civilization simulation on **Bevy 0.15** (ECS). Six plugins:

| Plugin | Directory | Responsibility |
|--------|-----------|----------------|
| `WorldPlugin` | `src/world/` | Procedural terrain (Z-levels -16..+15), chunk streaming (32×32 tiles), biomes, calendar, `SpatialIndex`, `ResourceCatalog` |
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plants, raids |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Component-typed chunk graph, hotspot flow fields |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera, chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, world map, right-click menu, activity log |

Per-directory notes cover subsystem detail. This repo currently keeps the simulation notes in `src/simulation/CLAUDE.md`.

## Settlement construction

Settlement layout now flows through `organic_settlement.rs`: `SettlementBrain` surveys practical anchors and traffic, pressure systems emit ranked `ConstructionIntent`s, and `chief_directive_system` executes the selected intent through the existing `Blueprint` / `Projects` / `JobBoard` backend. `SettlementPlan` remains as a compatibility/debug projection for overlays and legacy plot carving; `SettlementParcelIndex` is the authoritative organic parcel surface for future placement.

- **`Settlement.peak_population`** — monotonic max of owner-faction's `member_count`, maintained by `settlement_peak_population_system`. Drives civic milestones.
- **`SettlementBrain`** — one per full settlement. Tracks `phase`, anchors (hearth, water, storehouse, field, shrine, workshop, market, gate, high ground, material patch), soft districts, traffic heat, planned `road_segments`/`road_tiles`, frontier tiles, organic parcels, deterministic seed, and survey ticks. Survey/pressure/morphology/project-selection systems run in `SimulationSet::Economy`.
- **Organic parcels + compatibility projection** — `build_parcels` allocates practical lots using phase/tech quotas so civic cores, homes, fields, craft/storage, markets, shrines, and defenses do not crowd each other out. Permanent settlements build parcels from immediate road frontage and reject planned road tiles before the roads are physically carved. `compat_plan_from_brain` projects parcels/districts into `SettlementPlan` and preserves legacy listable Residential/Agricultural/Crafting/Storage zones when organic coverage is still sparse, keeping land tenure stable during the transition.
- **`StreetSpine`** — `None` / `Linear` / `Spokes` / `Grid`; the organic planner builds a road skeleton first (`build_road_network`) from the settlement core, phase, anchors, and traffic heat, then projects those segments through `SettlementPlan`. `settlement_planner_system` enqueues projected spine segments into `RoadCarveQueue`; `road_carve_system` lays Bresenham road tiles.
- **Plots** carry `frontage_edge` + `access_tile`; residential placement (Hut/Longhouse) can still use vacant frontage lots so doors face the road, while organic intents carry `door_dir` from parcel frontage when available.
- **Door direction is sourced.** `Door` carries `dir: TileEdge` + `doormat_tile`. `plan_building` reads `door_dir` from the matched plot's `frontage_edge` (carried through `BuildCandidate.door_dir → BuildIntent → Blueprint.door_dir`) so the entrance always lands on the road-facing wall at the centre cell, never a corner. Fallback when no plot frontage is `TileEdge::toward(centre, home)`. `entrance_cell_for_edge` computes the door cell from `(half_w, half_h, edge)`.
- **Doormat reservations** (`simulation/doormat.rs`): every door registers its 1-tile cardinal-outside neighbour in `DoormatReservations`. `is_clear_footprint` / `plot_rect_vacant` / `find_palisade_site` / `find_clear_tile_in_zone` / `find_unfilled_civic_zone_tile` / `find_bed_tile_around_hearth` / `seed_farmstead_yard` all refuse reserved tiles, so no neighbour wall / palisade / yard can sit on a door's opening side. Door spawn writes the doormat tile to `TileKind::Road` directly (`write_road_tile`); a Bresenham extension `(doormat → home)` is pushed onto `RoadCarveQueue` *only* when no existing Road sits within 4 chebyshev of the doormat (`road_within`), so dense villages don't pave the interior with overlapping spokes. The `Door` `on_remove` hook (registered alongside `JobEscrow`'s) frees the reservation on demolition.
- **Door direction is verified, not assumed.** `pick_clear_door_cardinal` tries the preferred cardinal (frontage_edge or `toward(home)`); if that doormat is Wall/Stone/Blueprint/Bed/impassable/reserved, falls back to other cardinals ranked by doormat→home chebyshev. Returns `None` if every cardinal is blocked — `plan_building` / `plan_composite_building` / `seed_walled_house_at` then abort the build rather than place an unreachable door. The seed loop continues on failure (stamping the failed anchor into `used`) instead of breaking, so one blocked candidate doesn't abort all subsequent house placement.
- **Doormat reachability gate.** `doormat_reaches_home` runs a bounded BFS (1500-node cap) from each candidate doormat through passable terrain to the faction's home tile. Cardinals whose doormats are locally clear but sealed inside a courtyard pocket fail this check, so dead-end house clusters never get built.
- **Roads are protected from new construction.** `is_clear_footprint` / `is_clear_shape` / `seed_walled_house_at`'s preflight / `pick_seed_house_anchor` / `find_palisade_site` / `seed_perimeter` / `seed_perimeter_rect` all reject `TileKind::Road` candidates. Organic intent placement and the legacy chief fallback also reject `SettlementBrain.road_tiles`, so a hut can't plant its wall straight across a planned or carved spine.
- **Wider palisade gateways.** All three palisade carvers (`find_palisade_site` chief loop, `seed_perimeter` home-centred ring, `seed_perimeter_rect` Defense-zone ring) leave a 3-tile gap on each cardinal axis instead of a single-tile gap, so the spine has real flow capacity through the wall.
- **Civic milestones** (`civic_milestones.rs`): `(Era, peak_pop) → bool` table gates Granary / Shrine / Market / Barracks / Monument growth. Seeded structures bypass the gate.
- **`building_template.rs`** ships `FootprintShape::{Rect, LShape, UShape}` + `Rotation` helpers. `BuildIntent::CompositeHouse { shape, rotation, wall_material }` is wired: `plan_composite_building` walks `shape_tiles` to emit perimeter Wall blueprints + one Door at the chosen frontage cardinal + interior Bed blueprints. LShape/UShape support exists in `shape_tiles`, but automatic LShape shelter emission is disabled until the template has true interior bed cells; settled growth currently uses Huts/Longhouses.
- **Organic layout determinism** (`organic_settlement.rs`): `organic_seed(settlement, faction)` and `SettlementBrain.layout_hash` make surveys, parcel quotas, desire paths, and Hut/Longhouse/composite-house choices deterministic for the same faction/terrain while different culture seeds diverge.
- **Game-start seeding** (`seed_starting_buildings_system`) is era-additive and runs through the shared `generate_candidates` stream for Neolithic+. Seed mode uses a tech bitset through `GameStartOptions.era`, stamps era-appropriate Bed/Campfire/Door/Workbench tiers, and bypasses civic milestone gates so Bronze starts can receive Market/Barracks/Monument at low founder population. Paleo/Meso still use the multi-hearth band camp via `paleolithic_hearth_positions`; Neolithic+ stamps walled Huts/Longhouses with tilled yard fertility; Chalcolithic+ adds palisade walls and Bronze upgrades walls to Mudbrick with 3×3 yards. The seed loop tries the full ranked candidate list each pass and relocates blocked single-tile/residential structures nearby, with yards attached to the actual stamped house anchor, so one blocked high-score house/civic anchor cannot starve later intents or starting bed capacity.
- **Child farm plots** (`Plot.parent_plot`): residential plot acquisition also claims the nearest unowned same-village Agricultural plot within 12 tiles — same household, mirrored tenure (rent flows through the parent).
- **Farming startup + posting** (`farm.rs`, `jobs.rs`): `seed_starting_farms_system` creates a 16×16 `Agricultural` plot and places `STARTING_GRAIN_SEEDS=32` as physical `GroundItem`s on faction storage, because `FactionStorage.totals` is only a rollup cache. Chief Farm postings are gated by crop tech, grain policy, members, and available seeds — not by low stored grain — so stocked societies still plant the next cycle. `chief_job_posting_system` emits one OPEN plot-scoped posting per uncovered state-owned Agricultural plot (capped by remaining workforce), so a fresh village with three carved plots gets three concurrent postings rather than a single bootstrap fallback.
- **Kitchen gardens + relaxed Farm scorer** (`organic_settlement::append_kitchen_gardens`, `goal_scorers::FarmWorkScorer`): every Residential parcel without a belt parcel within 12 tiles gets a 4×4 Agricultural parcel emitted behind its back edge; the existing 12-tile child-claim path then binds it as the household's child plot. `FarmWorkScorer` now fires for any `HouseholdMember` whose household holds an Agricultural plot — not only `Profession::Farmer` — so a Mason in a kitchen-garden household can still tend their own crops. Farmers keep a small score lift + skill/EV advantage.

## Nomadic mode

`Lifestyle::{Settled, Nomadic}` on `FactionData`; nomadic factions skip Settlement spawning, run a camp pipeline, and migrate seasonally. Two coexisting flows: AI caravans survey, pack, physically travel, unload, and pitch (`Surveying → PendingCommit → PackingCamp → Traveling → PitchingCamp → Idle`), while player-controlled nomadic factions transition between `CampState::Pitched` and `CampState::Packed` via `PlayerCommand::PackCamp` / `PitchCamp`. Detailed system list lives in `src/simulation/CLAUDE.md` (search `Camp lifecycle`, `nomad`, `wild_herd`, `pack_deploy`, `sedentary_collapse`).

- **Shelters** (`pack_deploy.rs`): `Deployable { packed_form, refund_pct, refund_resource, refund_qty }` on every nomadic structure. Bedroll (1 skin + 2 wood) and Yurt (8 wood + 6 skin, gated on `PORTABLE_DWELLINGS`) are `fully_packable`; Tent (6 wood + 3 skin) is `refund_only(0.5, wood, 6)` — half drops as `GroundItem`s on teardown.
- **Camp seeding** (`seed_nomadic_camp`): hearth ring, bedrolls (radial 2..=5, one per founder), tents (outer 5..=7, ~1 per 4 founders), yurts (Neolithic+, inner 3..=5, capped 2).
- **Migration** (`nomad.rs`): `nomad_migration_system` (Economy, daily) scores composite candidates — food cluster density, wild herd aggregate, water proximity (`score_water`), biome-season fit (`score_biome_season`), predator danger, recency penalty (`recent_camps` deque cap 6). On trigger sets `pending_migration`. `nomad_migration_commit_system` now drives AI caravan phases: validate final target while still `PendingCommit`, stamp `PackingDuty` and reuse `UnpitchStructure` labor for deployables plus no-cargo campfires, keep `home_tile` at the old camp during `Traveling`, then move `FactionData.home_tile` / `Camp.home_tile` only when `PitchingCamp` starts. `MigrationTarget.tile` stays the final camp; temporary reroutes use `route_tile` and never count as camps.
- **Pack animals**: `PackAnimalInventory { items: [(ResourceId, u32); 6], capacity_g }` on `Tamed` (Horse 60kg, Cow 80kg, Pig 30kg, Dog 15kg). `attach_pack_inventory_system` inserts on `Added<Tamed>`. `compute_faction_storage_system` folds tamed-animal inventories into nomad faction `storage.totals`. `combat::death_system` drops contents as `GroundItem`s on death.
- **Tamed-animal herding**: caravan departure stamps `FollowingBand` and overwrites `AnimalAI.target_tile` for every owner-faction animal so the herd physically walks to the new camp; arriving pack animals unload camp cargo near the final anchor.
- **Wild herds** (`wild_herd.rs`): `WildHerdRegistry` rows `(species, aggregate_count, leader_tile, range_center, bloomed, members, flee_until_tick, last_birthed_tick)`. Daily migration drifts leaders within `HERD_RANGE_HALF = 30` (Winter shifts south, Spring north); predator flee dominates (any wolf within `HERD_FLEE_RADIUS = 8` biases drift away ×3 for a day); water seek + nomad camp avoidance otherwise; non-Winter seasonal birth +12 capped at 200. Bloom/collapse at camera focus distance (32 / 48) spawns up to 60 individual entities; predation removes members so a hunted herd shrinks across cycles.
- **Slim chief directives** (`nomad_chief_directive_system`): nomadic factions retain a narrow chief role queueing Bedroll / Tent / Yurt blueprints (no postings) toward `nomad_shelter_targets(members)`.
- **Band redistribution** (`nomad_pool.rs`): `nomad_band_pool_balance_system` (every TICKS_PER_DAY/4) shrinks max-min holding spread of essentials (bedroll / packed_yurt / preserved_meat) to ≤1 unit across the band.
- **Preserved meat ration**: `2 meat + 1 wood → 3 preserved_meat` (CraftRecipe 12, gated on `FOOD_SMOKING`). `eat_task_system` runs two passes — fresh first, preserved only when nothing else on hand.
- **Sedentarization** (`nomad_sedentarize_system`): nomadic faction with `≥ 12` members + no pending migration + stable for one game-year emits `SettlementLifecycleEvent::SwitchArchetype` to the settled variant.
- **Reverse collapse** (`sedentary_collapse.rs`): settled faction sampling population crash + food deficit + shelter loss bumps `collapse_streak`; at one season triggers `SwitchArchetype` to the nomadic variant.

## Land ownership

`Plot`-based tenure layer over the compatibility `SettlementPlan` projection; see `src/simulation/CLAUDE.md::Land ownership` for schema. Plots start `Tenure::StateOwned`; in Mixed/Market presets the listing system publishes them, households acquire what they can afford, rent collects monthly with eviction after two misses, and sharecrop plots split harvest yield between tenant and landlord at gather time. Settlement-expansion plot diff (Phase 8) deferred.

## Simulation scheduling (`SimulationSet`)

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy (needs, mood, LOD, goal updates, animal sensing)
- **ParallelB** — HTN dispatchers; `goal_dispatch_system` is the stale-reset / Explore-cleanup catch-all
- **Sequential** — mutating, ordered: `gather` → `dig`/`construction` → `movement` → `combat` → `production`
- **Economy** — gossip, faction storage rollup, reproduction, raids, technology, market prices

## Spatial / tile / rendering conventions

- World tiles: `(i32, i32)`; convert with `tile_to_world()`.
- Chunks: `ChunkCoord::from_world()` (uses `div_euclid()`).
- Z-levels: `i8`, `Z_MIN=-16`, `Z_MAX=15`.
- `TileKind::River` is the freshwater sibling of `TileKind::Water`. Both are impassable; `kind.is_water_like()` accepts either, `kind.is_freshwater()` is River-only. Lakes/oceans stay `Water` until `LakeBasin` learns a fresh/salt flag. `chunk_map.river_distance_at(tx, ty)` returns chebyshev tiles to nearest river (`u8::MAX` = far / unloaded), populated at chunk-gen and used by riparian biome shift, fertility boost, settlement-spawn scoring, and herd/nomad fresh-water preference.
- **Tile palette:** `TileKind` carries 23 variants. **Surfaces:** `Grass`, `Forest`, `Sand` (hot/dry; reuses Farmland's u8 slot), `Snow` (tundra), `Marsh` (wetland, slow path), `Scrub` (steppe / arid), `Water`, `River`, `Road`. **Stone lithologies** (`is_stone_like()`): `Stone` (legacy/fallback), `Granite`, `Limestone` (yields 3 vs. 2 per swing), `Sandstone`, `Basalt`, plus underground `Wall` and `Ore`. **Soil variants** (`is_soil_like()`): `Dirt` (legacy), `Loam` (1.5× fertility), `Silt` (1.4×, riparian), `Clay`, `SandySoil` (0.6×). Helpers: `is_stone_like` / `is_soil_like` / `stone_yield_count` / `soil_fertility_mult`. **Farmland is removed** — wheat now spawns on high-fertility `Grass` and the `seed_farmstead_yard` path bumps fertility on natural soil rather than writing a synthetic tile. Pathing speeds: `Sand 0.75`, `Snow 0.6`, `Marsh 0.4`, `Scrub 0.9`, soils 0.85–0.9, stone variants 1.0.
- Fixed update: **20 Hz** (`main.rs`).
- After mutating a tile, emit `TileChangedEvent { tile }`; `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- `CameraViewZ` defaults to `i32::MAX` (surface); lower it to peer underground.
- `TileMaterials`/`FogTileMaterials` keyed by `(TileKind, OreKind, z_bucket)`. Ore tiles fan out via `RENDERABLE_ORES`; colors in `color_map.rs::ore_tile_color`.
- **`sprite_library.rs`:** procedural pixel art from a 32-color palette via `ascii_to_image`. Reuse the palette/helpers — don't introduce new color systems.
- **PNG textures** in `assets/textures/` toggled by `entity_sprites::toggle_art_mode`.
- **`AnimalTextures`:** 8-direction PNGs for Wolf/Deer/Horse loaded at Startup from `assets/textures/<species>/rotations/{south,...}.png` (48×48). `ArtMode::Pixel` uses these; `ArtMode::Ascii` falls back to procedural sprites. `FacingDirection` is 8-way; `cardinal_str()` collapses to 4-way for the procedural library used by other animals. `animate_{wolves,deer,horses}_system` swaps the directional PNG and applies bob/sway on `VisualChild`.
- **GroundItem sprites:** `entity_sprites::spawn_ground_item_sprites` reactively attaches a child sprite, looking up `ResourceDef.sprite_key` in `SpriteLibrary`. Add a sprite by inserting `RESOURCE_X` in `sprite_library.rs`, registering it under a key, and pointing the catalog entry's `sprite_key` at it. All seed-class resources share `"resource_seed"`.
- **`SpatialIndex` (`world/spatial.rs`):** maintained incrementally. Every indexed entity carries `Indexed { kind, tile, z }`. `sync_indexed_after_move_system` (Sequential, after movement systems) handles add+move via `Or<(Changed<Transform>, Added<Indexed>)>`. Despawn uses an `on_remove` hook on `Indexed` (registered in `WorldPlugin::build`). `IndexedKind` covers Person/Wolf/Deer/Horse (mobile, also in `agent_counts`) plus Plant/GroundItem/Bed (static, 2D only). When converting an animal to a `Corpse` in `combat.rs::death_system`, also `remove::<Indexed>()`. New spawn sites for indexed kinds **must** include `Indexed::new(...)`. Sites that mutate `PersonAI.current_z` without mutating `Transform` must call `transform.set_changed()`.

## Constraints

- **ECS:** logic in Systems; Components hold data only. No OO inheritance.
- **UI:** `bevy_egui` for panels (avoid `bevy_ui` except specific overlays).
- **Hashing/randomness:** `ahash::AHashMap` (not `std::HashMap`). `fastrand` in hot paths, `rand` for init.
- **No new crates** without explicit permission.
- **Error handling:** avoid `unwrap()` in core systems; use `match`/`if let`.
- **Mutable aliasing:** be careful with Bevy query aliasing; test empirically.
- **Doc updates:** when behaviour changes, update the matching `AGENTS.md`. Subsystem-local changes in `src/<dir>/AGENTS.md`; cross-cutting in this file.
