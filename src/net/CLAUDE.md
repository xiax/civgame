# Networking (`src/net/`)

LAN multiplayer, network IDs, the server-authoritative boundary, and the custom snapshot protocol. Built on Lightyear. See root `CLAUDE.md` for the simulation-set authority gating summary.

## Network IDs (`net_id.rs`)

`NetId(u32)` is the stable, serializable handle for any entity that can cross a network boundary or be referenced from a player command. `auto_tag_replicable<T>` observers (`NetIdPlugin`, PostUpdate, before `assign_net_ids_system`) auto-insert `NeedsNetId` on every freshly-added `Person / Vehicle / GroundItem / Plant / Corpse / Blueprint / Bed / Door / Wall / Workbench / Campfire / Settlement / Camp` (plus `EdgeWallVisual`/`EdgeDoorVisual`). `assign_net_ids_system` swaps it for `Networked(NetId)` and registers in `NetIdMap` (bidirectional `Entity ↔ NetId`). IDs monotonic per session, **never reused**. Server resolves `NetId → Entity` via `NetIdMap.entity_of`; client populates a separate map from replication.

`PlayerCommand` entity-target variants (`PickUpItem`/`PickUpCorpse`/`AttackEntity`/`Teach`/`MilitaryAttack`/`VehicleOrder`) carry `NetId`, not raw `Entity`. UI builds commands through the `CommandSender` SystemParam — its `net_id_for(entity)` calls `NetIdMap::lookup_or_alloc`. Dispatch / lifecycle resolve back to `Entity`; unresolvable id → `CommandFailure::TargetGone`.

## Network boundary

`NetPlugin` registers `NetMode` (`Local` default; `ListenServer`/`DedicatedServer`/`Client` from CLI) and the `NetPlayerCommandEvent { sender_faction_id, actors, command }` channel. `command_loopback_system` (`PreUpdate`, runs in every mode except `Client`) validates `sender_faction_id` against `ControlledFactions` and re-emits as `PlayerCommandEvent`. Every UI command flows through this even in single-player so the server-auth path never atrophies. `CommandSender` writes `NetPlayerCommandEvent` and fills `sender_faction_id` from `Res<PlayerFaction>`; faction-level variants also carry `faction_id` in their payload. Sim-internal sites (executors, tests) write `PlayerCommandEvent` directly.

**Authority gating.** `SimulationSet::{Input, ParallelA, ParallelB, Sequential, Economy}` + `EconomyPlugin` + `PathfindingPlugin` mutating systems gate on `net_mode_runs_sim` (`Option<Res<NetMode>>` — missing reads as `runs_sim = true` so headless test fixtures keep ticking).

**CLI (`cli.rs`):** `NetConfig { mode, bind_addr, connect_addr, player_name, on_disconnect }`. `--listen`/`--server` + `--bind` → `ListenServer`/`DedicatedServer`; `--connect` + optional `--player NAME` → `Client`; bare `cargo run` stays `Local`. `--on-disconnect=ai-takeover|pause|drop-faction`.

**Lightyear install per `NetMode`:**
- `Local`: loopback only.
- `DedicatedServer`: `ServerPlugins` (UDP `--bind` + `NetcodeConfig` with shared `DEV_NETCODE_KEY` and `NETCODE_PROTOCOL_ID = PROTOCOL_VERSION as u64`) + `ProtocolPlugin` + server systems. Switches to `MinimalPlugins + LogPlugin` (no window/rendering).
- `ListenServer`: Dedicated + `ClientPlugins` (`NetConfig::Local { id: HOST_SERVER_LOCAL_CLIENT_ID=1 }`) so the host plays through the same path as remotes.
- `Client`: `ClientPlugins` (Netcode over UDP) + `ProtocolPlugin` + client systems. Client id derived deterministically from `--player NAME` via `derive_client_id(name)` using `ahash::RandomState::with_seeds(..)` with a fixed seed quad (NOT `AHasher::default()`, which is process-keyed and would break reconnect-by-name).

## Protocol (`PROTOCOL_VERSION = 10`, `protocol.rs`)

