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
- **ParallelB** — `goal_dispatch_system` (Sleep-only fallback — every other goal is plan-driven; survives only because the bed/camp fallback chain wasn't worth porting)
- **Sequential** — mutating systems with tight ordering: `gather` → `dig` / `construction` → `movement` → `combat` → `production` → `plan_execution`
- **Economy** — post-simulation pass: gossip, faction storage rollup, reproduction, raids, technology, market price updates

### Key simulation systems

- **Person agents:** Goal-driven via `PersonAI` component.
  - **Goals (`goals.rs`):** High-level objectives (e.g., `Survive`, `Gather`, `Defend`) driven by Needs and Faction state.
  - **Plans (`plan/`):** Multi-step sequences (e.g., "Find Food" → "Harvest") scored by a static linear function — `dot(state, plan.state_weights) + plan.bias` plus manual bonuses (persistence, ally influence, dist-weighted memory penalty). `PlanScoringMethod::Weighted` for normal agents; `Random` for ε-greedy exploration. Built from a `StepRegistry` + `PlanRegistry` populated at startup. Module split: `plan/mod.rs` holds types, `ActivePlan`/`PlanHistory`/`KnownPlans`, `resolve_target`, and `plan_execution_system`; `plan/registry.rs` holds the step + plan data tables; `plan/state.rs` holds `build_state_vec` + `count_visible_*`. Each `PlanDef` carries `flags: PlanFlags` (`PF_EXPLORE`, `PF_SCAVENGE`, `PF_TARGETS_FOOD/WOOD/STONE`, `PF_DROP_FOOD_ON_TIMEOUT`) — the candidate filter reads flags rather than per-plan-id matches.
  - **Explore is per-resource:** `ExploreForFood` (id 35) / `ExploreForWood` (36) / `ExploreForStone` (37) each carry `memory_target_kind: Some(...)`. The candidate filter inverts for these IDs: an Explore plan is only available when the agent has neither memory nor visibility of its target. `explore_satisfaction_system` (runs after `vision_system`, before `plan_execution_system`) removes the `ActivePlan` and logs `PlanOutcome::Success` the moment memory of the target kind is recorded — so workers stop wandering as soon as they spot what they were looking for, instead of running the 5000-tick timeout.
  - **Tasks (`tasks.rs`):** The **current active task** an agent is performing — `TaskKind` enum: `Idle, Gather, Trader, Raid, Defend, Planter, Hunter, Scavenge, Construct, ConstructBed, DepositResource, Socialize, Reproduce, Explore, Dig, Sleep`. Tasks are transient and managed by `plan_execution_system` (the dispatch path for every goal except `Sleep`); `goal_dispatch_system` survives only as the Sleep fallback. An agent is `Idle` when between tasks. `task_interacts_from_adjacent()` flags tasks where the agent works from an adjacent tile rather than stepping onto the target.
  - **Professions (`person.rs::Profession`):** Persistent role assigned by `faction_profession_system` (currently `None | Farmer`). Distinct from tasks — a Farmer profession biases task selection toward planting/harvesting.
  - **Skills (`skills.rs`):** Per-agent `[u8; 8]` array — `Farming, Mining, Building, Trading, Combat, Crafting, Social, Medicine`. Default 5; `gain_xp()` is saturating.
  - **Bucketing:** Agents are bucketed across 20 fixed time slots (`BucketSlot`) to spread CPU load across frames.
- **Needs:** 6 tracked needs (hunger, sleep, shelter, safety, social, reproduction) clamped [0, 255] that decay over time and feed goal selection.
- **Factions:** Groups share a technology bitset (`u64`, up to 64 techs, Prehistoric → Bronze Age), camp entity (`FactionCenter`), bonding scores, storage rollup (`compute_faction_storage_system`), and raid logic. `SOLO` faction ID = 0 means ungrouped. `StorageTileMap` indexes storage tiles per faction.
- **Construction (`construction.rs`):** `BlueprintMap`, `WallMap`, `BedMap` resources track build state. `faction_blueprint_system` decides what to build; `construction_system` consumes resources and finalizes tiles/entities.
- **Hearth pacing by era (`generate_candidates` in `construction.rs`):** Era is read via `technology::current_era(techs)` (highest era of any unlocked tech). Paleolithic/Mesolithic — `paleolithic_hearth_count` upper bound `(members+5)/6`, gated on crescent saturation across all hearths AND `bed_deficit_pre > 0`; bands fill out hearth #1's crescent before #2 opens. Neolithic — `(members+7)/8` upper bound, gated on every existing hearth having ≥ 8 beds in its 2..6 crescent ring (one extended-family household per hearth); helper `count_beds_in_crescent` does the count. Chalcolithic+ — single civic-zone hearth (hearth-per-house remains future work — requires a Household component).
- **Defensive walls are Chalcolithic+:** `generate_candidates` skips the `BuildIntent::PalisadeSegment` defense branch unless `current_era(techs) >= Chalcolithic`. Real Neolithic farming villages were typically unwalled; defensive perimeters belong to city-state society. The `best_wall_material` ladder now requires `COPPER_WORKING` for `WallMaterial::Stone` (was incorrectly gated on Paleolithic `FLINT_KNAPPING`); pre-Chalcolithic huts use `Mudbrick` (with `FIRED_POTTERY`), `WattleDaub` (with `PERM_SETTLEMENT`), or `Palisade`. The era gate only suppresses defensive perimeters — `Hut` / `Longhouse` walls still place freely using `wall_mat`.
- **Carrying (`carry.rs`):** `Carrier` holds two hand slots, separate from `EconomicAgent.inventory`. Goods have a `Bulk` class — `TwoHand` (Wood, Stone, Iron) requires both hands empty to pick up; `OneHand` / `Small` need one free hand. `enforce_hand_state_system` runs before gather/dig and consults `gather_target_yield_bulk` to upgrade the hand requirement to "both empty" when the harvest yield is `TwoHand` (otherwise `route_yield` would silently spill the wood/stone to ground). `is_at_haul_cap` returns `true` for any TwoHand stack with `qty > 0` since no more can be added regardless of qty.
- **Eating from hands (`production.rs::eat_task_system`):** Foraged Fruit/Meat/Grain are `Bulk::Small` and routed through `Carrier::try_pick_up` first, so freshly-gathered food usually lives in hands, not `EconomicAgent.inventory`. The eat executor and the `requires_any_edible` precondition both call `production::total_edible(agent, carrier)` and decrement from whichever store the picked edible came from (`agent.inventory[idx].1 -= 1` or `Carrier::remove_good`). Without this an agent holding fruit in hand would abort `TaskKind::Eat` immediately and starve.
- **Source vs good visibility (`STATE_DIM`=41):** Each visibility slot answers exactly one yes/no question, and source visibility never shares a slot with good visibility. Sources (slots 35-37): `SI_VIS_PLANT_FOOD` (mature edible plants), `SI_VIS_TREE` (mature trees), `SI_VIS_STONE_TILE` (Stone tiles) — feed `Forage`/`Gather` plans, which can only act on a source. Goods (slots 38-40): `SI_VIS_GROUND_WOOD`, `SI_VIS_GROUND_STONE`, `SI_VIS_GROUND_FOOD` — feed `Scavenge*` plans, which can only pick up loose `GroundItem`s. Counters: `count_visible_plant_food`/`count_visible_trees`/`count_visible_stone_tiles` (sources) and `count_visible_ground_*` (goods). The candidate filter in `plan_execution_system` mirrors the split: gather/farm plans gate on source-vis OR memory; plans with `PF_SCAVENGE` gate on the matching ground slot; plans with `PF_EXPLORE` invert the gate (require both source-vis AND ground-vis to be zero). The resource a Scavenge/Explore plan targets is named via the matching `PF_TARGETS_FOOD/WOOD/STONE` flag. `Deliver*ToCraftOrder` plans for wood/stone are storage-driven (not vis-driven) — see WithdrawMaterial below.
- **WithdrawMaterial intent:** Storage tiles hold many goods at once, so the resolver — not the executor — decides what to take. All `TaskKind::WithdrawMaterial` steps share one `StepTarget::WithdrawForFactionNeed { need, selector }` variant that funnels through `resolve_withdraw_for_faction_need`: `need` is `Blueprint` / `CraftOrder` / `HaulClaim` (where demand comes from), `selector` is `MostDeficient` / `Specific(Good)` (which good to commit to). Steps 32 (`FetchMaterialFromStorage`, Blueprint+MostDeficient), 40 (`FetchCraftOrderMaterialFromStorage`, CraftOrder+MostDeficient), and 41 (`WithdrawClaimedHaulMaterial`, HaulClaim) parameterise the same resolver — adding a new "fetch X for Y" is just a new `StepDef` with different params. Steps 46/47 and the per-good plans 11/12 (`DeliverWoodToCraftOrder`, `DeliverStoneToCraftOrder`) are gone; plan 15 (`DeliverFromStorageToCraftOrder`) handles every material because step 40's `MostDeficient` selector picks whichever good open orders need most.
- **Storage reservations (`StorageReservations` resource):** Tile-scoped `(tile, good) → reserved_qty` map. Each successful `WithdrawMaterial` dispatch increments the entry by the committed qty and stashes `(reserved_tile, reserved_good, reserved_qty)` on `PersonAI`; `release_reservation` decrements and clears the fields on every teardown path (executor exit, plan abort/preempt, drafted/playerorder swap). The resolver subtracts reservations from raw `GroundItem.qty` when computing effective stock, so two agents in the same tick can no longer commit to the same one-unit stack — the second resolver sees zero effective stock and either picks another tile/good or returns `None`. Wrapped in a `Mutex` because `plan_execution_system` runs per-agent in parallel.
- **WithdrawMaterial executor reliability:** `withdraw_material_task_system` (production.rs) on entry drops any held stack whose good doesn't match `withdraw_good` (using `spawn_or_merge_ground_item` at the agent's current tile, not the storage tile) so the worker arrives with hands aligned for the deposit step. After the spatial scan it computes `taken = promised - remaining`; if `taken == 0` (the reserved stack was emptied between dispatch and arrival) the executor pushes `PlanOutcome::FailedNoTarget` to `PlanHistory` and removes `ActivePlan`, preventing the silent "haul-with-empty-hands" trip the next `HaulTo*` step would otherwise produce. Every exit path calls `release_reservation`.
- **Scavenge plans:** `ScavengeFood` (plan 6), `ScavengeWood` (plan 38), `ScavengeStone` (plan 39) all share the shape `[Collect*, DepositGoods]` — walk to the nearest matching `GroundItem`, pick it up, deposit at faction storage. They clean up loose materials left by `harvest_ground_drops`, prior spills, and combat drops. `ScavengeWood`/`Stone` score on `SI_VIS_GROUND_WOOD`/`SI_VIS_GROUND_STONE` (weight 1.5) and outscore their `Gather*` siblings only when ground litter is actually present.
- **Terrain deformation (`dig.rs`):** `dig_system` mines surface or wall tiles, yields stone, awards Mining XP, and emits `TileChangedEvent`. Combined with `CameraViewZ`, the player can view and dig underground.
- **LOD:** Entities have `Detail / Aggregate / Dormant` levels by camera distance. Dormant entities skip simulation entirely (every per-agent system checks this first).
- **Memory & gossip:** Agents store known locations and agent sightings; share them through `PlanRegistry` gossip with `u8` freshness decay (`memory.rs`, `plan::plan_gossip_system`).

