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

**Incremental excavation (`simulation::excavation`):** stone/ore Mine and Dig Down advance through 7 levels (`EXCAVATION_LEVEL_MAX`). Levels 1-6 slow traversal (0.92×..0.52×) and grant ranged cover (5%/level, capped 30%); level 7 fires `carve::finalize_carved_tile` + `TileCarvedEvent`. Bare hands cap at `HAND_DEPTH_LIMIT=3` on stone-like tiles. Durable state in `ExcavationMap` Resource; cached as 3 bits (4-6) in `TileData.flags`. See `src/simulation/CLAUDE.md` → Incremental excavation, and `plans/incremental-mining.md`.

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

## Network IDs (`net_id.rs`)

`NetId(u32)` is the stable, serializable handle for any entity that can ever cross a network boundary or be referenced from a player command. Add `NeedsNetId` at spawn for replicable kinds; `assign_net_ids_system` (PostUpdate, chained before `release_net_ids_on_despawn`) swaps it for `Networked(NetId)` and registers in `NetIdMap` (bidirectional `Entity ↔ NetId`). IDs are monotonic per session and **never reused** — `RemovedComponents<Networked>` frees the mapping but doesn't recycle the id. Server resolves `NetId → Entity` via `NetIdMap.entity_of`; client populates a separate map from replication.

**`PlayerCommand` entity-target variants** (`PickUpItem`/`PickUpCorpse`/`AttackEntity`/`Teach`/`MilitaryAttack`/`VehicleOrder`) carry `NetId`, not raw `Entity`, so the event type is shape-serializable. UI sites build commands through the `CommandSender` SystemParam (`simulation/player_command.rs`) — its `net_id_for(entity)` calls `NetIdMap::lookup_or_alloc` to fold in any entity that wasn't tagged with `NeedsNetId` at spawn. Dispatch (`CommandRouting.net_ids`) and lifecycle (`net_ids: Res<NetIdMap>`) resolve back to `Entity` at the top of every affected arm; unresolvable id → `CommandFailure::TargetGone`. Foundation for `plans/multiplayer.md`.

## Network boundary (`src/net/`)

`NetPlugin` registers `NetMode` resource (`Local` default; `ListenServer`/`DedicatedServer`/`Client` resolved from CLI) and the `NetPlayerCommandEvent { sender_faction_id, actors, command }` channel. `command_loopback_system` (`PreUpdate`) is the network-boundary validator: it drains `NetPlayerCommandEvent`, checks `sender_faction_id` against `ControlledFactions` (`simulation/faction.rs`), and re-emits as `PlayerCommandEvent` for the sim's existing drain. Every UI command flows through this channel even in single-player so the server-auth path never atrophies. `CommandSender` writes `NetPlayerCommandEvent` and fills `sender_faction_id` from `Res<PlayerFaction>`; faction-level `PlayerCommand` variants (`EncodeTablet`, `QueueVehicle`, `QueueCustomVehicle`, `VehicleOrder`, `DebugSpawnTestVehicle`) additionally carry `faction_id: u32` in their payload so the drain validates without consulting `PlayerFaction`. `lightyear = "0.19"` is a no-default-features dependency for Phase 2 transport; not wired yet. `src/net/snapshot.rs` provides pure-fn `snapshot_*_map` / `apply_*_snapshot` helpers for `WallMap`/`DoorMap`/`BridgeMap`/`DamMap`/`RuntimeWater` keyed by `NetId` — consumed by Phase 2 bootstrap snapshot. Sim-internal call sites (executors, tests) continue to write `PlayerCommandEvent` directly — only UI / future network connections cross the boundary.

`src/net/cli.rs` parses argv into `NetConfig { mode, bind_addr, connect_addr, player_name, on_disconnect }`: `--listen`/`--server` + `--bind host:port` pick `ListenServer`/`DedicatedServer`; `--connect host:port` (+ optional `--player NAME`) picks `Client`; bare `cargo run` stays `Local`. `--on-disconnect=ai-takeover|pause|drop-faction` overrides the default policy. `main.rs` inserts the resolved mode + `DisconnectPolicy` resource before `NetPlugin::build` so CLI choices win. `DedicatedServer` mode switches the App from `DefaultPlugins` to `MinimalPlugins + LogPlugin` and skips `RenderingPlugin` + `UiPlugin` (no window, no winit, no rendering).