All serde-derived, ride `OrderedReliableChannel`. Bincode round-trip tests + pure-fn `snapshot_*_map` / `apply_*_snapshot` helpers in `snapshot.rs`.
- **Bootstrap:** `ClientHello`, `FactionAssignment { faction_id, world_seed }`, `BootstrapSnapshot { server_tick, calendar, factions, settlements, controlled_factions, overlay_tiles, interest_chunks, edge_walls, edge_doors }`.
- **Overlays:** `ChunkOverlayDelta { chunk, ops: Vec<TileOverlayOp> }` (idempotent). `TileOverlayOp::{AddPlant, RemovePlant, AddStructure, RemoveStructure, AddEdgeWall, RemoveEdgeWall, AddEdgeDoor, RemoveEdgeDoor, SetEdgeDoorOpen, SetRuntimeWater, …}` for Walls/Doors/Bridges/Dams/RuntimeWater/Plants/Structures/edge structures. `AddPlant` carries `species: u16` alongside legacy `kind` for catalog-less clients. Edge ops are keyed by `EdgeKey`; `push_ops_for_tile` emits a tile's canonical N+E edges.
- **Entity state:** `EntityStateDelta { server_tick, chunk, entries: Vec<EntityStateEntry { net_id, kind, tile, z, facing, health_current, health_max, faction_id }> }`; `EntityKindWire::{Person, Animal(species), Vehicle}`. `EntityRemoved { net_ids }`.
- **Commands:** `NetCommandFrame { command_id, sender_faction_id, actors, command }` + `NetCommandAck { command_id, status: Accepted/OwnershipRejected/AllActorsGone, reason }`. `CommandId(u32)` monotonic, LOCAL sentinel = 0.
- **Misc:** `ClientCameraFocus { tile }`, `InspectorSummaryRequest/Response`.

`PlayerCommand` and every sub-type (`BuildSiteKind`/`WallMaterial`, vehicle types, `VehicleOrderKind`, `MigrationIntent`, `PackedMigrationAutonomy`, `ResourceId`, `EdgeKey`) derive `Serialize`/`Deserialize`.

## Server (`server.rs`)

- `handle_client_hello_system` validates `protocol_version`, reclaims via `PendingReconnect.take(&player_name)` inside the `RECONNECT_GRACE_TICKS = 1200` (~60 s) window, else allocates via `allocate_free_faction`; ships `FactionAssignment` + `BootstrapSnapshot` either way.
- `replicate_tile_overlays_system` dedups `TileChangedEvent`, groups by chunk, ships per-recipient via interest filter. `emit_tile_changed_for_replicated_entities_system` fires `TileChangedEvent` on `Added<Plant>`/`Added<StructureLabel>` so spawns flow through the standard cadence.
- `replicate_entity_state_system` (every `ENTITY_REP_INTERVAL_UPDATES = 3` ≈ 50 ms) samples Networked Persons/Vehicles/animals, batches by chunk, tier-rate-limited (Owned 50 ms / Neighbour 100 ms / Far 500 ms via `tier_cadence`/`send_index`).
- `replicate_entity_removals_system` coalesces by recipient via `LastKnownChunkMap`. `receive_command_frames_system` validates ownership + acks. `expire_pending_reconnects_system` (1 Hz) GCs.
- `structure_kind_wire_from_label` buckets `StructureLabel` strings into `Bed/Workshop/Campfire/Storage/CivicAnchor`; `label_hash_u16` salts the label into a stable u16.

## Client (`client.rs`)

- `send_client_hello_system` on `ClientConnectEvent`. `apply_bootstrap_snapshot_system` drains `FactionAssignment` + `BootstrapSnapshot` (auto-spawns `Networked(NetId)` stubs via `ensure_stub` + `NetIdMap::bind`), sets `WorldSeed`, fires `RegenerateWorldRequest`, writes `PendingStarts.primary_start` from the assigned faction's `home_tile`, triggers `NextState(Playing)` — the canonical mid-game reconnect path. Also drains `ChunkOverlayDelta` (populating `ReplicatedPlantMap`/`ReplicatedStructureMap`/`EdgeStructureMap` + edge cache + visual stubs) + `NetCommandAck` (bounded `ClientAckLog` cap 32).
- `send_command_frames_system` reads `NetPlayerCommandEvent`, stamps `ClientCommandSequencer.next()` id, ships. `apply_entity_state_delta_system` auto-spawns stubs and upserts `Transform` + `ReplicatedEntity` + `ReplicatedEntityKind`. `send_camera_focus_system` ships `ClientCameraFocus` every 500 ms when chunk shifts.
- `rendering::fog::fog_update_system` runs the same per-agent LOS sweep over `ReplicatedEntityKind::Person` stubs; walls stamped with `owner_faction` so `has_vision_los` reads the same owner check on both sides.

