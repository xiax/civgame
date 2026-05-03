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
| `WorldPlugin` | `src/world/` | Procedural terrain (Perlin noise, Z-levels -16..+15), chunk streaming (32×32 tiles), biomes, calendar/seasons, `SpatialIndex` |
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plant growth, raids |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Flow-field caching, chunk-level navigation graph |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera (WASD/scroll), chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, economy panel, world map, right-click context menu, activity log |

### Simulation scheduling (`SimulationSet`)

Systems must be assigned to the correct set — misplacement causes subtle bugs:

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy systems (needs ticks, mood, LOD, goal updates, animal sensing)
- **ParallelB** — `goal_dispatch_system` (Sleep-only fallback; every other goal is plan-driven)
- **Sequential** — mutating systems with tight ordering: `gather` → `dig` / `construction` → `movement` → `combat` → `production` → `plan_execution`
- **Economy** — post-simulation: gossip, faction storage rollup, reproduction, raids, technology, market price updates

### Agent AI (Goals → Plans → Tasks)

- **Goals (`goals.rs`):** High-level objectives (`Survive`, `Gather`, `Defend`, `Farm`, `Build`, `Socialize`, ...) driven by Needs + Faction state.
- **Plans (`plan/`):** Multi-step sequences scored by static linear function `dot(state, plan.state_weights) + plan.bias` plus manual bonuses (persistence, ally influence, dist-weighted memory penalty). `PlanScoringMethod::Weighted` is normal; `Random` is ε-greedy. Module split: `plan/mod.rs` (types, `ActivePlan`/`PlanHistory`/`KnownPlans`, `resolve_target`, `plan_execution_system`), `plan/registry.rs` (step+plan tables), `plan/state.rs` (`build_state_vec` + `count_visible_*`).
- **PlanFlags (`PlanDef.flags`):** `PF_EXPLORE`, `PF_SCAVENGE`, `PF_TARGETS_FOOD/WOOD/STONE`, `PF_DROP_FOOD_ON_TIMEOUT`, `PF_UNINTERRUPTIBLE`. Candidate filter reads flags, not plan-id matches. `PF_UNINTERRUPTIBLE` plans (e.g. 7 BuildBlueprint, 13–16 craft pipeline, 29 HaulFromStorageAndBuild, 33 ClaimedHaul, 34 ClaimedBuild) survive a goal flip — they end only via completion, timeout, target invalidation, precondition fail, or external preempt.
- **Tasks (`tasks.rs`):** The agent's *current* action — `TaskKind` enum (Gather, Construct, WithdrawMaterial, Hunt, Butcher, Equip, ...). Transient; managed by `plan_execution_system`. `Idle` between tasks.
- **Professions (`person.rs::Profession`):** `None | Farmer | Hunter`. Persistent role. Farmer auto-assigned by `faction_profession_system` when food < 100. Hunter is chief-driven for every faction via `faction_hunter_assignment_system` (Economy, every `TICKS_PER_DAY/4`): target headcount = `max(1, adults*0.20)` × martial × prey-density, capped at adults/2; demotes do full teardown. Plans gate on `PlanDef.requires_profession`.
- **Skills (`skills.rs`):** `[u8; 8]` — Farming, Mining, Building, Trading, Combat, Crafting, Social, Medicine. Default 5; `gain_xp()` saturating.
- **Bucketing:** Agents sliced across 20 fixed `BucketSlot`s to spread CPU.
- **LOD:** `Detail / Aggregate / Dormant`. Dormant entities skip sim entirely. Focus-aware (see Game lifecycle).
- **Memory & gossip (`memory.rs`):** Known locations + agent sightings, `u8` freshness decay, shared via `plan::plan_gossip_system`.
- **Needs:** 6 needs (hunger, sleep, shelter, safety, social, reproduction), `[0,255]`, decay over time, feed goal selection.

### Plan-design rules of thumb