`PlayerCommand` and its sub-types (`BuildSiteKind`/`WallMaterial`, `VehicleGrid`/`VehicleCell`/`VehicleModuleInstance`/`VehiclePartVariantId`/`VehicleModuleId`/`VehicleModuleDefId`, `VehicleOrderKind`, `MigrationIntent`, `PackedMigrationAutonomy`, `ResourceId`) all derive `Serialize`/`Deserialize` so `NetPlayerCommandEvent.command` can ride a Lightyear reliable channel without an intermediate wire-form translation. `NetPlayerCommandEvent.actors: Vec<NetId>` — `CommandSender::send` translates entities → NetIds at the boundary; `command_loopback_system` resolves NetIds → entities via `NetIdMap::entity_of` when re-emitting `PlayerCommandEvent`, dropping unresolvable ids silently. Bincode round-trip tests in `net/protocol.rs`.

### Phase 2 — Lightyear plugins, protocol, server/client systems

**Protocol surface (`net/protocol.rs`).** `PROTOCOL_VERSION` + serde-derived `ClientHello`, `FactionAssignment { faction_id, world_seed }`, `BootstrapSnapshot { server_tick, calendar: CalendarWire, factions: Vec<FactionSummary>, settlements: Vec<SettlementSummary>, controlled_factions, overlay_tiles: OverlayTileSnapshot, interest_chunks }`, `ChunkOverlayDelta { chunk, ops: Vec<TileOverlayOp> }`, `TileOverlayOp::{AddWall/RemoveWall/AddDoor/RemoveDoor/SetDoorOpen/AddBridge/RemoveBridge/AddDam/RemoveDam/SetRuntimeWater/ClearRuntimeWater}`, `NetCommandFrame { command_id, sender_faction_id, actors, command }`, `NetCommandAck { command_id, status, reason }` with `NetCommandAckStatus::{Accepted/OwnershipRejected/AllActorsGone}`, monotonic `CommandId(u32)` (LOCAL sentinel = 0). `world::water_runtime::RuntimeWaterCell` carries `Serialize/Deserialize`. 8 bincode round-trip tests.

**Bootstrap helpers (`net/bootstrap.rs`).** `build_bootstrap_snapshot(server_tick, controlled, calendar, factions, settlement_map, settlement_q, wall_map, door_map, bridge_map, dam_map, runtime_water, networked_q) → BootstrapSnapshot` packs current state. `apply_bootstrap_snapshot(snap, &mut Calendar, &mut ControlledFactions, &mut WallMap/DoorMap/BridgeMap/DamMap/RuntimeWater, &NetIdMap)` rebuilds on client. `compute_interest_chunks(controlled, factions, INTEREST_RADIUS_CHUNKS=4)` enumerates initial-replication chunks deduplicated + sorted. `tile_to_chunk_coord(tile)` pure integer math; `Season::from_index(u8)` inverse for `CalendarWire`. Caps: `MAX_FACTIONS_IN_BOOTSTRAP=64`, `MAX_SETTLEMENTS_IN_BOOTSTRAP=128`. 5 unit tests.

**NetPlugin install per NetMode.** `NetPlugin.build()` branches on `NetMode`:
- `Local`: loopback only (no Lightyear runtime).
- `DedicatedServer`: `ServerPlugins` (`ServerTransport::UdpSocket(--bind)` + `NetcodeConfig` with `NETCODE_PROTOCOL_ID = PROTOCOL_VERSION as u64`) + `ProtocolPlugin` + server systems + `start_server` Startup.
- `ListenServer`: same as Dedicated **plus** `ClientPlugins` with `NetConfig::Local { id: HOST_SERVER_LOCAL_CLIENT_ID=1 }` — host plays through the same Lightyear path as remotes (single codepath). Client systems also installed.
- `Client`: `ClientPlugins` (`NetConfig::Netcode { server_addr: --connect, ... }` over `ClientTransport::UdpSocket("0.0.0.0:0")`) + `ProtocolPlugin` + client systems + `connect_client` Startup.

`ProtocolPlugin` (`net/protocol_plugin.rs`) registers `OrderedReliableChannel` (`ChannelMode::OrderedReliable`) and every wire message with the appropriate `ChannelDirection`. NET tick interval is `NET_TICK_INTERVAL = 50ms` (matches FixedUpdate 20 Hz).

