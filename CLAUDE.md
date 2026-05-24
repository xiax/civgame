# CLAUDE.md

Guidance for Claude Code working in this repository.

## Commands

```bash
cargo run                  # Run the game
cargo run -- --sandbox     # Sandbox (5×5 chunks, one of every entity)
cargo build --release      # Optimized build
cargo check                # Fast type check
cargo test --bin civgame   # Run tests (binary crate — `cargo test` alone errors)
```

## Architecture

CivGame is a Dwarf Fortress-style civilization simulation on **Bevy 0.15** (ECS). Six plugins:

| Plugin | Directory | Responsibility |
|--------|-----------|----------------|
| `WorldPlugin` | `src/world/` | Procedural terrain (Z `-16..+15`), chunk streaming (32×32 tiles), biomes, calendar, `SpatialIndex`, `ResourceCatalog` |
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plants, raids |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Component-typed chunk graph, hotspot flow fields |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera, chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, world map, right-click menu, activity log |

Per-directory `CLAUDE.md` files cover subsystem detail and are auto-loaded when reading/editing in those trees.

## Game-start options (`GameStartOptions`, `game_state.rs`)

Read once by `spawn_population` + `seed_starting_buildings_system`:
- `era: Era` — every member starts Aware of techs through this era. Learned set is role-scoped via `PersonKnowledge::seeded_realistic_through_era` (chief gets Personal+Household+Subsistence+Specialist+Institutional, ~1/8 are Specialist, rest Common).
- `player_population: u32` — group size for player faction (others use `GROUP_SIZE=20`).
- `economy: EconomyPreset` — `Subsistence` / `Mixed` / `Market`. Applied via `policy::apply_preset`.
- `lifestyle: Lifestyle` — `Settled` (default) or `Nomadic`. Player-faction only; AI stays Settled.
- `seed_buildings: bool` — sandbox sets false.

## Simulation scheduling (`SimulationSet`)

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy (needs, mood, LOD, goal updates, ambient social pairing, animal sensing).
- **ParallelB** — HTN dispatchers; `goal_dispatch_system` is the stale-reset / Explore-cleanup catch-all.
- **Sequential** — mutating, ordered: gather → dig/construction → movement → combat → production.
- **Economy** — gossip, faction storage rollup, reproduction, raids, technology, market prices.

`Input` (exclusive) drains `PlayerCommandEvent` ahead of ParallelA.

## Spatial / tile / Z conventions

