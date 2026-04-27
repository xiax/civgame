# Project Overview: CivGame

CivGame is a Dwarf Fortress-style civilization simulation game built using the Bevy game engine.

## Core Pillars

- **Dwarf Fortress-style Simulation**: Deep simulation of individual agents, their needs, moods, jobs, and social interactions.
- **Complex Economy**: Agents participate in markets, trade goods, and fulfill economic roles.
- **Dynamic World**: Procedural terrain generation, chunk-based streaming, and seasonal cycles.
- **ECS Architecture**: Built strictly on Bevy's Entity-Component-System paradigm for performance and modularity.

## Technology Stack

- **Engine**: Bevy 0.15
- **UI**: `bevy_egui` 0.31
- **Language**: Rust
- **Utilities**: `ahash` (fast hashing), `fastrand`/`rand` (randomness), `noise` (terrain generation).

## Directory Structure

- `src/world`: Terrain, chunks, globe, seasons.
- `src/simulation`: Agent logic, needs, combat, factions.
- `src/economy`: Markets, goods, transactions.
- `src/pathfinding`: Navigation, flow fields.
- `src/rendering`: Camera, sprites, tiles.
- `src/ui`: HUD, inspectors, maps.

## Related Pages
- [Plugins](plugins.md)
- [Simulation](simulation.md)
- [Economy](economy.md)