**Server systems (`net/server.rs`).** `accept_connections_system` drains `ServerConnectEvent`, allocates lowest unowned materialised non-household faction via `allocate_free_faction`, registers in `ServerConnections::by_client` + adds to `ControlledFactions`, ships `FactionAssignment` then `BootstrapSnapshot` on `OrderedReliableChannel` to that single client. `record_disconnections_system` drains `ServerDisconnectEvent` into `PendingDisconnects` for the policy stage. `replicate_tile_overlays_system` dedups `TileChangedEvent` (`.tx/.ty`), groups by chunk via `tile_to_chunk_coord`, emits `ChunkOverlayDelta` to `NetworkTarget::All` with idempotent Add/Remove ops for each affected map plus Set/Clear runtime water. `receive_command_frames_system` drains `ServerReceiveMessage<NetCommandFrame>`, validates `sender_faction_id` against `ServerConnections.faction_for(client_id)`, re-emits `NetPlayerCommandEvent` (the loopback validator then dispatches), sends `NetCommandAck::Accepted` or `OwnershipRejected`. v1 broadcasts overlay deltas to all clients — interest rooms are Phase 3.

**Client systems (`net/client.rs`).** `send_client_hello_system` ships `ClientHello { PROTOCOL_VERSION, player_name }` on `ClientConnectEvent`. `apply_bootstrap_snapshot_system` drains `FactionAssignment` (sets `PlayerFaction.faction_id`, adds to `ControlledFactions`), `BootstrapSnapshot` (auto-spawns `Networked(NetId)` stubs via `ensure_stub` + `NetIdMap::bind` for every overlay entity, then `apply_bootstrap_snapshot` rebuilds calendar/maps), `ChunkOverlayDelta` (per-op application with stub auto-spawn), `NetCommandAck` (folded into bounded `ClientAckLog`, cap 32). `send_command_frames_system` reads `NetPlayerCommandEvent` (which the client App's `CommandSender` also writes — same loopback path), stamps `ClientCommandSequencer.next()` id, wraps in `NetCommandFrame`, ships via `ConnectionManager::send_message`. `observe_disconnect_system` logs `ClientDisconnectEvent`.

**Disconnect policy + speed lock.** `DisconnectPolicy::{AiTakeover (default), Pause, DropFaction}` Resource. `apply_disconnect_policy_system` drains `PendingDisconnects` and applies per policy — AiTakeover/DropFaction remove faction from `ControlledFactions` (chief AI reclaims agency); Pause flips `Time<Virtual>::pause()`. `ConnectedRemotes { count }` + `speed_lock_system` clamps `Time<Virtual>::relative_speed → 1.0` and unpauses when any remote client connected. Both use `Option<ResMut<Time<Virtual>>>` so headless test fixtures don't panic.

**`NetIdMap::bind(entity, server_chosen_id)`** adopts a server-allocated NetId for a client-spawned stub and bumps `next` past it to prevent collision with future client-local allocations. Consumed by `client::apply_bootstrap_snapshot_system::ensure_stub`.

**Phase 3a — entity-state replication (min, shipped).** `PROTOCOL_VERSION` is now `2`. Custom `EntityStateDelta { server_tick, chunk, entries: Vec<EntityStateEntry> }` shipped on `OrderedReliableChannel` Server→Client; `EntityKindWire { Person, Animal(AnimalSpeciesWire), Vehicle }`; `EntityStateEntry { net_id, kind, tile, z, facing, health_current: u16, health_max: u16, faction_id }`. Server `replicate_entity_state_system` (Update, every `ENTITY_REP_INTERVAL_UPDATES = 3` ≈ 50 ms) samples every `Networked` Person/Vehicle and each animal species (Wolf/Deer/Horse/Cow/Pig/Cat) via parallel read-only queries, batches by `tile_to_chunk_coord`. Vehicle health totals from `VehicleHealth.cells`; vehicle heading collapses to a cardinal `facing` byte. Client `apply_entity_state_delta_system` drains messages, auto-spawns `Networked` stubs via `ensure_replicated_stub`, upserts `Transform` + `ReplicatedEntity` bookkeeping (`tile`, `z`, `facing`, `health_*`, `faction_id`, `last_tick`) + a `ReplicatedEntityKind` marker (`Person | Animal | Vehicle`); marker is component-side so adding a kind doesn't bump the protocol. `server::ReplicationStats { entity_deltas_sent, entity_entries_sent, entity_bytes_sent }` resource + 5 s `info!` line is the Phase 3g foothold (bytes via `bincode::serialized_size`). Custom-snapshot was picked over Lightyear `Replicate` for mirroring `ChunkOverlayDelta`, avoiding 20+ component registrations, and keeping interest-gating + rate-tiering one cadence constant away.

**Per-entity remove signal (shipped).** `EntityRemoved { net_ids: Vec<NetId> }` Server→Client message. `NetworkedRemovedEvent { net_id }` in `net_id.rs` populated by `release_net_ids_on_despawn` (PostUpdate) **before** the mapping is dropped — necessary so the NetId is still readable. Server `replicate_entity_removals_system` (Update) drains the event and broadcasts a coalesced `EntityRemoved`. Client `apply_entity_removed_system` looks up via `NetIdMap::entity_of` and `despawn_recursive`s the stub; unresolvable ids drop silently (no-op). Removes stay broadcast even after Phase 3b — they're tiny and a remove for a stub the client never spawned is just a missed lookup.

**Phase 3b — interest rooms (shipped).** `ConnectionState.interest_chunks: AHashMap<(i32, i32), InterestTier>` per connection. `compute_interest_system` (Update, every `INTEREST_REBUILD_INTERVAL_UPDATES = 30` ≈ 500 ms) rebuilds each client's frame from anchor chunks (faction home + every owned `Settlement::market_tile`): `Owned` ±1 ring, `Neighbour` ±`NEIGHBOUR_TIER_RADIUS = 2`, `Far` fills the rest of `INTEREST_RADIUS_CHUNKS = 4`. `ServerConnections::clients_interested_in(chunk) → Vec<ClientId>` is the tier-blind lookup used by tile-overlay replication. `replicate_tile_overlays_system` sends per-chunk via `NetworkTarget::Only(recipients)` (unwatched chunks skip the wire).

**Phase 3c — rate tiering (shipped).** `InterestTier { Owned, Neighbour, Far }` + `tier_cadence(tier) → 1 / 2 / 10`. `replicate_entity_state_system` walks a monotonic `send_index: Local<u32>` and uses `ServerConnections::clients_for_entity_chunk(chunk, send_index)` which filters per-client by `send_index % tier_cadence(tier) == 0` — Owned chunks ship every 50 ms, Neighbour every 100 ms, Far every 500 ms. Tile overlays don't tier (sparse + reliable). Phase 3 follow-ons: client-side periodic `ClientCameraFocus` message + active military groups should feed interest, so scouting expeditions outside the settlement ring don't degrade to `Far`-only replication.

**Interest-gated removes (shipped).** `LastKnownChunkMap` resource (`AHashMap<NetId, (i32, i32)>`) populated by `replicate_entity_state_system` on every send. `replicate_entity_removals_system` coalesces removed ids by recipient set — `client_id_sort_key(ClientId) → (u8, u64)` (`Netcode = 0 / Steam = 1 / Local = 2`) for stable `Vec<ClientId>` hashing — and ships `NetworkTarget::Only(recipients)` per group. NetIds with no recorded chunk (despawned before ever being replicated) fall back to broadcast. Map entry evicted on drain, so the table can't grow unbounded.

**Camera focus (shipped).** New `ClientCameraFocus { tile }` Client→Server message. Client `send_camera_focus_system` (Update, every `CAMERA_FOCUS_SEND_INTERVAL_UPDATES = 30` ≈ 500 ms) reads the `Camera2d` `Transform`, converts to a tile, and only ships when the **chunk** has shifted since the last successful send. Server `receive_camera_focus_system` stashes the focus chunk on `ConnectionState.camera_focus_chunk`; `compute_interest_system` folds it in alongside faction home + settlements as an `Owned`-tier anchor. Scouting expeditions outside the settlement ring now pull live replication where the camera is looking.

**Phase 3e — reconnect-restoration (shipped).** `accept_connections_system` reduced to a log line — faction allocation now waits for the corresponding `ClientHello` (which carries `player_name` — load-bearing for reconnect-by-name). New `handle_client_hello_system` drains `ServerReceiveMessage<ClientHello>`, validates `protocol_version`, and either reclaims via `PendingReconnect.take(&player_name)` (if still inside the grace window) or allocates a fresh faction via `allocate_free_faction`; both paths ship `FactionAssignment` + `BootstrapSnapshot` identically. `ConnectionState` carries `player_name`; `record_disconnections_system` stashes `PendingReconnect { faction_id, expires_tick = now + RECONNECT_GRACE_TICKS }` keyed on name. `RECONNECT_GRACE_TICKS = 1200` ≈ 60 s at 20 Hz `SimClock`. `expire_pending_reconnects_system` (Update, every 60 ticks ≈ 1 Hz) GCs expired entries so the table doesn't grow. Disconnect policy still runs in parallel — `AiTakeover`/`DropFaction` strip control immediately; a returning client inside the grace window can still reclaim.

**Active-military interest (shipped).** `compute_interest_system` walks `faction.raid_party: Vec<Entity>` (via a `Query<&Transform>`) and folds each member's current chunk into the anchor set at **Neighbour** tier — a marching war band stays visible to the controlling player without burning the Owned-tier budget reserved for the settlement ring. Raid parties cap at `RAID_MAX_PARTY_ABS = 8`, so the per-rebuild cost stays trivial. Hunt parties + drafted defenders deferred (sim doesn't expose a single Vec the way `raid_party` does).