- World tiles: `(i32, i32)`. Chunks: `ChunkCoord::from_world()` (uses `div_euclid`).
- Z-levels: `i8`, `Z_MIN=-16`, `Z_MAX=15`.
- Fixed update **20 Hz** (`main.rs`). Game speed (`Paused / 1× / 2× / 5×`) lives on `Time<Virtual>` via `GameSpeed` (`simulation/speed.rs`); higher presets fire FixedUpdate more often per real second, scaling every per-tick / cadence-gated system. `SimClock.scale_factor()` carries bucket compensation only. `SimTimingDiagnostics` reddens when avg tick CPU > `SpeedPreset::budget_ms_per_tick()` (50/25/10 ms).
- After mutating a tile, emit `TileChangedEvent { tile }`; `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- `CameraViewZ` defaults to `i32::MAX` (surface); lower to peer underground.
- `ChunkMap::vertical_clearance_at(x, y)` counts open `Air`/`Ramp` Z-levels above surface — for tall multi-Z vehicles (`pathfinding::vehicle_path::footprint_astar`).
- **`SpatialIndex`** (`world/spatial.rs`) is incremental: every indexed entity carries `Indexed { kind, tile, z }`. `sync_indexed_after_move_system` (Sequential) handles add/move via `Or<(Changed<Transform>, Added<Indexed>)>`; an `on_remove` hook handles despawn. New spawn sites for indexed kinds **must** include `Indexed::new(...)`. Sites mutating `PersonAI.current_z` without touching `Transform` must call `transform.set_changed()`.

### Tile palette

`TileKind` has 26 variants:
- **Surfaces**: `Grass`, `Forest`, `Sand`, `Snow`, `Marsh`, `Scrub`, `Water`, `River`, `Road`.
- **Stone lithologies** (`is_stone_like`): `Stone` (legacy), `Granite`, `Limestone` (yields 3 vs. 2), `Sandstone`, `Basalt`, plus underground `Wall` and `Ore`.
- **Soils** (`is_soil_like`): `Dirt`, `Loam` (1.5×), `Silt` (1.4×, riparian), `Clay`, `SandySoil` (0.6×), `Cropland` (1.3×).
- **`Bridge`** — passable, road-speed, reports `is_freshwater()` (water flows under decking).
- **`Dam`** — constructed barrier across a watercourse. Passable + road-speed (crest carries a road) but **not** water-like / freshwater / drinkable. Durable truth is the `Dam` entity in `DamMap`; the tile kind is its cache projection, restamped on chunk reload by `restamp_runtime_water_on_chunk_load`. Crest registered in `RuntimeWater.dam_crests`. Tech-gated on **`DAM_BUILDING`** (Bronze Age; prereqs `BRIDGE_BUILDING` + `MONUMENTAL_BUILDING`). AI plans via `organic_settlement::dam_intent_emitter_system` (`CivicKind::Dam` at Bronze+30).
- **`Cropland`** — tilled farm soil. Only ever appears inside an Agricultural plot. Worked into existence by `farm::prepare_field_task_system` (Sequential, `FIELD_PREP_WORK_TICKS=80`). `carve_plots_system` populates `PlotIndex.ag_tiles` + per-tile `farm::FieldTileIndex` entries but leaves the underlying soil/grass; founders pay Spring 1 to till. At game start the carve runs inside the `OnEnter(Playing)` chain (survey → project plans → carve → seed-farms, see `plans/spawn-farm-seeding.md`). Personal kitchen gardens are real Agricultural parcels (`organic_settlement::append_dwelling_gardens`); no seed-time house yard. `is_soil_like`, speed 0.9, never paved by road carving.

Helpers: `stone_yield_count`, `soil_fertility_mult`. No `Farmland` variant — Grain grows on `Cropland`; world-gen `TileData.fertility` is the immutable per-tile recovery ceiling, while `FieldTileIndex.by_tile[tile].nutrients` is the live nutrient pool. Pathing speeds: Sand 0.75, Snow 0.6, Marsh 0.4, Scrub 0.9, soils 0.85–0.9 (Cropland 0.9), stone 1.0.

`river_distance_at(tx, ty)` returns chebyshev tiles to nearest river (`u8::MAX` = far/unloaded), populated at chunk-gen and read by riparian biome shift, fertility boost, settlement scoring, herd/nomad freshwater preference.

## Rendering conventions

- `TileMaterials` / `FogTileMaterials` keyed by `(TileKind, OreKind, z_bucket)`. Ore tiles fan out via `RENDERABLE_ORES`; colors in `color_map.rs::ore_tile_color`.
- **`sprite_library.rs`** — procedural pixel art from a 32-color palette via `ascii_to_image`. Reuse the palette/helpers; don't introduce a new color system.
- **PNG textures** in `assets/textures/` toggled by `entity_sprites::toggle_art_mode`.
- **`AnimalTextures`** — 8-direction PNGs for Wolf/Deer/Horse/Cow/Cat loaded at Startup from `textures/<species>/rotations/`; ascii fallback (`creature_*` sprite-lib keys) otherwise.
- **GroundItem sprites** — `entity_sprites::spawn_ground_item_sprites` reactively attaches a child sprite via `ResourceDef.sprite_key`. Add a sprite by inserting `RESOURCE_X` in `sprite_library.rs`, registering it under a key, and pointing the catalog entry's `sprite_key` at it.
- **Vehicle part sprites** (`rendering/vehicle_part_sprites.rs`) — hand-drawn ASCII art per `VehiclePartKind` + variant + multi-cell weapon-module composite, registered into `SpriteLibrary`. Three views — `VehicleSpriteView::{Side, Front, Back}`; `view_for_heading` maps 0=Back / 1=Side(flip) / 2=Front / 3=Side. Asymmetric parts (Hitch, Yoke, CrewSeat, WeaponMount, Engine, Turret) ship distinct `_BACK` art; symmetric parts (Frame, Deck, Wall, Axle, Wheel, CargoBay, Track, ArmorPlate) re-register `_FRONT` under `_back`. Resolvers `vehicle_part_sprite_key(kind, variant_label, view)` + `vehicle_module_sprite_key(module_label, view)`. `entity_sprites::vehicle_sprite_plan_with_data(design, heading, &VehicleData)` populates `VehicleSpriteCell.{sprite_key, fallback_sprite_key, flip_x}`; `spawn_vehicle_sprites` tries variant→base→colour-quad. Multi-cell weapon modules collapse to one composite at anchor `(z, y, x)` min, sized footprint × 16 px. Legacy colour-only `vehicle_sprite_plan` kept as wrapper for designer preview + headless tests.
- **Connector overlay pass** (`entity_sprites::push_connector_overlays`) — after the per-cell loop, walks `NEIGHBORS_6` to emit bridging sprites that close the transparent borders between adjacent cells. Sprite keys `vehicle_connector_<label>_<view>_<dir>` where `dir` ∈ `Up/Down/Left/Right` (camera-space; grid delta passes through `grid_delta_to_screen_dir` with heading rotation). Adjacency rules: `Axle↔Wheel → axle_wheel`, `Frame/Deck/Wall ↔ same kind → <kind>_seam`, `Hitch/Yoke ↔ Frame → <kind>_attach`. Every `CrewSeat` also emits a `crew_seat_facing` overlay pointing at chassis-forward (+Y grid → screen direction per heading). Overlays carry `flip_x = false` (direction already baked into the chosen key), z-order base + 0.001, and `fallback_sprite_key = None` — a missing connector silently doesn't draw. Module cells are skipped so the composite owns its silhouette.
- **Water-current streaks** (`rendering/water_current_render.rs`) — animated flow-streak sprites on flowing-`River` tiles; scrolls along `world::water_current::WaterCurrentField` (`animate_current_streaks_system`). `water_current_render_system` reconciles per-chunk `CurrentStreakIndex` every frame (spawn-area chunks pre-load without `ChunkLoadedEvent`). z=0.35, `ProjectedAnchor::Static`.
- **Day-night overlay** (`rendering/day_night.rs`) — full-screen sprite at z=90 tinted by `Calendar::day_fraction()`; layered above world sprites, per-entity fog tinting multiplies below.
- **Tilted-view projection** (`rendering/projection.rs`) — `MapViewMode::{TopDown, Tilted}` (toggle `V` / HUD). Symmetric pre/post: `revert_view_projection_system` (PreUpdate) strips so sim sees logical Transforms; `apply_view_projection_system` (PostUpdate) re-projects. TopDown is identity. `ProjectedAnchor::{Static{z}, Dynamic}` auto-attached per marker by `auto_attach_dynamic::<T>`. Helpers: `project` / `unproject_to_world` / `unproject_to_tile` / `camera_view_to_logical` / `logical_to_view_camera` / `tile_to_view_camera` (bundled in `ViewProjection` SystemParam). `CursorParams::pick_cliff_aware` walks `[Z_MIN, Z_MAX]` matching `surface_z_at` so cliff-tops resolve. Drag-select projects logical into view-space; bookmarks store **logical** coords. `ElevationSkirt` renders south-facing cliffs (north strips on `ChunkLoadedEvent`).

## Tools (`simulation/tools.rs`)

`ToolForm` (Knife/Axe/Pick/Hammer/Sickle/Awl/FishingKit) = *what*; `Item.material+quality` → `ToolTier` (Bone<Stone<FineStone<Copper<Bronze) = *how well* via `work_speed_mult`. Tools live in `ToolKit` (`Vec<Item>`, `capacity_for_era`), separate from `Carrier` hands; spawned on every Person. **Hard gates** (no `ToolKit` reads as satisfied; empty kit blocks): Axe→fell trees (no-Axe → deadwood, tree kept), Pick→mine Stone/Wall/Ore + dig stone-like floors (no-Pick → trickle), Sickle→reap mature Grain, Fishing Kit→fish, recipe `tool_requirements`→craft. `seed_starting_tools_system` (OnEnter) pre-stows era loadouts. Acquisition: `Task::WithdrawTool`→`StowToolKit`. Faction demand: `compute_faction_tool_deficits` feeds chief craft postings (generic `tools` commodity remains only as Ard-Plow ingredient).

## Constraints

- **ECS:** logic in Systems; Components hold data only. No OO inheritance.
- **UI:** `bevy_egui` for panels (avoid `bevy_ui` except specific overlays).
- **Hashing/randomness:** `ahash::AHashMap` (not `std::HashMap`). `fastrand` in hot paths, `rand` for init.
- **No new crates** without explicit permission.
- **Error handling:** avoid `unwrap()` in core systems; use `match` / `if let`.
- **Mutable aliasing:** be careful with Bevy query aliasing; test empirically.
- **Doc updates:** when behaviour changes, update the matching `CLAUDE.md`. Subsystem-local changes go in `src/<dir>/CLAUDE.md`; cross-cutting in this file. Keep entries terse.