### World and rendering

- **Terrain noise (`terrain.rs::surface_v`):** 4-octave FBM — continental macro layer at freq 0.005 (weight 0.35), base octave at 0.02 (0.40), then 2× and 4× harmonics (0.18, 0.07). The result is reshaped via a signed power curve `sign(c) * |c|^0.65` (centered around 0.5) so peaks/basins push toward Z extremes instead of clustering at mid-elevation. Lowering the base frequency or the power exponent makes features bigger / more dramatic respectively.
- **Globe noise (`globe.rs::generate_globe`):** Elevation uses 4 octaves: continental at 0.012 (weight 0.30), then 0.03/0.06/0.12 (0.42, 0.20, 0.08). Rainfall uses 2 octaves at 0.03 / 0.09 (0.70 / 0.30). Halved frequencies (vs the older 0.06 base) give continent-scale biome patches across the 64×32 globe grid.
- **Cached globe (`world.bin`):** `globe.rs::load_or_generate` deserializes this file if present and skips regeneration. Delete `world.bin` after changing globe-level noise to see the effect; tile-level (`terrain.rs`) changes are always live.
- **`TileChangedEvent` pipeline:** Mutations to `ChunkMap` emit `TileChangedEvent`; `refresh_changed_tiles_system` (PostUpdate) rebuilds the affected tile sprites. Use this whenever code edits a tile in place.
- **`CameraViewZ`:** Player-controlled view Z-level — defaults to `i32::MAX` (surface). Lower it to peer underground; `update_tile_z_view_system` re-skins all tile sprites accordingly.
- **`sprite_library.rs`:** Procedural pixel-art sprites built from a 32-color warm earth-tone palette via `ascii_to_image` + char-substitution templates. Loaded at `Startup` by `setup_sprite_library`. New sprites should reuse the palette and substitution helpers rather than introducing new color systems.
- **PNG textures** in `assets/textures/` (e.g., `gatherer_s_a.png`, `wolf_anim_s_a.png`) are toggled via `entity_sprites::toggle_art_mode` as an alternative to the procedural sprites.