- **Plan scoring is *viability*, not motivation.** Inside a need-driven goal the triggering need is constant across candidates, so weighting it again is circular noise. Plan weights answer "would this succeed and produce value?" via `SI_HAS_*` (inventory), `SI_VIS_*` (visibility), `SI_MEM_*` (memory), `SI_SKILL_*`, `SI_STORAGE_*`. Need-slots 0–5 are populated for inspector readout but not currently weighted (`#[allow(dead_code)]`). Bias is for "this is the cheap-and-immediate right answer."
- **Source vs good visibility (`STATE_DIM=42`):** Sources (slots 35–37: `SI_VIS_PLANT_FOOD`, `SI_VIS_TREE`, `SI_VIS_STONE_TILE`) feed Forage/Gather. Goods (slots 38–40: ground wood/stone/food) feed Scavenge. Never collapse the two — gather/farm gate on source-vis OR memory; `PF_SCAVENGE` plans gate on the matching ground slot; `PF_EXPLORE` plans invert (need both source-vis AND ground-vis to be zero).
- **Explore is per-resource and self-cancelling:** `ExploreForFood/Wood/Stone` (35/36/37) carry `memory_target_kind`. `explore_satisfaction_system` (after `vision_system`, before `plan_execution_system`) ends the plan with `PlanOutcome::Success` the moment matching memory is recorded — workers don't run out the 5000-tick timeout.
- **Farming is `Farm`-goal only.** Plans 1 (FarmFood), 4 (PlantFromStorage), 66 (PlantBerryFromStorage) never appear under Survive/GatherFood. Seed↔plant mapping is centralised in `PlantKind::seed_good()` + `PlantKind::ALL` (`plants.rs`); the Planter executor walks `PlantKind::ALL` and consumes one seed from carrier-or-inventory via `consume_one_good` (`production.rs`). Adding a new seed/plant pair = new `Good` variant + new `PlantKind` variant + arm in `seed_good()` + arm in `Good::is_seed()`. `StepPreconditions::requires_good` checks inventory + carrier so harvested seeds (which land in hands) satisfy plant-step gating.
- **Faction storage stocks in state vector (slots 29–32, 34, 41):** `SI_STORAGE_FOOD/WOOD/STONE/GRAIN_SEED/BERRY_SEED`, normalised against `STORAGE_SATURATE=20`. Refreshed by `compute_faction_storage_system` (Economy). Lets withdraw/haul plans score on actual stock; lets producers self-throttle when storage is full.

### Faction systems

- **Factions (`faction.rs`):** Tech bitset (`u64`, ≤64 techs, Prehistoric → Bronze Age), `FactionCenter`, bonding, storage rollup, raids. `SOLO=0` = ungrouped. `StorageTileMap` indexes storage tiles per faction. `FactionStorage::totals` refreshed every Economy tick.
- **Construction (`construction.rs`):** `BlueprintMap`, `WallMap`, `BedMap`. `faction_blueprint_system` decides what to build; `construction_system` consumes resources and finalizes tiles/entities. `generate_candidates` paces hearths by era (Paleo/Meso `(members+5)/6` gated on crescent saturation + bed deficit; Neolithic `(members+7)/8` gated on each hearth having ≥8 beds in 2..6 crescent ring; Chalcolithic+ single civic hearth). Defensive walls (`PalisadeSegment`) are Chalcolithic+ only. `best_wall_material` ladder: Palisade < WattleDaub (`PERM_SETTLEMENT`) < Mudbrick (`FIRED_POTTERY`) < Stone (`COPPER_WORKING`).
- **CraftOrders + jobs:** `CraftOrder` carries `spawn_tick`; `faction_craft_order_system` despawns orders > `CRAFT_ORDER_TIMEOUT_TICKS=600`. `chief_job_posting_system` (`jobs.rs`) emits one `JobKind::Craft` per faction, picking the recipe with largest `demand-supply` (subject to tech + station availability — Workbench within 12 tiles of `home_tile`, Loom for loom recipes; Loom recipes pass `bench: None` because `job_claim_release_system` only validates Workbench). `resource_demand_system` (`faction.rs`) populates demand for crafted outputs as a fraction of `member_count`.
- **WithdrawMaterial intent:** Storage tiles hold many goods, so the *resolver* picks. All `TaskKind::WithdrawMaterial` steps share `StepTarget::WithdrawForFactionNeed { need, selector }` (`need`: Blueprint/CraftOrder/HaulClaim; `selector`: MostDeficient/Specific). One resolver, parameterised. Per-good deliver-plans removed; plan 15 `DeliverFromStorageToCraftOrder` covers everything.
- **StorageReservations resource:** `(tile, good) → reserved_qty`, mutex-wrapped (parallel resolver). Each successful `WithdrawMaterial` dispatch increments and stashes `(reserved_tile, reserved_good, reserved_qty)` on `PersonAI`; `release_reservation` decrements on every teardown path. Resolver subtracts reservations from raw `GroundItem.qty` so two agents can't commit to the same one-unit stack.
- **Activity log (`ui/activity_log.rs`):** Bottom-right egui panel; `ActivityLogEvent { tick, actor, faction_id, kind }` with kinds `Constructed`, `Crafted`, `TechDiscovered`, `RegionSettled`. Filtered to player faction; capped at 16 entries.