**Phase 3g — bandwidth diagnostics (shipped).** `ReplicationStats` extended with per-channel raw accumulators (`entity_deltas_sent` / `entity_entries_sent` / `entity_bytes_sent` / `tile_overlay_deltas_sent` / `tile_overlay_bytes_sent` / `entity_removed_msgs_sent` / `entity_removed_bytes_sent`) plus matching `*_per_sec` snapshot fields. `report_replication_stats_system` (Update, every `STATS_REPORT_INTERVAL_UPDATES = 60` ≈ 1 s) divides accumulators by the 1-s window, populates `*_per_sec`, emits a single `debug!` line (`net rep/s: …`) for `RUST_LOG=civgame::net=debug` follow-along, and zeros accumulators. Tile-overlay + entity-removed replicators measure bytes via `bincode::serialized_size`. Follows the project's existing Resource-based diagnostic convention (cf. `BackgroundWorkDiagnostics` / `PathfindingDiagnostics`) rather than pulling in `bevy_diagnostic` — keeps the diagnostic surface consistent across subsystems.

**Hunt + drafted defender interest (shipped).** `compute_interest_system` folds `HuntOrder::Hunt.mustered` and any `Drafted` Person (`Query<(&Transform, &FactionMember), With<Drafted>>`) into the Neighbour-tier military anchor set alongside `raid_party`. Same rationale as raid parties — roving cohorts shouldn't burn the Owned-tier budget reserved for the settlement ring but must stay live for the controlling player.

