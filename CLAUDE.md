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
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plants, raids, diplomacy + territory |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Component-typed chunk graph, hotspot flow fields |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera, chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, world map, right-click menu, activity log |

Per-directory `CLAUDE.md` files cover subsystem detail and are auto-loaded when reading/editing in those trees.

## Knowledge system (`simulation/{technology,knowledge,knowledge_bits,knowledge_catalog,building_technique}.rs`)

Catalog of 86 `TechId`/`KnowledgeId` entries spanning four axes:

- **Kind** (`KnowledgeKind`) — `PracticalSkill` / `PracticalTechnique` / `Belief` / `Lore`. Skills + techniques carry optional mastery 0..=3 in `PersonKnowledge.mastery`; beliefs carry confidence in `PersonKnowledge.belief` per `BeliefGroupId`; lore is learned but never mastered.
- **Domain** (`KnowledgeDomain`) — Subsistence / Craft / Construction / Transport / Institutional / Medicine / Cosmology / Lore / Martial.
- **Truth** (`TruthStatus`) — True / FalseUseful / FalseHarmful / Contested. Almost all skills are True; beliefs span the range.
- **Adoption scale** (`technology_adoption::AdoptionScale`) — Personal / Household / Subsistence / Specialist / MilitaryTransport / Institutional. Drives founder-Learned seeding.

Layout: 50 core techs (`FIRE_MAKING 0` → `POWERED_TRACTION 49`) + 14 building techniques (50–63) + 16 foundations (64–79) + 6 beliefs (80–85). `KnowledgeBits` is a 128-bit fixed-width bitset for `aware`/`learned`. `KNOWLEDGE_META[TECH_COUNT]` array layers domain/kind/truth/belief_group onto each id; `knowledge_def(id)` bundles `TechDef` + `KnowledgeMeta` into one view.

**Building techniques** (`building_technique.rs`): `BuildingTechnique` enum (Wattle Screens → Hydraulic Masonry) gated by `KnowledgeId`. `select_building_technique(techs, locality, purpose)` picks the cultural method from Learned-pool × `LocalSiteContext` (forest density, clay/stone/wetland/river-silt proximity, biome) × `StructurePurpose`. Each technique's `output_material()` resolves to one of the 5 existing `WallMaterial` tiers.

**Construction materials** (`economy/resource_catalog`): `clay`, `reeds`, `thatch`, `limestone`, `lime`. `CraftRecipe 46 (Burn Lime)` — `FIRED_POTTERY`-gated, 2 limestone + 1 wood → 1 lime. Grain harvest co-yields 1 thatch. Mined Limestone tiles yield `limestone` (other lithologies map to generic `stone`).

**Foundations**: universal `AdoptionScale::Personal` knowledge auto-Learned by every founder at-era (Paleolithic: Fire Use, Ember Carrying, Toolstone Recognition, Edge Geometry, Cordage, Hafting, Hide Working, Animal Tracking, Seasonal Memory, Oral Tradition, Route Memory, Water Source Memory; Neolithic adds Clay Tokens, Measures & Units, Ration Arithmetic, Practical Geometry).

**Beliefs** (`PersonKnowledge.belief: AHashMap<BeliefGroupId, BeliefState>`): three groups (cosmology / disease_causation / omens). Seeded by `seed_initial_beliefs(target_era)`: pre-Neolithic → Sky Dome + Spirit Illness + Eclipse Omens; Neolithic+ → Geocentric + Miasma + Weather Omens. Beliefs never land in `learned` — `seeded_realistic_through_era` skips them. `FactionData.chief_disease_belief` / `chief_cosmology_belief` cached, refreshed every Economy tick by `sync_faction_techs_from_chief_system`. Consumer hooks:
- `MIASMA_THEORY` accepted → +30% `WaterAccess` intent priority (`organic_settlement::pressure_to_intent`).
- `SPIRIT_ILLNESS` accepted → +30% `Ritual` intent priority AND injured patient skips home_tile fallback in `htn_seek_care_dispatch_system`; Healer-side radius scan still reaches them.
- `ECLIPSE_OMENS` accepted + `Calendar::eclipse_active()` → `ECLIPSE_OMEN_MOOD_PENALTY = 25` in `mood::derive_mood_system`. Eclipse cadence in `world/seasons.rs::is_eclipse_today` / `eclipse_active`.