### Carrying & item routing

- **Carrier (`carry.rs`):** Two hand slots, separate from `EconomicAgent.inventory`. `Bulk` class — `TwoHand` (Wood, Stone, Iron) needs both hands empty; `OneHand`/`Small` need one. `enforce_hand_state_system` runs before gather/dig and consults `gather_target_yield_bulk` to upgrade requirements when a harvest yield is `TwoHand`. `Carrier::pickup_capacity(item)` mirrors `try_pick_up` for resolver pre-sizing.
- **Withdraw routes through hands first** (`production.rs::withdraw_material_task_system`): Stone weighs 5000g — same as the entire inventory cap — so storage withdraws offer to `Carrier::try_pick_up` first, then `EconomicAgent::add_good` for leftovers. Resolver sums `pickup_capacity + (inv_room/unit_w)`; if 0 it picks a different tile.
- **Eat from hands too** (`production.rs::eat_task_system`): Foraged Fruit/Meat/Grain are `Bulk::Small` and usually live in hands. `total_edible(agent, carrier)` covers both stores; `requires_any_edible` precondition uses the same.
- **Equipment unification (`economy/item.rs`, `simulation/items.rs`, `simulation/combat.rs`):** Combat stats live on `Item` (`weapon_stats`, `armor_stats`), built by `Item::new_manufactured(good, material, quality)`. `Equipment::items: HashMap<EquipmentSlot, Item>` (value-typed, no entity layer). `ArmorStats::covered_parts` is a 4-bit bitmask over `armor_coverage::{HEAD, TORSO, ARMS, LEGS}` so `Item` stays `Copy`. `TaskKind::Equip` (StepDef 56) drives `equip_task_system` (Sequential, after `item_pickup_system`): pulls highest-multiplier matching `Item` from inventory or `Carrier`, swaps slot, displaced occupant goes to inventory or ground via `spawn_or_merge_ground_item_full` (merges by full `Item` equality so manufactured stats survive storage round-trip).

### Hunting pipeline (`corpse.rs` + plans 5/64/65)

- Wolf/Deer no longer drop Meat/Skin on death — `combat.rs::death_system` strips AI/needs/species and inserts `Corpse { species, fresh_until_tick }`, indexed in `CorpseMap`. `corpse_decay_system` (Economy) despawns at `CORPSE_FRESHNESS_TICKS=600`.
- **Chief hunt orders (`HuntOrder` on `FactionData`):** `chief_hunt_order_system` posts daily (TICKS_PER_DAY=3600, staggered by `fid`). Scans `SpatialIndex` within `HUNT_SCAN_RADIUS=40` for prey, posts `Hunt { species, area_tile, target_party_size = 4(Wolf)/2(Deer), mustered, deployed_tick }` or `Scout`. Mid-day invalidation sweep clears stale orders.
- **Plan 5 HuntFood** (hunter-only, `HUNTING_SPEAR` gated, `PF_UNINTERRUPTIBLE`, gated on `Hunt`): muster at hearth → travel → hunt → PickUpCorpse → HaulCorpse → Butcher (60 ticks → drops `species_yield()` Meat+Skin, despawns corpse).
- **Plan 65 ScoutForPrey** (gated on `Scout`): single explore step writes `MemoryKind::Prey`; chief flips to Hunt next decision. Not `PF_UNINTERRUPTIBLE`.
- **Plan 64 AcquireHuntingSpear:** WithdrawGood(Weapon) → Equip(MainHand). `StepPreconditions::forbids_good` checks `Equipment::has_good` so it self-deselects after equip.
- `corpse_follow_system` (Sequential, after `movement_system`) snaps corpse Transform to carrier. `respond_to_distress_system` recruits any same-faction Hunter within `HUNTER_RESPOND_RANGE=50` regardless of LOS, dropping `carried_corpse`.

