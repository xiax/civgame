# CLAUDE.md

Guidance for Claude Code working in this repository.

## Commands

```bash
cargo run                  # Run the game
cargo run -- --sandbox     # Sandbox mode (5×5 chunk map, one of every entity)
cargo build --release      # Optimized build
cargo check                # Fast type check
cargo test --bin civgame   # Run tests (binary crate — `cargo test` alone errors)
```

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

Per-directory `CLAUDE.md` files cover subsystem detail. Claude Code auto-loads them when reading/editing in those trees.

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
- Fixed update: **20 Hz** (`main.rs`).
- After mutating a tile, emit `TileChangedEvent { tile }`; `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- `CameraViewZ` defaults to `i32::MAX` (surface); lower it to peer underground.
- `TileMaterials`/`FogTileMaterials` keyed by `(TileKind, OreKind, z_bucket)`. Ore tiles fan out via `RENDERABLE_ORES`; colors in `color_map.rs::ore_tile_color`.
- **`sprite_library.rs`:** procedural pixel art from a 32-color palette via `ascii_to_image`. Reuse the palette/helpers — don't introduce new color systems.
- **PNG textures** in `assets/textures/` toggled by `entity_sprites::toggle_art_mode`.
- **`AnimalTextures`:** 8-direction PNGs for Wolf/Deer/Horse loaded at Startup from `assets/textures/<species>/rotations/{south,...}.png` (48×48). `ArtMode::Pixel` uses these; `ArtMode::Ascii` falls back to procedural sprites. `FacingDirection` is 8-way; `cardinal_str()` collapses to 4-way for the procedural library used by other animals. `animate_{wolves,deer,horses}_system` swaps the directional PNG and applies bob/sway on `VisualChild`.
- **GroundItem sprites:** `entity_sprites::spawn_ground_item_sprites` reactively attaches a child sprite. Currently only `Good::Stone`; add a match arm for more.
- **`SpatialIndex` (`world/spatial.rs`):** maintained incrementally. Every indexed entity carries `Indexed { kind, tile, z }`. `sync_indexed_after_move_system` (Sequential, after movement systems) handles add+move via `Or<(Changed<Transform>, Added<Indexed>)>`. Despawn uses an `on_remove` hook on `Indexed` (registered in `WorldPlugin::build`). `IndexedKind` covers Person/Wolf/Deer/Horse (mobile, also in `agent_counts`) plus Plant/GroundItem/Bed (static, 2D only). When converting an animal to a `Corpse` in `combat.rs::death_system`, also `remove::<Indexed>()`. New spawn sites for indexed kinds **must** include `Indexed::new(...)`. Sites that mutate `PersonAI.current_z` without mutating `Transform` must call `transform.set_changed()`.

## Constraints

- **ECS:** logic in Systems; Components hold data only. No OO inheritance.
- **UI:** `bevy_egui` for panels (avoid `bevy_ui` except specific overlays).
- **Hashing/randomness:** `ahash::AHashMap` (not `std::HashMap`). `fastrand` in hot paths, `rand` for init.
- **No new crates** without explicit permission.
- **Error handling:** avoid `unwrap()` in core systems; use `match`/`if let`.
- **Mutable aliasing:** be careful with Bevy query aliasing; test empirically.
- **Doc updates:** when behaviour changes, update the matching `CLAUDE.md`. Subsystem-local changes in `src/<dir>/CLAUDE.md`; cross-cutting in this file.
