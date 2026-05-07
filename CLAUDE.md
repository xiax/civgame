# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo run                  # Run the game (normal mode)
cargo run -- --sandbox     # Run sandbox mode (small 5×5 chunk map, one of every entity type)
cargo build --release      # Optimized build (thin LTO, single codegen unit)
cargo check                # Fast type check without compiling
cargo test --bin civgame   # Run tests (this is a binary crate — `cargo test` alone errors)
```

## Architecture

CivGame is a Dwarf Fortress-style civilization simulation built on **Bevy 0.15** (ECS). Six plugins compose the game:

| Plugin | Directory | Responsibility |
|--------|-----------|----------------|
| `WorldPlugin` | `src/world/` | Procedural terrain (Perlin noise, Z-levels -16..+15), chunk streaming (32×32 tiles), biomes, calendar/seasons, `SpatialIndex`, loads `ResourceCatalog` |
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plant growth, raids |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Flow-field caching, chunk-level navigation graph |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera (WASD/scroll), chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, economy panel, world map, right-click context menu, activity log |

### Subsystem deep-dives (path-scoped)

The detailed subsystem rules live next to the code they describe. Claude Code auto-loads these when you read or edit files in those trees:

- `src/simulation/CLAUDE.md` — Agent AI (Goals → HTN → Tasks), method-design rules, faction systems, knowledge & technology, hunting pipeline, typed-task variants, behavioural test fixture, game lifecycle / regions.
- `src/economy/CLAUDE.md` — Resource catalog, `Good`/`ResourceId` coexistence, recipes, carrying & item routing, equipment.
- `src/world/CLAUDE.md` — World generation, climate, geology & mining.
- `src/ui/CLAUDE.md` — Right-click menu, inspector, tech panel, activity log, world map, muster.

### Simulation scheduling (`SimulationSet`)

Systems must be assigned to the correct set — misplacement causes subtle bugs:

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy systems (needs ticks, mood, LOD, goal updates, animal sensing)
- **ParallelB** — HTN dispatchers (`htn_dispatch_system` for Sleep, then `htn_eat`, `htn_acquire_food`, `htn_acquire_good`, `htn_stockpile_food`); `goal_dispatch_system` runs alongside them as the no-plan stale-reset / Explore-cleanup catch-all for plan-driven goals
- **Sequential** — mutating systems with tight ordering: `gather` → `dig` / `construction` → `movement` → `combat` → `production`
- **Economy** — post-simulation: gossip, faction storage rollup, reproduction, raids, technology, market price updates

### Spatial / tile / rendering conventions

- World tiles: `(i32, i32)`; convert with `tile_to_world()`. (Widened from `i16` for the 32K×16K globe.)
- Chunk coords: `ChunkCoord::from_world()` using `div_euclid()`.
- Z-levels: `i8`, `Z_MIN=-16` to `Z_MAX=15`.
- Fixed update: **20 Hz** (`Time::<Fixed>::from_hz(20.0)` in `main.rs`).
- After mutating tiles, emit `TileChangedEvent { tile: (x, y) }` so `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- **`CameraViewZ`** defaults to `i32::MAX` (surface); lower it to peer underground (`update_tile_z_view_system` re-skins all tile sprites).
- **`TileMaterials`/`FogTileMaterials`** keyed by `(TileKind, OreKind, z_bucket)`. Ore tiles fan out via `RENDERABLE_ORES` (`chunk_streaming.rs`); colors in `color_map.rs::ore_tile_color`. `resolve_render_tile` returns `(TileKind, OreKind, z, Visibility)`.
- **`sprite_library.rs`:** Procedural pixel art from a 32-color warm earth-tone palette via `ascii_to_image` + char-substitution. Loaded at `Startup`. Reuse the palette / helpers — don't introduce new color systems.
- **PNG textures** in `assets/textures/` toggled by `entity_sprites::toggle_art_mode`.
- **GroundItem sprites:** `entity_sprites::spawn_ground_item_sprites` attaches a child sprite reactively. Currently only `Good::Stone` → `"resource_loose_rock"` (FogPersistent); add a match arm for more goods.
- **`SpatialIndex` (`world/spatial.rs`):** Maintained incrementally, not rebuilt. Every entity that should appear in `map`/`agent_counts` carries an `Indexed { kind, tile, z }` component. `sync_indexed_after_move_system` (Sequential, after `movement_system`/`animal_movement_system`/`sync_rider_horse_position_system`) handles add+move via `Or<(Changed<Transform>, Added<Indexed>)>`. Despawn is handled by the `on_remove` hook on `Indexed` (registered in `WorldPlugin::build`) — fires for `despawn_recursive` (chunk unload), explicit `despawn`, and component removal. `IndexedKind` covers Person/Wolf/Deer/Horse (mobile, also tracked in `agent_counts`) plus Plant/GroundItem/Bed (static, 2D only). When stripping species components to convert a hunted animal into a `Corpse` (`combat.rs::death_system`), also `remove::<Indexed>()` so the corpse leaves the index. New spawn sites for indexed entity kinds **must** include `Indexed::new(IndexedKind::…)` in the bundle. Sites that mutate `PersonAI.current_z` without mutating `Transform` must also call `transform.set_changed()` so the sync system observes the z-shift.

## Constraints

- **ECS discipline:** Logic in Systems; Components hold data only. No OO inheritance.
- **UI:** Use `bevy_egui` for panels. Avoid `bevy_ui` except for specific overlays.
- **Hashing/randomness:** `ahash::AHashMap` (not `std::HashMap`). `fastrand` in hot paths, `rand` for init.
- **No new crates** without explicit user permission.
- **Error handling:** Avoid `unwrap()` in core systems; use `match`/`if let`.
- **Mutable aliasing:** Be mindful of Bevy query mutable aliasing — test empirically when systems share component queries.
- **Updating these docs:** When you change behaviour the docs describe, update the matching file. Subsystem-local changes go in the relevant `src/<dir>/CLAUDE.md`; cross-cutting changes (scheduling, conventions, plugin layout) go here.
