# CLAUDE.md

Guidance for Claude Code working in this repository. This file is the cross-cutting index; **subsystem depth lives in the per-directory `CLAUDE.md` files**, which auto-load when you read/edit in those trees.

## Commands

```bash
cargo run                  # Run the game
cargo run -- --sandbox     # Sandbox (5×5 chunks, one of every entity)
cargo build --release      # Optimized build
cargo check                # Fast type check
cargo test --bin civgame   # Run tests (binary crate — `cargo test` alone errors)
```

`[profile.dev.package."*"] opt-level = 3` (Cargo.toml) optimizes dependencies (Bevy etc.) even in debug, so `cargo run` is not bottlenecked on unoptimized deps; our own crate stays at `opt-level = 1` for fast incremental builds.

## Architecture

CivGame is a Dwarf Fortress-style civilization simulation on **Bevy 0.15** (ECS). Plugins, each with its own `CLAUDE.md`:

| Plugin | Directory | Responsibility |
|--------|-----------|----------------|
| `WorldPlugin` | `src/world/` | Procedural terrain (Z `-16..+15`), chunk streaming (32×32 tiles), biomes, hydrology, calendar, `SpatialIndex`, tile palette, edge structures, floristic regions |
| `SimulationPlugin` | `src/simulation/` | Agent AI (Goals→HTN→Tasks), needs, combat, reproduction, factions, technology + knowledge, beliefs, plants, tools, raids, diplomacy, husbandry, game lifecycle |
| `EconomyPlugin` | `src/economy/` | Resource catalog, goods, items, carrying, markets, prices, recipes, transactions, policy |
| `PathfindingPlugin` | `src/pathfinding/` | Component-typed chunk graph, hotspot flow fields |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel-art tiles, camera, chunk-streaming visuals, entity/plant/vehicle sprites, projection |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, world map, right-click menu, activity log |
| `NetPlugin` | `src/net/` | LAN multiplayer, `NetId`, server-authoritative boundary, custom snapshot protocol |

## Knowledge system

Catalog of 91 `TechId`/`KnowledgeId` entries (`simulation/{technology,knowledge,knowledge_catalog,building_technique}.rs`) spanning four axes — **Kind** (`PracticalSkill`/`PracticalTechnique`/`Belief`/`Lore`), **Domain** (Subsistence/Craft/Construction/Transport/Institutional/Medicine/Cosmology/Lore/Martial), **Truth** (`True`/`FalseUseful`/`FalseHarmful`/`Contested`), **Adoption scale** (`AdoptionScale`, drives founder-Learned seeding). Layout: 50 core techs + 14 building techniques (50–63) + 16 foundations (64–79) + 6 beliefs (80–85) + 5 biome-plant entries (86–90). `KnowledgeBits` is a 128-bit bitset for `aware`/`learned`. Building techniques pick a cultural method from Learned-pool × locality × purpose. Full taxonomy, foundations, beliefs (consumer hooks), and biome-plant gating → `src/simulation/CLAUDE.md` → Knowledge & technology. Construction materials → `src/economy/CLAUDE.md`.

## Simulation scheduling (`SimulationSet`)

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy (needs, mood, LOD, goal updates, ambient social pairing, animal sensing).
- **ParallelB** — HTN dispatchers; `goal_dispatch_system` is the stale-reset / Explore-cleanup catch-all.
- **Sequential** — mutating, ordered: gather → dig/construction → movement → combat → production.
- **Economy** — gossip, faction storage rollup, reproduction, raids, technology, market prices.

`Input` (exclusive) drains `PlayerCommandEvent` ahead of ParallelA. Server-authority gating of these sets → `src/net/CLAUDE.md`.

## Spatial / tile / Z conventions