UI: inspector Beliefs subsection per agent; tech panel Beliefs section per faction. Per-era tech list filters Belief-kind entries out.

## Game-start options (`GameStartOptions`, `game_state.rs`)

Read once by `spawn_population` + `seed_starting_buildings_system`:
- `era: Era` — every member starts Aware of techs through this era. Learned set role-scoped via `PersonKnowledge::seeded_realistic_through_era` (chief: Personal+Household+Subsistence+Specialist+Institutional; ~1/8 Specialist; rest Common). Foundations auto-Learn; beliefs seed via `seed_initial_beliefs(target)`.
- `player_population: u32` — group size for player faction (others use `GROUP_SIZE=20`).
- `economy: EconomyPreset` — `Subsistence` / `Mixed` / `Market`. Applied via `policy::apply_preset`.
- `lifestyle: Lifestyle` — `Settled` (default) or `Nomadic`. Player-faction only; AI stays Settled.
- `seed_buildings: bool` — sandbox sets false.

## Game lifecycle (`GameState`, `game_state.rs`)

`GameState::{MainMenu, SpawnSelect, MultiplayerLobby, Playing}` (default `MainMenu`). `GameStatePlugin` owns lifecycle resources (`PendingSpawn`, `PendingStarts`, `GameStartOptions`) and `legacy_pending_spawn_compat_system` (PreUpdate) which mirrors `PendingStarts.primary_start → PendingSpawn.0` for legacy single-slot readers.

**`PendingStarts { primary_start, slots: Vec<PlayerStartSlot> }`** is the multi-slot input read by `spawn_population`. `PlayerStartSlot { slot_id, player_name, client_id, megachunk, lifestyle, ready, faction_id }`. SP = one slot at `HOST_SERVER_LOCAL_CLIENT_ID = 1`; MP = one per joined client. `spawn_population` iterates slots for humans (each home via `pick_player_home_in_megachunk`), then loops up to `(NEARBY_RIVAL_COUNT + 1) − human_count` AI rivals scored against the union of human homes. `spawn_world_system` pre-gens the union of windows around every human start. Auto-fills one SP slot when `slots.is_empty()` — covers headless test fixture.

UI flow: MainMenu → SpawnSelect (SP) or MultiplayerLobby (MP) → `Playing`. `main_menu_boot_route_system` auto-routes to `MultiplayerLobby` when launched with `--listen` / `--connect`.

## LAN multiplayer (`src/net/lan.rs`, `src/net/lobby_state.rs`, `src/ui/{main_menu,lobby}.rs`)

**Topology:** re-launch the binary on Host/Join. `ui::main_menu::relaunch_as_host()` spawns parent exe with `--listen --bind 0.0.0.0:5000 --player NAME` and `exit(0)`s the menu — Lightyear consumes its NetConfig at install time, no live transport swap.

**Discovery:** UDP broadcast on port 5001 (distinct from game port 5000). `LanAdvert { protocol_version, game_name, host_name, game_port, players, max_players, phase: AdvertPhase, world_seed }` bincode-serialised (< 512 B); `BROADCAST_INTERVAL = 1s`, `LISTEN_TTL = 3s`. Listener thread spawned from `NetPlugin::build` for every non-Dedicated mode. Single-machine LAN tests see loopback via `255.255.255.255` send.

**Lobby protocol (`PROTOCOL_VERSION = 6`):** Client→Server `LobbyJoin/SelectStart/SetReady/Leave`; Server→Client `LobbySnapshot/Reject/StartGame` on `OrderedReliableChannel`. Pure validators (`is_start_megachunk_acceptable` for `MIN_HUMAN_MEGACHUNK_DISTANCE = 3`; `lobby_ready_to_start`; `LobbyState::is_select_acceptable`) unit-tested without an App. `LobbyState { phase: Hosting → SelectingStarts → Starting → InGame, config, slots, version }` with reclaim-by-name `accepts_join`; `bump()` auto-advances `SelectingStarts → Starting` when every slot has `megachunk.is_some() && ready`.