### Player UI

- **Right-click context menu (`ui/orders.rs`):** Three sections — tile actions (Move/Mine/Gather/Dig Down/Deconstruct/Build), entities on tile (Attack / Pick up corpse), ground item stacks (Pick up: Nx Good). `PlayerOrderKind::{PickUpItem, AttackEntity, PickUpCorpse}` route to Scavenge/MilitaryAttack/PickUpCorpse tasks. `TileDisplayQueries` and `RoutingResources` are `SystemParam` bundles to stay under Bevy's 16-param limit.
- **Inspector (`ui/inspector.rs`):** Drop / unequip / equip buttons queue actions via `PendingInspectorAction`; `inspector_action_system` executes after panel render. `valid_equip_slots(good)` in `items.rs` maps goods to slots.
- **Muster button (`military.rs`):** Sets `MusterHuntersRequest.pending`; `apply_muster_hunters_system` (Economy) inserts `Drafted` on every player-faction Hunter, clearing plan/reservations/carried corpse. Player issues rally point separately via right-click. Orthogonal to chief muster (uses `HuntOrder::Hunt::mustered`, not `Drafted`).

### World generation

- **Terrain noise (`terrain.rs::surface_v`):** 4-octave FBM (continental 0.005 / base 0.02 / 2× / 4× harmonics, weights 0.35/0.40/0.18/0.07), reshaped via `sign(c)*|c|^0.65` to push to Z extremes. Lowering base freq or power = bigger / more dramatic features.
- **Globe pipeline (`globe.rs::generate_globe`):** 256×128 climate-cell grid. (1) `plates.rs` Lloyd-relaxed Voronoi over `NUM_PLATES=8` with motion vectors → uplift/subsidence. (2) Heightmap = 70% multi-octave Perlin + 30%×1.4 plate uplift. (3) `erosion.rs` thermal(20) → hydraulic(40). (4) `hydrology.rs` pit_fill → flow_dirs (D8) → flow_accum → `extract_rivers(min_accum=80)` + `LakeMap`. (5) `climate.rs` temp from latitude+elev; rainfall = base Perlin + orographic along latitude-banded prevailing winds. (6) Per-cell biome via `biome::classify`. ~50ms.
- **Climate-grid vs mega-chunk decoupling:** `GLOBE_WIDTH=256, GLOBE_HEIGHT=128, GLOBE_CELL_CHUNKS=4` → climate cell = 4×4 chunks = 128×128 tiles. `MEGACHUNK_SIZE_CHUNKS=16` → mega-chunk = 16×16 chunks = 512×512 tiles = 4×4 climate cells. Total world: 1024×512 chunks ≈ 32K×16K tiles.
- **Continuous climate:** `Globe::sample_climate(tile_x, tile_y)` bilinearly interpolates the four nearest cells (X-wrap, Y-clamp). `biome::classify_at_tile` runs *per-tile* in `generate_chunk_from_globe` and `tile_at_3d` — no biome stripes at cell boundaries.
- **Rivers & lakes** stamped into chunks: `bresenham_stamp`s a Water swath along river edges (depresses `surface_z` by 1); lakes flood-fill discs with Water at `lake.level_z`. Both pre-computed in `globe`.
- **Cached globe (`world.bin`):** Versioned `GlobeFile { version, globe }`; bump `GLOBE_FILE_VERSION` (currently `2`) on serialised-layout changes — auto-regenerates on mismatch. Determinism via per-component seeded RNGs.
- **Loose rocks (`chunk_streaming.rs::spawn_chunk_loose_rocks`):** Scatters Stone GroundItems (qty 1–3) on ~35% of surface Stone tiles using `ROCK_HASH_SEED=0xDEAD_C0DE`. Immediately scavengeable.

### Geology & mining