### Geology

- **Surface vs subsurface:** `terrain.rs::surface_kind_fn` chooses the surface tile (Water/Grass/Farmland/Forest/Stone) by biome thresholds. Below the surface, `proc_tile` runs a layered model: cave noise first, then biome-thick **topsoil** (Dirt), then **ore vein lookup**, falling back to bedrock (`TileKind::Wall`). Cave cavities take precedence over ore — anything carved by cave noise drops to Air/Dirt and the would-be ore is lost.
- **Topsoil depth (`topsoil_depth(biome)`):** Mountain 1, Desert/Tundra 2, Grassland 3, Taiga/Tropical/Temperate 4, Ocean 0. Mountains expose bedrock and ore almost immediately; forests have a deep dirt cap.
- **Ore veins (`ORE_BANDS` in terrain.rs):** Six 3D Perlin fields seeded `WORLD_SEED + 2..=7`, one per ore. Each band has a depth range (tiles below surface), threshold, and frequency. Ordered shallow → deep: Coal (1..6, 0.45), Copper (2..8, 0.50), Tin (5..12, 0.55), Iron (6..14, 0.52), Silver (10..18, 0.60), Gold (14..32, 0.65). Higher threshold = rarer. Bands skip the noise sample entirely when depth is out-of-range, keeping subsurface lookups cheap. Ore is encoded as `TileKind::Ore` + `TileData.ore: u8` (`OreKind` enum); `Wall` and `Stone` carry `OreKind::None`.
- **Two tile-read paths (footgun):** `chunk.rs::tile_at_local` (cache-only) returns `Wall` for any uncarved subsurface tile — passability/LOS-correct but **material-wrong**. For ore-accurate reads (mining yields, inspector display) call `terrain::tile_at_3d` instead. `carve_tile` does this internally to compute per-block drops.
- **Mining yields (`carve.rs::carve_tile`):** Returns `Vec<(Good, u32)>` — per-block drops based on the actual tile read via `tile_at_3d`. `Wall|Stone → (Stone, 2)`, `Ore → (ore_yield_good(ore), 2)` where `ore_yield_good` maps `OreKind → Good` (Coal/Iron/Copper/Tin/Gold/Silver). Floor is still rewritten to `Dirt` after carving for walkability. `gather.rs` and `dig.rs` route the drops through `route_yield`/`Carrier::try_pick_up`; `gather.rs` credits the matching `ActivityKind::*Mining` per drop and applies faction multipliers per-good.
- **`StoneProfile.bonus_yields` removed:** Surface Stone tiles no longer roll random Coal/Iron drops; ore is now strictly geological. Players (and the AI) must dig down to find ore. `COPPER_WORKING` triggers on `CopperMining`; `TIN_PROSPECTING` triggers on `TinMining` — both also still trigger on `StoneMining`.
- **`ActivityKind`:** `ACTIVITY_COUNT = 13`. New variants: `CopperMining = 9, TinMining = 10, GoldMining = 11, SilverMining = 12`. Update `ACTIVITY_KINDS`/`ACTIVITY_NAMES` (debug_panel.rs) and `activity_name` (tech_panel.rs) when adding more.
- **Rendering:** `TileMaterials.materials` and `FogTileMaterials.materials` are keyed by `(TileKind, OreKind, z_bucket)`. Non-ore tiles use `OreKind::None`; ore tiles fan out into per-ore color handles via `RENDERABLE_ORES` in `chunk_streaming.rs`. `color_map.rs::ore_tile_color` defines the per-ore sRGB. `resolve_render_tile` returns `(TileKind, OreKind, z, Visibility)`.
- **No save-format change:** `WorldGen` is reconstructed each launch from `WORLD_SEED` constants; chunk caches still hold only surface data; `world.bin` (`globe.rs`) is the globe biome grid only. Determinism is preserved by per-ore seed offsets.

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