- World tiles: `(i32, i32)`. Chunks: `ChunkCoord::from_world()` (uses `div_euclid`). Z-levels: `i8`, `Z_MIN=-16`, `Z_MAX=15`.
- Fixed update **20 Hz** (`main.rs`). Game speed (`Paused / 1× / 2× / 5×`) lives on `Time<Virtual>` via `GameSpeed` (`simulation/speed.rs`); higher presets fire FixedUpdate more often per real second. `SimTimingDiagnostics` reddens when avg tick CPU > `SpeedPreset::budget_ms_per_tick()` (50/25/10 ms).
- After mutating a tile, emit `TileChangedEvent { tile }`; `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- `CameraViewZ` defaults to `i32::MAX` (surface); lower to peer underground. `ChunkMap::vertical_clearance_at(x, y)` counts open `Air`/`Ramp` Z-levels for tall multi-Z vehicles. `river_distance_at(tx, ty)` = chebyshev tiles to nearest river (`u8::MAX` = far/unloaded).
- **`SpatialIndex`** (`world/spatial.rs`) is incremental: every indexed entity carries `Indexed { kind, tile, z }`. `sync_indexed_after_move_system` (Sequential) handles add/move via `Or<(Changed<Transform>, Added<Indexed>)>`; an `on_remove` hook handles despawn. New spawn sites for indexed kinds **must** include `Indexed::new(...)`. Sites mutating `PersonAI.current_z` without touching `Transform` must call `transform.set_changed()`.
- **`TileKind`** has 26 variants (surfaces / stone lithologies / soils / Bridge / Dam / Cropland), with per-kind pathing speeds + yield/fertility helpers → `src/world/CLAUDE.md` → Tile palette.
- **Incremental excavation** (`simulation::excavation`) — stone/ore Mine + Dig Down advance through 7 levels (1–6 slow traversal + grant ranged cover; 7 finalises). Bare hands cap at `HAND_DEPTH_LIMIT=3`. State in `ExcavationMap`, cached as 3 bits in `TileData.flags`. Detail → `src/simulation/CLAUDE.md`.
- **Thin housing edge walls** put walls/doors on tile *boundary edges* so the footprint stays passable floor → `src/world/CLAUDE.md` → Thin housing edge walls (rendering/replication in `rendering`/`net`).
- **Diplomacy & Territory** — sparse `TerritoryMap` + faction-pair `DiplomacyLedger` (treaties + reputation) → `src/simulation/CLAUDE.md` → Diplomacy & Territory.

## Constraints

- **ECS:** logic in Systems; Components hold data only. No OO inheritance.
- **UI:** `bevy_egui` for panels (avoid `bevy_ui` except specific overlays).
- **Hashing/randomness:** use `crate::collections::{AHashMap, AHashSet}` (deterministic fixed-seed `ahash`, NOT `ahash::AHashMap` or `std::HashMap` — both are process-keyed and make the sim non-reproducible, which flaked the behavioural suite; see `src/collections.rs`). Never use process-keyed `AHasher::default()` / `ahash::AHashMap::default()` / `std::collections::HashMap` for anything iterated to drive a decision; `ahash::RandomState::with_seeds(..)` stays the primitive for the few hand-seeded sites (`net`, `region`).
  - **All `src/simulation/` randomness routes through `simulation::sim_rng::SimRng`** (derivation-based, read as `Res<SimRng>` — **never `ResMut`**, so it's parallel-safe and order-independent). Each draw builds a local `fastrand::Rng` via `for_entity(entity, tick, RngSite)` / `for_tile(tile, tick, RngSite)` / `for_key(stable_key, tick, RngSite)`; the result is a pure function of `WorldSeed + key + tick + site_salt`, so it's reproducible regardless of execution order or thread. **Global `fastrand::{f32,u8,…,shuffle}` and `rand::thread_rng()` are banned in `src/simulation/`** (a `#[test]` in `sim_rng.rs` enforces it; rendering/UI cosmetic randomness is exempt). `sim_rng::mix` is the one splitmix64 primitive (generalises `region::home_pick_seed`). New randomness site → add an `RngSite` variant (never renumber existing ones — they feed saved-seed reproducibility) and key on a stable id + `clock.tick`. Big systems at the 16-param ceiling fold `SimRng` into an existing `SystemParam` bundle (`CombatEventWriters`, `StandRouting`, `MovementStandRouting`). Convenience `Foo::random()` (`BiologicalSex`/`BodyPart`/`Personality`/`SkinTone`/`HairColor`/`Stats::roll_3d6`) survive for dev/test only (seed a fresh `fastrand::Rng::new()`); production-sim callers use the `_from(&mut Rng)` variant fed by `SimRng`. Seeded at `OnEnter(Playing)` by `reseed_sim_rng_system` (before `spawn_population`); replicates for free (clients re-derive from the bootstrap `world_seed`). **Caveat:** seed-determinism holds under a *deterministic schedule* (the test fixture pins the single-threaded executor); full *parallel* bit-determinism additionally needs order-independent shared-state mutation (reservations, `par_iter` contention, async path results) — a separate concern. Detail → `src/simulation/CLAUDE.md` → Deterministic simulation RNG.
- **No new crates** without explicit permission.
- **Error handling:** avoid `unwrap()` in core systems; use `match` / `if let`.
- **Mutable aliasing:** be careful with Bevy query aliasing; test empirically.
- **`PersonAI.state` is encapsulated:** never assign it directly — use `ActionQueue` transition methods (`begin_working` / `begin_seeking` / `begin_routing` / `begin_sleeping` / `begin_attacking` / `finish_task` / `cancel_chain`). Direct `ai.state = AiState::X` writes outside `src/simulation/` are a compile error. See `src/simulation/CLAUDE.md` → ActionQueue.
- **Doc updates:** when behaviour changes, update the matching `CLAUDE.md`. Subsystem-local changes go in `src/<dir>/CLAUDE.md`; cross-cutting in this file. Keep entries terse.