**Phase 3d — client FogMap recompute (shipped).** `PROTOCOL_VERSION = 3`. `WireWallEntry` + `TileOverlayOp::AddWall` carry `owner_faction: Option<u32>`; client `ensure_wall_stub` stamps a faux `Wall { material: Palisade (placeholder), owner_faction }` on every wall stub so `has_vision_los` reads the same owner check on both sides of the wire. `rendering::fog::fog_update_system` gains a `Query<&ReplicatedEntity>` and runs the same per-agent LOS sweep over every player-faction `ReplicatedEntityKind::Person` stub (treated as Full LOD; active-lookout state not yet replicated, so client agents use standard radius). Material on the stub is a placeholder — fog reads `owner_faction` only and wall destruction stays server-side, so the fake material never surfaces. Bootstrap snapshot writes the owner via `build_bootstrap_snapshot(..., wall_q)`; `BootstrapParams` SystemParam bundles the read-only overlay queries so `handle_client_hello_system` stays under Bevy's 16-param ceiling.

**Phase 3f — per-client netcode tokens (shipped).** Shared `DEV_NETCODE_KEY: [u8; 32]` used by both server `NetcodeConfig` and client `Authentication::Manual` — previously the server used `generate_key()` (random per startup) while the client passed `[0; 32]`, so no remote could actually connect. Client `client_id` derived deterministically from `--player NAME` via `derive_client_id(name)` — `ahash::RandomState::with_seeds(..)` with a fixed seed quad, NOT `AHasher::default()` (which keys off process-local randomness and would re-roll the id every restart, breaking reconnect-by-name). Reserved `0` and `HOST_SERVER_LOCAL_CLIENT_ID` are bumped so a misconfigured empty name can't impersonate the host slot. Two clients sharing `--player NAME` collide on `client_id`; the netcode runtime rejects the second as `AlreadyConnected`. Production deployments should mint per-deployment `ConnectToken`s out-of-band; manual-handshake is the LAN/dev/playtest foothold.

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
