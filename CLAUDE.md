# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo run                  # Run the game (normal mode)
cargo run -- --sandbox     # Run sandbox mode (small 5×5 chunk map, one of every entity type)
cargo build --release      # Optimized build (thin LTO, single codegen unit)
cargo check                # Fast type check without compiling
cargo test                 # Run tests
```

## Architecture

CivGame is a Dwarf Fortress-style civilization simulation built on **Bevy 0.15** (ECS). Six plugins compose the game:

| Plugin | Directory | Responsibility |
|--------|-----------|----------------|
| `WorldPlugin` | `src/world/` | Procedural terrain (Perlin noise, Z-levels -16..+15), chunk streaming (32×32 tiles, 64×64 world), biomes, calendar/seasons, `SpatialIndex` |
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plant growth, raids |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Flow-field caching, chunk-level navigation graph |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera (WASD/scroll), chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, economy panel, world map, right-click context menu |

### Simulation scheduling (`SimulationSet`)

Systems must be assigned to the correct set — misplacement causes subtle bugs:

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy systems (needs ticks, mood, LOD, goal updates, animal sensing)
- **ParallelB** — `goal_dispatch_system` (selects the next task for each agent)
- **Sequential** — mutating systems with tight ordering: `gather` → `dig` / `construction` → `movement` → `combat` → `production` → `plan_execution`
- **Economy** — post-simulation pass: gossip, faction storage rollup, reproduction, raids, technology, market price updates

### Key simulation systems

- **Person agents:** Goal-driven via `PersonAI` component.
  - **Goals (`goals.rs`):** High-level objectives (e.g., `Survive`, `Gather`, `Defend`) driven by Needs and Faction state.
  - **Plans (`plan.rs`):** Multi-step sequences (e.g., "Find Food" → "Harvest") scored by a per-agent 3-layer Q-network (`UtilityNet`). Built from a `StepRegistry` + `PlanRegistry` populated at startup.
  - **Tasks (`tasks.rs`):** The **current active task** an agent is performing — `TaskKind` enum: `Idle, Gather, Trader, Raid, Defend, Planter, Hunter, Scavenge, Construct, ConstructBed, DepositResource, Socialize, Reproduce, Explore, Dig, Sleep`. Tasks are transient and managed by either the plan system or `goal_dispatch_system`. An agent is `Idle` when between tasks. `task_interacts_from_adjacent()` flags tasks where the agent works from an adjacent tile rather than stepping onto the target.
  - **Professions (`person.rs::Profession`):** Persistent role assigned by `faction_profession_system` (currently `None | Farmer`). Distinct from tasks — a Farmer profession biases task selection toward planting/harvesting.
  - **Skills (`skills.rs`):** Per-agent `[u8; 8]` array — `Farming, Mining, Building, Trading, Combat, Crafting, Social, Medicine`. Default 5; `gain_xp()` is saturating.
  - **Bucketing:** Agents are bucketed across 20 fixed time slots (`BucketSlot`) to spread CPU load across frames.
- **Needs:** 6 tracked needs (hunger, sleep, shelter, safety, social, reproduction) clamped [0, 255] that decay over time and feed goal selection.
- **Factions:** Groups share a technology bitset (`u64`, up to 64 techs, Prehistoric → Bronze Age), camp entity (`FactionCenter`), bonding scores, storage rollup (`compute_faction_storage_system`), and raid logic. `SOLO` faction ID = 0 means ungrouped. `StorageTileMap` indexes storage tiles per faction.
- **Construction (`construction.rs`):** `BlueprintMap`, `WallMap`, `BedMap` resources track build state. `faction_blueprint_system` decides what to build; `construction_system` consumes resources and finalizes tiles/entities.
- **Terrain deformation (`dig.rs`):** `dig_system` mines surface or wall tiles, yields stone, awards Mining XP, and emits `TileChangedEvent`. Combined with `CameraViewZ`, the player can view and dig underground.
- **LOD:** Entities have `Detail / Aggregate / Dormant` levels by camera distance. Dormant entities skip simulation entirely (every per-agent system checks this first).
- **Memory & gossip:** Agents store known locations and agent sightings; share them through `PlanRegistry` gossip with `u8` freshness decay (`memory.rs`, `plan::plan_gossip_system`).

### World and rendering

- **`TileChangedEvent` pipeline:** Mutations to `ChunkMap` emit `TileChangedEvent`; `refresh_changed_tiles_system` (PostUpdate) rebuilds the affected tile sprites. Use this whenever code edits a tile in place.
- **`CameraViewZ`:** Player-controlled view Z-level — defaults to `i32::MAX` (surface). Lower it to peer underground; `update_tile_z_view_system` re-skins all tile sprites accordingly.
- **`sprite_library.rs`:** Procedural pixel-art sprites built from a 32-color warm earth-tone palette via `ascii_to_image` + char-substitution templates. Loaded at `Startup` by `setup_sprite_library`. New sprites should reuse the palette and substitution helpers rather than introducing new color systems.
- **PNG textures** in `assets/textures/` (e.g., `gatherer_s_a.png`, `wolf_anim_s_a.png`) are toggled via `entity_sprites::toggle_art_mode` as an alternative to the procedural sprites.

### Spatial and tile conventions

- World tiles: `(i16, i16)` pairs; convert with `tile_to_world()` / inverse.
- Chunk coords: `ChunkCoord::from_world()` using `div_euclid()`.
- Z-levels: `i8`, range `Z_MIN` (-16) to `Z_MAX` (+15).
- Fixed update loop: **20 Hz** (`Time::<Fixed>::from_hz(20.0)` in `main.rs`).
- After mutating tiles, write `TileChangedEvent { tile: (x, y) }` so the renderer refreshes.

## Constraints

- **ECS discipline:** All logic lives in Systems; Components hold data only. No object-oriented inheritance.
- **UI:** Use `bevy_egui` for all panels. Avoid `bevy_ui` except for specific rendering overlays.
- **Hashing/randomness:** Use `ahash::AHashMap` (not `std::HashMap`). Use `fastrand` in hot paths, `rand` for initialization.
- **No new crates** without explicit user permission.
- **Error handling:** Avoid `unwrap()` in core systems; use `match` / `if let`.
- **Mutable aliasing:** Be mindful of Bevy query mutable aliasing — test empirically when touching systems with overlapping component queries.