**Lobby wiring (Phase 8):** `server::handle_lobby_{join,select_start,set_ready,leave}_system` drain the wire + `LocalLobbyCommand` (host UI bypass) channels. `server::broadcast_lobby_snapshot_system` ships `LobbySnapshot` to every connected client whenever `lobby.version` bumps past `last_sent_version` (`Local<u32>`). `server::start_game_transition_system` fires when `phase == Starting`: allocates a monotonic `faction_id` per slot (sorted by `slot_id`), broadcasts `LobbyStartGame`, populates `PendingStarts.{slots, primary_start}` + `GameStartOptions` from `lobby.config`, transitions `GameState::Playing`, phase → `InGame`. Client side: `client::apply_lobby_snapshot_system` mirrors snapshots onto `LobbyUiState.remote_slots` + `WorldSeed/GameStartOptions`; `client::apply_lobby_start_game_system` builds `PendingStarts` from `slot_assignments` and flips into `Playing`. Lobby UI: `ui::lobby::LobbyCommandChannels` SystemParam routes per `NetMode` — host writes `LocalLobbyCommand` events, remote client ships via `OrderedReliableChannel`. `ui::lobby::auto_join_lobby_on_enter` (OnEnter MultiplayerLobby) announces the local player.

**Authority:** `command_loopback_system` runs in every mode except `Client`. `SimulationSet::{Input, ParallelA, ParallelB, Sequential, Economy}` + `EconomyPlugin` + `PathfindingPlugin` mutating systems gate on `net::net_mode_runs_sim` (`Option<Res<NetMode>>` — missing reads as `runs_sim = true` so headless test fixtures keep ticking). `client::apply_bootstrap_snapshot_system` sets `WorldSeed`, fires `RegenerateWorldRequest`, writes `PendingStarts.primary_start` from the assigned faction's `home_tile`, and triggers `NextState(Playing)` — canonical mid-game reconnect path. HUD speed controls disable + "MP — speed locked to 1×" when `ConnectedRemotes.count > 0`. `apply_disconnect_policy_system` decrements `ConnectedRemotes` so `speed_lock_system` releases on last remote leave.

