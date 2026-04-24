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
Input → ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA / ParallelB** — read-heavy systems that can run concurrently
- **Sequential** — mutates global state or requires tight ordering
- **Economy** — post-simulation price/transaction pass

### Key simulation systems

- **Person agents:** Goal-driven via `PersonAI` component. Plans are multi-step sequences scored by a per-agent 3-layer Q-network (`UtilityNet`, 32→16→8→1). Agents are bucketed across 20 fixed time slots to spread CPU load across frames.
- **Needs:** 6 tracked f32 needs (hunger, sleep, shelter, safety, social, reproduction) clamped [0, 255] that decay over time and drive job selection.
- **Factions:** Groups share a technology bitset (`u64`, up to 64 techs, Prehistoric → Bronze Age), camp entity (`FactionCenter`), bonding scores, and raid logic. `SOLO` faction ID = 0 means ungrouped.
- **LOD:** Entities have `Detail / Aggregate / Dormant` levels by camera distance. Dormant entities skip simulation entirely.
- **Memory & gossip:** Agents store known locations and agent sightings; share them through `PlanRegistry` gossip with `u8` freshness decay.

### Spatial and tile conventions

- World tiles: `(i16, i16)` pairs; convert with `tile_to_world()` / inverse.
- Chunk coords: `ChunkCoord::from_world()` using `div_euclid()`.
- Z-levels: `i8`, range `Z_MIN` (-16) to `Z_MAX` (+15).
- Fixed update loop: **20 Hz** (`Time::<Fixed>::from_hz(20.0)` in `main.rs`).

## Constraints

- **ECS discipline:** All logic lives in Systems; Components hold data only. No object-oriented inheritance.
- **UI:** Use `bevy_egui` for all panels. Avoid `bevy_ui` except for specific rendering overlays.
- **Hashing/randomness:** Use `ahash::AHashMap` (not `std::HashMap`). Use `fastrand` in hot paths, `rand` for initialization.
- **No new crates** without explicit user permission.
- **Error handling:** Avoid `unwrap()` in core systems; use `match` / `if let`.
- **Mutable aliasing:** Be mindful of Bevy query mutable aliasing — test empirically when touching systems with overlapping component queries.