- **Surface vs subsurface:** `surface_kind_fn` chooses surface tile by biome thresholds. Below: `proc_tile` runs cave-noise → `topsoil_depth(biome)` of Dirt → ore vein lookup → `Wall` bedrock. Caves take precedence over ore. `topsoil_depth`: Mountain 1, Desert/Tundra 2, Grassland 3, forests 4, Ocean 0.
- **Ore veins (`ORE_BANDS`):** Six 3D Perlin fields, seeded `WORLD_SEED+2..=7`. Shallow→deep: Coal (1..6, 0.45), Copper (2..8, 0.50), Tin (5..12, 0.55), Iron (6..14, 0.52), Silver (10..18, 0.60), Gold (14..32, 0.65). Encoded as `TileKind::Ore` + `TileData.ore: u8` (`OreKind`). Surface stone never drops random ore (no `bonus_yields`) — players must dig down.
- **Mining yields (`carve.rs::carve_tile`):** `Vec<(Good, u32)>` per block. `Wall|Stone → (Stone, 2)`; `Ore → (ore_yield_good(ore), 2)`. `gather.rs`/`dig.rs` route via `route_yield`/`Carrier::try_pick_up` and credit the matching `ActivityKind`. `ActivityKind` count: 13 (incl. CopperMining=9, TinMining=10, GoldMining=11, SilverMining=12; update `ACTIVITY_KINDS`/`ACTIVITY_NAMES` in debug_panel.rs and `activity_name` in tech_panel.rs when adding). When the floor is already passable (procedural topsoil Dirt), `carve_tile` still writes a delta with the existing kind so that `Chunk::set_delta` refreshes `surface_kind` off its `Air` placeholder — without this the right-click menu hides Dig Down on subsequent clicks within the topsoil column.
- **⚠️ Two tile-read paths:** `chunk.rs::tile_at_local` (cache-only) returns `Wall` for any uncarved subsurface tile — passability/LOS-correct but **material-wrong**. For ore-accurate reads (mining yields, inspector display) call `terrain::tile_at_3d`.

### Game lifecycle and regions

- **`GameState`:** `SpawnSelect` (default) | `Playing`. Globe gen at `WorldPlugin::build`. Spawn-select UI in `SpawnSelect`; spawn systems on `OnEnter(Playing)`. Update-stage systems gated `in_state(Playing)`; FixedUpdate sim systems are not gated (queries empty until entities spawn).
- **`PendingSpawn(Option<(i32, i32)>)`:** Player's chosen mega-chunk; `None` = globe centre (sandbox).
- **Sandbox bypass:** `SandboxPlugin` flips state to `Playing` at Startup, skipping spawn-select.
- **`SettledRegions` (`region.rs`):** `AHashMap<RegionId, SettledRegion>` + `by_megachunk` reverse index. `SettledRegion { megachunk, founding_tick, name, camera_bookmark, player_owned }`. `settle()` idempotent.
- **`MegaChunkCoord`:** `from_chunk/from_tile/center_tile/chunk_range`. `MEGACHUNK_SIZE_CHUNKS=16`, independent of climate cells.
- **Spawn-select UI (`ui/spawn_select.rs`):** Full-screen biome map with mega-chunk grid overlay; click on habitable cell sets `PendingSpawn`. Ocean/Mountain non-habitable.
- **Multi-focus chunk streaming (`SimulationFocus`):** `Vec<FocusPoint>` rebuilt each tick from camera + every settled region centre. Chunk DATA loads for any focus disc (camera `LOAD_RADIUS=12`, region `REGION_LOAD_RADIUS=6`); SPRITES + plants + loose rocks only inside the camera focus. Lets every settled region run sim in parallel without N regions of sprites.
- **Focus-aware LOD (`update_lod_levels_system`):** Camera distance produces base LOD; entities within 8 chunks of any non-camera focus are promoted from Dormant to Aggregate so off-screen agents keep ticking.
- **Edge-walk expansion (`region::detect_edge_crossing_system`, Economy):** Each tick checks every player-faction Person's mega-chunk; if unsettled, calls `settle()`, names "Outpost N", bookmarks position, emits `ActivityLogEvent::RegionSettled`. Agent walks across organically; chunk data already loaded by camera following them.
- **World-map switcher (`world_map_system`):** Settled mega-chunks outlined (yellow=player, red=other). Click bookmarks current camera onto the region containing it, then jumps to target's `camera_bookmark`. Tab toggles map.

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