**Plant + structure replication (Phase 8.5):** `TileOverlayOp::{AddPlant, RemovePlant, AddStructure, RemoveStructure}` ride the per-tick `replicate_tile_overlays_system`. `push_ops_for_tile` reads `PlantMap` + `StructureIndex` and emits the wire ops; `structure_kind_wire_from_label` buckets `StructureLabel` strings into `Bed/Workshop/Campfire/Storage/CivicAnchor`; `label_hash_u16` salts the label name into a stable u16. Client `apply_overlay_delta` populates `ReplicatedPlantMap` / `ReplicatedStructureMap` (tile→entity, mirror of server's `PlantMap`/`StructureIndex`). `server::emit_tile_changed_for_replicated_entities_system` fires `TileChangedEvent` on `Added<Plant>` / `Added<StructureLabel>` so spawn events flow through the standard replication cadence.

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
- Fixed update **20 Hz** (`main.rs`). Game speed (`Paused / 1× / 2× / 5×`) lives on `Time<Virtual>` via `GameSpeed` (`simulation/speed.rs`); higher presets fire FixedUpdate more often per real second. `SimClock.scale_factor()` carries bucket compensation only. `SimTimingDiagnostics` reddens when avg tick CPU > `SpeedPreset::budget_ms_per_tick()` (50/25/10 ms).
- After mutating a tile, emit `TileChangedEvent { tile }`; `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- `CameraViewZ` defaults to `i32::MAX` (surface); lower to peer underground.
- `ChunkMap::vertical_clearance_at(x, y)` counts open `Air`/`Ramp` Z-levels above surface — for tall multi-Z vehicles.
- **`SpatialIndex`** (`world/spatial.rs`) is incremental: every indexed entity carries `Indexed { kind, tile, z }`. `sync_indexed_after_move_system` (Sequential) handles add/move via `Or<(Changed<Transform>, Added<Indexed>)>`; an `on_remove` hook handles despawn. New spawn sites for indexed kinds **must** include `Indexed::new(...)`. Sites mutating `PersonAI.current_z` without touching `Transform` must call `transform.set_changed()`.

### Tile palette

`TileKind` has 26 variants:
- **Surfaces**: `Grass`, `Forest`, `Sand`, `Snow`, `Marsh`, `Scrub`, `Water`, `River`, `Road`.
- **Stone lithologies** (`is_stone_like`): `Stone` (legacy), `Granite`, `Limestone` (yields 3 vs. 2), `Sandstone`, `Basalt`, plus underground `Wall` and `Ore`.
- **Soils** (`is_soil_like`): `Dirt`, `Loam` (1.5×), `Silt` (1.4×, riparian), `Clay`, `SandySoil` (0.6×), `Cropland` (1.3×).
- **`Bridge`** — passable, road-speed, reports `is_freshwater()` (water flows under decking).
- **`Dam`** — constructed barrier across a watercourse. Passable + road-speed (crest carries a road) but **not** water-like / freshwater / drinkable. Durable truth is the `Dam` entity in `DamMap`; tile kind is its cache projection, restamped on chunk reload. Tech-gated on `DAM_BUILDING` (Bronze; prereqs `BRIDGE_BUILDING` + `MONUMENTAL_BUILDING`).
- **`Cropland`** — tilled farm soil. Only inside an Agricultural plot. Worked into existence by `farm::prepare_field_task_system` (`FIELD_PREP_WORK_TICKS=80`). `carve_plots_system` populates `PlotIndex.ag_tiles` + per-tile `farm::FieldTileIndex` but leaves the underlying soil/grass; founders pay Spring 1 to till. Personal kitchen gardens are real Agricultural parcels (`organic_settlement::append_dwelling_gardens`). `is_soil_like`, speed 0.9, never paved by road carving.

Helpers: `stone_yield_count`, `soil_fertility_mult`. No `Farmland` variant — Grain grows on `Cropland`; world-gen `TileData.fertility` is immutable per-tile recovery ceiling; `FieldTileIndex.by_tile[tile].nutrients` is the live nutrient pool. Pathing speeds: Sand 0.75, Snow 0.6, Marsh 0.4, Scrub 0.9, soils 0.85–0.9 (Cropland 0.9), stone 1.0.

**Diplomacy & Territory** — sparse per-tile `TerritoryMap` derived from live `Settlement` + pitched `Camp` anchors. `DiplomacyLedger` (faction-pair) holds `TreatySet` (Trade/Alliance/NonAggression/War — War is exclusive) + four `Reputation` tracks with daily half-life decay. AI proposes/responds on daily / quarter-daily cadences. Trespass rides `Changed<Transform>` and escalates via `TerritoryDefenseQueue`. See `src/simulation/CLAUDE.md` → Diplomacy & Territory.

**Incremental excavation** (`simulation::excavation`) — stone/ore Mine and Dig Down advance through 7 levels. Levels 1-6 slow traversal (0.92×..0.52×) and grant ranged cover (5%/level, cap 30%); level 7 finalises the carve. Bare hands cap at `HAND_DEPTH_LIMIT=3` on stone-like tiles. State in `ExcavationMap` Resource, cached as 3 bits in `TileData.flags`.

`river_distance_at(tx, ty)` returns chebyshev tiles to nearest river (`u8::MAX` = far/unloaded).

## Rendering conventions

- `TileMaterials` / `FogTileMaterials` keyed by `(TileKind, OreKind, z_bucket)`. Ore tiles fan out via `RENDERABLE_ORES`; colors in `color_map.rs::ore_tile_color`.
- **`sprite_library.rs`** — procedural pixel art from a 32-color palette via `ascii_to_image`. Reuse the palette/helpers; don't introduce a new color system.
- **PNG textures** in `assets/textures/` toggled by `entity_sprites::toggle_art_mode`.
- **`AnimalTextures`** — 8-direction PNGs for Wolf/Deer/Horse/Cow/Cat from `textures/<species>/rotations/`; ascii fallback (`creature_*` keys) otherwise.
- **GroundItem sprites** — `entity_sprites::spawn_ground_item_sprites` reactively attaches a child sprite via `ResourceDef.sprite_key`. Add by registering a key in `sprite_library.rs` and pointing the catalog entry at it.
- **Vehicle part sprites** (`rendering/vehicle_part_sprites.rs`) — hand-drawn ASCII art per `VehiclePartKind` + variant + multi-cell weapon-module composite. Three views (`VehicleSpriteView::{Side, Front, Back}`); `view_for_heading` maps 0=Back / 1=Side(flip) / 2=Front / 3=Side. Asymmetric parts ship distinct `_BACK` art; symmetric parts re-register `_FRONT` under `_back`. `entity_sprites::vehicle_sprite_plan_with_data(design, heading, &VehicleData)` populates per-cell sprite plan; multi-cell weapon modules collapse to one composite. Connector overlays bridge cells (`push_connector_overlays` walks `NEIGHBORS_6` and emits seam/attach/axle-wheel sprites).
- **Water-current streaks** (`rendering/water_current_render.rs`) — animated flow-streak sprites on flowing-`River` tiles; scrolls along `world::water_current::WaterCurrentField`. z=0.35, `ProjectedAnchor::Static`.
- **Day-night overlay** (`rendering/day_night.rs`) — full-screen sprite at z=90 tinted by `Calendar::day_fraction()`; per-entity fog tinting multiplies below.
- **Tilted-view projection** (`rendering/projection.rs`) — `MapViewMode::{TopDown, Tilted}` (toggle `V` / HUD). Symmetric pre/post: `revert_view_projection_system` (PreUpdate) strips so sim sees logical Transforms; `apply_view_projection_system` (PostUpdate) re-projects. `ProjectedAnchor::{Static{z}, Dynamic}` auto-attached per marker by `auto_attach_dynamic::<T>`. Helpers bundled in `ViewProjection` SystemParam. `CursorParams::pick_cliff_aware` walks `[Z_MIN, Z_MAX]` matching `surface_z_at`. Bookmarks store **logical** coords. `ElevationSkirt` renders south-facing cliffs.

## Network IDs (`net_id.rs`)

`NetId(u32)` is the stable, serializable handle for any entity that can ever cross a network boundary or be referenced from a player command. `auto_tag_replicable<T>` observers (`NetIdPlugin`, PostUpdate, before `assign_net_ids_system`) auto-insert `NeedsNetId` on every freshly-added `Person / Vehicle / GroundItem / Plant / Corpse / Blueprint / Bed / Door / Wall / Workbench / Campfire / Settlement / Camp`. `assign_net_ids_system` swaps it for `Networked(NetId)` and registers in `NetIdMap` (bidirectional `Entity ↔ NetId`). IDs monotonic per session, **never reused**. Server resolves `NetId → Entity` via `NetIdMap.entity_of`; client populates a separate map from replication.

**`PlayerCommand` entity-target variants** (`PickUpItem`/`PickUpCorpse`/`AttackEntity`/`Teach`/`MilitaryAttack`/`VehicleOrder`) carry `NetId`, not raw `Entity`. UI builds commands through `CommandSender` SystemParam — its `net_id_for(entity)` calls `NetIdMap::lookup_or_alloc`. Dispatch / lifecycle resolve back to `Entity`; unresolvable id → `CommandFailure::TargetGone`.

## Network boundary (`src/net/`)

`NetPlugin` registers `NetMode` (`Local` default; `ListenServer`/`DedicatedServer`/`Client` from CLI) and the `NetPlayerCommandEvent { sender_faction_id, actors, command }` channel. `command_loopback_system` (`PreUpdate`) validates `sender_faction_id` against `ControlledFactions` and re-emits as `PlayerCommandEvent`. Every UI command flows through this even in single-player so the server-auth path never atrophies. `CommandSender` writes `NetPlayerCommandEvent` and fills `sender_faction_id` from `Res<PlayerFaction>`; faction-level variants additionally carry `faction_id` in their payload. Sim-internal sites (executors, tests) continue to write `PlayerCommandEvent` directly.

**CLI (`src/net/cli.rs`):** `NetConfig { mode, bind_addr, connect_addr, player_name, on_disconnect }`. `--listen`/`--server` + `--bind` → `ListenServer`/`DedicatedServer`; `--connect` + optional `--player NAME` → `Client`; bare `cargo run` stays `Local`. `--on-disconnect=ai-takeover|pause|drop-faction`. `DedicatedServer` switches to `MinimalPlugins + LogPlugin` (no window, no rendering).

**Lightyear install per `NetMode`** (`NetPlugin.build`):
- `Local`: loopback only.
- `DedicatedServer`: `ServerPlugins` (UDP `--bind` + `NetcodeConfig` with shared `DEV_NETCODE_KEY` and `NETCODE_PROTOCOL_ID = PROTOCOL_VERSION as u64`) + `ProtocolPlugin` + server systems.
- `ListenServer`: Dedicated + `ClientPlugins` (`NetConfig::Local { id: HOST_SERVER_LOCAL_CLIENT_ID=1 }`) so host plays through the same path as remotes.
- `Client`: `ClientPlugins` (Netcode over UDP) + `ProtocolPlugin` + client systems. Client id derived deterministically from `--player NAME` via `derive_client_id(name)` using `ahash::RandomState::with_seeds(..)` with a fixed seed quad (NOT `AHasher::default()`, which is process-keyed and would break reconnect-by-name).

**Protocol (`PROTOCOL_VERSION = 6`, `net/protocol.rs`).** All serde-derived, ride `OrderedReliableChannel`:
- Bootstrap: `ClientHello`, `FactionAssignment { faction_id, world_seed }`, `BootstrapSnapshot { server_tick, calendar, factions, settlements, controlled_factions, overlay_tiles, interest_chunks }`.
- Overlays: `ChunkOverlayDelta { chunk, ops: Vec<TileOverlayOp> }` for Walls/Doors/Bridges/Dams/RuntimeWater/Plants/Structures (idempotent Add/Remove/SetOpen/SetRuntimeWater/etc.).
- Entity state: `EntityStateDelta { server_tick, chunk, entries: Vec<EntityStateEntry { net_id, kind, tile, z, facing, health_current, health_max, faction_id }> }`; `EntityKindWire::{Person, Animal(species), Vehicle}`. `EntityRemoved { net_ids }`.
- Commands: `NetCommandFrame { command_id, sender_faction_id, actors, command }` + `NetCommandAck { command_id, status: Accepted/OwnershipRejected/AllActorsGone, reason }`. `CommandId(u32)` monotonic, LOCAL sentinel = 0.
- Misc: `ClientCameraFocus { tile }`, `InspectorSummaryRequest/Response`.

`PlayerCommand` and every sub-type (`BuildSiteKind`/`WallMaterial`, vehicle types, `VehicleOrderKind`, `MigrationIntent`, `PackedMigrationAutonomy`, `ResourceId`) derive `Serialize`/`Deserialize`. Bincode round-trip tests in `net/protocol.rs`. Pure-fn `snapshot_*_map` / `apply_*_snapshot` helpers in `net/snapshot.rs`.

**Server (`net/server.rs`).** `handle_client_hello_system` validates `protocol_version`, reclaims via `PendingReconnect.take(&player_name)` if inside the `RECONNECT_GRACE_TICKS = 1200` (~60 s) grace window, else allocates fresh via `allocate_free_faction`; ships `FactionAssignment` + `BootstrapSnapshot` either way. `replicate_tile_overlays_system` dedups `TileChangedEvent`, groups by chunk, ships per-recipient via interest filter. `replicate_entity_state_system` (Update, `ENTITY_REP_INTERVAL_UPDATES = 3` ≈ 50 ms) samples Networked Persons/Vehicles/animals, batches by chunk, tier-rate-limited (Owned 50 ms / Neighbour 100 ms / Far 500 ms via `tier_cadence` and `send_index`). `replicate_entity_removals_system` coalesces by recipient via `LastKnownChunkMap`, broadcasts unrecorded ids. `receive_command_frames_system` validates ownership + acks. `expire_pending_reconnects_system` (1 Hz) GCs.

**Client (`net/client.rs`).** `send_client_hello_system` on `ClientConnectEvent`. `apply_bootstrap_snapshot_system` drains `FactionAssignment` + `BootstrapSnapshot` (auto-spawns `Networked(NetId)` stubs via `ensure_stub` + `NetIdMap::bind`) + `ChunkOverlayDelta` + `NetCommandAck` (bounded `ClientAckLog` cap 32). `send_command_frames_system` reads `NetPlayerCommandEvent`, stamps `ClientCommandSequencer.next()` id, ships. `apply_entity_state_delta_system` auto-spawns stubs and upserts `Transform` + `ReplicatedEntity` + `ReplicatedEntityKind` marker. `send_camera_focus_system` ships `ClientCameraFocus` every 500 ms when chunk shifts. `rendering::fog::fog_update_system` runs the same per-agent LOS sweep over `ReplicatedEntityKind::Person` stubs; walls stamped with `owner_faction` (placeholder material) so `has_vision_los` reads the same owner check on both sides.

**Interest rooms.** `ConnectionState.interest_chunks: AHashMap<(i32, i32), InterestTier>`. `compute_interest_system` (Update, every `INTEREST_REBUILD_INTERVAL_UPDATES = 30` ≈ 500 ms) rebuilds anchors per client from faction home + every owned `Settlement::market_tile` + camera focus chunk + active-military positions (`raid_party`, `HuntOrder::Hunt.mustered`, `Drafted` Persons). Tiers: `Owned` ±1, `Neighbour` ±`NEIGHBOUR_TIER_RADIUS = 2`, `Far` fills `INTEREST_RADIUS_CHUNKS = 4`.

**Disconnect policy + speed lock.** `DisconnectPolicy::{AiTakeover (default), Pause, DropFaction}`. AiTakeover/DropFaction remove from `ControlledFactions`; Pause flips `Time<Virtual>::pause()`. `ConnectedRemotes` + `speed_lock_system` clamps to 1.0 while any remote connected.

**Diagnostics.** `ReplicationStats` (raw counters + `*_per_sec` snapshots for entity deltas/entries/bytes, tile overlay deltas/bytes, removed msgs/bytes). `report_replication_stats_system` (Update, every `STATS_REPORT_INTERVAL_UPDATES = 60` ≈ 1 s) emits `debug!` line under `RUST_LOG=civgame::net=debug`. Bytes via `bincode::serialized_size`.

**Why custom snapshot over Lightyear `Replicate`:** mirrors `ChunkOverlayDelta` shape, avoids 20+ component registrations, keeps interest-gating + rate-tiering one cadence constant away.

## Tools (`simulation/tools.rs`)

`ToolForm` (Knife/Axe/Pick/Hammer/Sickle/Awl/FishingKit) = *what*; `Item.material+quality` → `ToolTier` (Bone<Stone<FineStone<Copper<Bronze) = *how well* via `work_speed_mult`. Tools live in `ToolKit` (`Vec<Item>`, `capacity_for_era`), separate from `Carrier` hands; spawned on every Person. **Hard gates** (no `ToolKit` reads as satisfied; empty kit blocks): Axe→fell trees (no-Axe → deadwood, tree kept), Pick→mine Stone/Wall/Ore + dig stone-like floors (no-Pick → trickle), Sickle→reap mature Grain, Fishing Kit→fish, recipe `tool_requirements`→craft. `seed_starting_tools_system` (OnEnter) pre-stows era loadouts. Acquisition: `Task::WithdrawTool`→`StowToolKit`. Faction demand: `compute_faction_tool_deficits` feeds chief craft postings (generic `tools` commodity remains only as Ard-Plow ingredient).

## Constraints

- **ECS:** logic in Systems; Components hold data only. No OO inheritance.
- **UI:** `bevy_egui` for panels (avoid `bevy_ui` except specific overlays).
- **Hashing/randomness:** `ahash::AHashMap` (not `std::HashMap`). `fastrand` in hot paths, `rand` for init.
- **No new crates** without explicit permission.
- **Error handling:** avoid `unwrap()` in core systems; use `match` / `if let`.
- **Mutable aliasing:** be careful with Bevy query aliasing; test empirically.
- **`PersonAI.state` is encapsulated:** never assign it directly — use `ActionQueue` transition methods (`begin_working` / `begin_seeking` / `begin_routing` / `begin_sleeping` / `begin_attacking` / `finish_task` / `cancel_chain`). Direct `ai.state = AiState::X` writes outside `src/simulation/` are a compile error. See `src/simulation/CLAUDE.md` → "ActionQueue and typed Task variants".
- **Doc updates:** when behaviour changes, update the matching `CLAUDE.md`. Subsystem-local changes go in `src/<dir>/CLAUDE.md`; cross-cutting in this file. Keep entries terse.