## Interest rooms

`ConnectionState.interest_chunks: AHashMap<(i32, i32), InterestTier>`. `compute_interest_system` (every `INTEREST_REBUILD_INTERVAL_UPDATES = 30` ≈ 500 ms) rebuilds anchors per client from faction home + every owned `Settlement::market_tile` + camera focus chunk + active-military positions (`raid_party`, `HuntOrder::Hunt.mustered`, `Drafted` Persons). Tiers: `Owned` ±1, `Neighbour` ±`NEIGHBOUR_TIER_RADIUS = 2`, `Far` fills `INTEREST_RADIUS_CHUNKS = 4`.

## Disconnect policy + speed lock + diagnostics

`DisconnectPolicy::{AiTakeover (default), Pause, DropFaction}`. AiTakeover/DropFaction remove from `ControlledFactions`; Pause flips `Time<Virtual>::pause()`. `ConnectedRemotes` + `speed_lock_system` clamps speed to 1.0 while any remote connected; `apply_disconnect_policy_system` decrements `ConnectedRemotes` so the lock releases on last leave. `ReplicationStats` (raw counters + `*_per_sec` snapshots) reported by `report_replication_stats_system` (every `STATS_REPORT_INTERVAL_UPDATES = 60` ≈ 1 s) under `RUST_LOG=civgame::net=debug`; bytes via `bincode::serialized_size`.

**Why custom snapshot over Lightyear `Replicate`:** mirrors the `ChunkOverlayDelta` shape, avoids 20+ component registrations, keeps interest-gating + rate-tiering one cadence constant away.

## LAN lobby (`lan.rs`, `lobby_state.rs`, `../ui/{main_menu,lobby}.rs`)

**Topology:** re-launch the binary on Host/Join. `ui::main_menu::relaunch_as_host()` spawns the parent exe with `--listen --bind 0.0.0.0:5000 --player NAME` and `exit(0)`s the menu — Lightyear consumes its NetConfig at install time, no live transport swap.

**Discovery:** UDP broadcast on port 5001 (game port is 5000). `LanAdvert { protocol_version, game_name, host_name, game_port, players, max_players, phase, world_seed }` bincode-serialised (< 512 B); `BROADCAST_INTERVAL = 1s`, `LISTEN_TTL = 3s`. Listener thread spawned from `NetPlugin::build` for every non-Dedicated mode; single-machine tests see loopback via `255.255.255.255`.

**Lobby protocol:** Client→Server `LobbyJoin/SelectStart/SetReady/Leave`; Server→Client `LobbySnapshot/Reject/StartGame` on `OrderedReliableChannel`. Pure validators (`is_start_megachunk_acceptable` for `MIN_HUMAN_MEGACHUNK_DISTANCE = 3`; `lobby_ready_to_start`; `LobbyState::is_select_acceptable`) unit-tested without an App. `LobbyState { phase: Hosting → SelectingStarts → Starting → InGame, config, slots, version }` with reclaim-by-name `accepts_join`; `bump()` auto-advances `SelectingStarts → Starting` when every slot has `megachunk.is_some() && ready`.

**Wiring:** `server::handle_lobby_{join,select_start,set_ready,leave}_system` drain the wire + `LocalLobbyCommand` (host UI bypass) channels. `server::broadcast_lobby_snapshot_system` ships `LobbySnapshot` whenever `lobby.version` bumps past `last_sent_version`. `server::start_game_transition_system` fires on `phase == Starting`: allocates a monotonic `faction_id` per slot (sorted by `slot_id`), broadcasts `LobbyStartGame`, populates `PendingStarts.{slots, primary_start}` + `GameStartOptions` from `lobby.config`, transitions `GameState::Playing`, phase → `InGame`. Client: `client::apply_lobby_snapshot_system` mirrors snapshots onto `LobbyUiState.remote_slots` + `WorldSeed/GameStartOptions`; `client::apply_lobby_start_game_system` builds `PendingStarts` from `slot_assignments` and flips into `Playing`. Lobby UI: `ui::lobby::LobbyCommandChannels` SystemParam routes per `NetMode` (host writes `LocalLobbyCommand` events, remote ships via `OrderedReliableChannel`); `auto_join_lobby_on_enter` (OnEnter MultiplayerLobby) announces the local player.
