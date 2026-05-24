# Server-Authoritative Multiplayer For Player Factions

## Context

Add internet multiplayer as a **server-authoritative simulation**: one server runs the full CivGame ECS sim, each client controls one assigned `FactionData`, and clients receive only the world state their faction should observe. Direction matches what we want (server-auth + interest-managed replication scales for thousands of agents), but a naive feature-gated split between offline and online creates a two-pipeline drift risk: solo play would never exercise the server path. This plan flips that: **the server is always on, even in single-player**, via Lightyear's local (channels) transport. Every command takes the same path whether the player is solo or remote.

## Architecture: always-on server

`NetMode` resource selected from CLI:
- `Local` — default (`cargo run`). Same process runs Lightyear server + client over local channel transport. UI sends `NetPlayerCommand` → channel → server validates ownership against `ControlledFactions` → emits `PlayerCommandEvent`. One code path.
- `ListenServer` — `Local` + binds a UDP socket for remote clients.
- `DedicatedServer` — server only; skips `RenderingPlugin`/`UiPlugin`.
- `Client` — connect-only; no sim, only render/UI/interpolation, bootstraps from snapshot.

Lightyear is **always a dependency** (no `multiplayer` feature flag). Cost is justified by eliminating two-path drift.

### Sim cost in MP

Agent count does **not** scale with player count. The world already has N factions in single-player (player's + AI rivals). In MP, K of those N are player-driven instead of chief-AI; populations are the same. Per remote client, MP adds: O(visible-chunks) interest computation + replication send (target <200 KB/s/client). In `Local` mode, one client App's render/UI runs alongside server — that's the real always-on cost. If too expensive, fallback skips Lightyear transport hop while still serializing/validating.

Bigger worlds (e.g. 4 players × dedicated regions) is a content-scaling choice, not architectural.

## Library: Lightyear 0.19.x

Per lib.rs compatibility table: Lightyear 0.18–0.19 → Bevy 0.15. Has rooms-based interest, component replication, channels, messages, custom transports (UDP/QUIC/WebTransport), **local channel transport** for in-process server. Prediction opt-in (we leave off in v1). Pin `lightyear = "0.19.1"`, fall back to `0.19.0` if needed. Verify `cargo add lightyear@0.19` builds before committing.

`bevy_replicon` would fit our problem cleanly but tracks Bevy 0.18/0.19 — revisit if/when we bump Bevy.

**Determinism is out of scope.** Unseeded `fastrand` is fine for server-auth — only the server rolls dice. Audit only if we want spectator replay or rollback prediction.

## Progress

- **Phase 1a shipped** — `src/net_id.rs` with `NetId(u32)` / `NetIdMap` / `Networked` / `NeedsNetId`, `assign_net_ids_system` + `release_net_ids_on_despawn` in PostUpdate. `NetIdPlugin` wired into `main.rs` + `test_fixture.rs`. 4 unit tests.
- **Phase 1b shipped** — 6 `PlayerCommand` variants (`PickUpItem`/`PickUpCorpse`/`AttackEntity`/`Teach`/`MilitaryAttack`/`VehicleOrder`) carry `NetId` instead of `Entity`. `CommandRouting.net_ids` + `Res<NetIdMap>` in lifecycle resolve back to `Entity` at dispatch time. `CommandSender` SystemParam (in `player_command.rs`) wraps EventWriter + NetIdMap + Commands; UI sites in `ui/orders.rs` (right-click menu, military right-click, `emit_military_attack`) use `sender.net_id_for(entity)`. `DebugTestDriveResources` SystemParam bundles drain's debug-test-drive params to stay under Bevy's 16-param ceiling. All 1100 tests green.
- **Phase 1c shipped** — `ControlledFactions { ids: Vec<u32> }` resource in `simulation/faction.rs`, seeded from the player faction id at `spawn_population` + in `TestSim::new`. Faction-level `PlayerCommand` variants (`EncodeTablet`, `QueueVehicle`, `QueueCustomVehicle`, `VehicleOrder`, `DebugSpawnTestVehicle`) carry `faction_id: u32` in their payload; `drain_player_command_events_system` reads the payload and validates against `ControlledFactions`, dropping commands whose target faction isn't controlled here (sets up Phase 2's per-connection ownership enforcement). UI fills payloads from `PlayerFaction`.
- **Phase 1d shipped** — `lightyear = "0.19"` added as a no-default-features dependency (Phase 2 foothold). New `src/net/{mod, protocol, snapshot}.rs`. `NetMode` resource (default `Local`), `NetPlayerCommandEvent { sender_faction_id, actors, command }`, and `command_loopback_system` (`PreUpdate`) that validates against `ControlledFactions` and re-emits as the sim's existing `PlayerCommandEvent`. `CommandSender` now writes `NetPlayerCommandEvent` so every UI command crosses the network-boundary event channel even in single-player — the load-bearing one-path property. 2 loopback unit tests (pass-through + drop).
- **Phase 1e shipped** — `src/net/snapshot.rs` carries pure-fn `snapshot_*_map(&map, &Query<&Networked>) → Vec<*SnapshotEntry>` + `apply_*_snapshot(&mut map, &snap, &NetIdMap)` for `WallMap`, `DoorMap`, `BridgeMap`, `DamMap`, plus `snapshot_runtime_water` / `apply_runtime_water_snapshot` (tile-keyed, no entity refs). Door entry preserves `open` + `faction_id`; runtime-water entry preserves full `RuntimeWaterCell`. Unmapped `NetId`s on the apply side are dropped (client-stub-not-yet-materialised case). 4 unit tests.
- **Phase 1f shipped** — `ui/{hud, migration_panel, vehicle_designer, inspector, orders}.rs` all emit via `CommandSender` (no remaining `EventWriter<PlayerCommandEvent>` in `src/ui/`). `CommandSender` SystemParam now bundles `EventWriter<NetPlayerCommandEvent>` + `NetIdMap` + `Commands` + `Res<PlayerFaction>` so call sites still hand back a `NetId` via `sender.net_id_for(entity)` and the sender-faction id is filled in at the boundary, not at every site.
- **Phase 1 complete: 1106 tests green.**
- **Phase 2a shipped** — `src/net/cli.rs` hand-rolled parser fills `NetConfig { mode, bind_addr, connect_addr, player_name }` from argv. `--listen`/`--server`/`--connect`/`--bind`/`--player` recognised; unknown flags (e.g. `--sandbox`) pass through; `--listen`/`--server` require `--bind host:port`; conflicting roles or malformed sockets reject with non-zero exit. `main.rs` inserts the resolved `NetMode` *before* `NetPlugin::init_resource` so the CLI choice wins. Window title reflects mode (`[listen]` / `[server]` / `[client → host:port]`). 14 unit tests.
- **Phase 2-prep — serde on `PlayerCommand` shipped** — derived `Serialize`/`Deserialize` on `PlayerCommand` and every non-trivial sub-type it carries: `BuildSiteKind`, `WallMaterial`, `VehicleGrid`, `VehicleCell`, `VehicleModuleInstance`, `VehiclePartVariantId`/`VehicleModuleId`/`VehicleModuleDefId`, `VehicleOrderKind`, `MigrationIntent`, `PackedMigrationAutonomy`, and `ResourceId`. `NetId` and `VehiclePartKind`/`VehiclePurpose` already had it. Bevy's `serialize` feature is already enabled transitively (verified `glam feature "serde"` in `cargo tree`), so `IVec3` rides for free. 7 bincode round-trip tests in `src/net/protocol.rs` exercise Move / NetId-target / `Build(Wall(Mudbrick))` / freeform `QueueCustomVehicle` grid+module / `VehicleOrder(SiegeWall)` / `SetMigrationIntent` / `SetPackedAutonomy`.
- **Phase 2c-prep — headless plugin topology shipped** — `main.rs` branches on `NetMode::DedicatedServer`: that mode adds `MinimalPlugins + LogPlugin` (no window, no winit event loop, no rendering) and skips `RenderingPlugin` + `UiPlugin`; every other mode keeps `DefaultPlugins` and the full client topology. Runtime not yet validated end-to-end (no `--server` smoke run); the gate is plumbed so Phase 2c can add the Lightyear transport layer without touching `main.rs` again.
- **Phase 2-prep — wire-side actors are `Vec<NetId>` shipped** — `NetPlayerCommandEvent.actors` is `Vec<NetId>` (was `Vec<Entity>`), so the event is fully shape-serializable. `CommandSender::send` translates entities → NetIds at the boundary via `NetIdMap::lookup_or_alloc` (the same fold-in path the entity-target variants already used). `command_loopback_system` resolves NetIds → entities via `NetIdMap::entity_of`; unresolvable ids drop silently from the actor list, mirroring how entity-target variants surface `CommandFailure::TargetGone`. Plus one unit test: `loopback_resolves_netid_actors_to_entities` round-trips a live NetId + a phantom NetId through the validator and asserts only the live entity survives.
- **Lightyear topology open question — answered.** Lightyear 0.19 supports same-`App` host-server mode: install both `ServerPlugins` and `ClientPlugins` in the same `App`, and the co-located client uses `NetConfig::Local { id }` (no socket). External clients use `NetConfig::Netcode { ... }`. Pattern is documented in `lightyear-0.19.1/src/tests/host_server_stepper.rs`. No sub-Apps needed; `src/net/mod.rs` hosts both plugin groups conditionally on `NetMode`.
- **Phase 2b shipped — full wire protocol.** `src/net/protocol.rs` now carries `PROTOCOL_VERSION`, `ClientHello`, `FactionAssignment`, `BootstrapSnapshot` (with `CalendarWire`, `FactionSummary`, `SettlementSummary`, `OverlayTileSnapshot`), `ChunkOverlayDelta` + `TileOverlayOp` (Add/Remove Wall/Door/Bridge/Dam + SetDoorOpen + SetRuntimeWater/ClearRuntimeWater), `NetCommandAck` + `NetCommandAckStatus`, `NetCommandFrame` (client→server command + monotonic `CommandId`). All serde, with bincode round-trip tests. `world::water_runtime::RuntimeWaterCell` got `Serialize/Deserialize` so it rides directly. 8 new round-trip tests.
- **Phase 2b shipped — bootstrap helpers.** `src/net/bootstrap.rs` with `build_bootstrap_snapshot(server_tick, controlled, calendar, factions, settlement_map, settlement_q, wall_map, door_map, bridge_map, dam_map, runtime_water, networked_q) → BootstrapSnapshot` (server) + `apply_bootstrap_snapshot(snap, &mut Calendar, &mut ControlledFactions, &mut WallMap/DoorMap/BridgeMap/DamMap/RuntimeWater, &NetIdMap)` (client). `compute_interest_chunks(controlled, factions, radius)` enumerates initial-replication chunk set. `tile_to_chunk_coord` pure helper. `Season::from_index(u8)` inverse for `CalendarWire`. 5 unit tests. `MAX_FACTIONS_IN_BOOTSTRAP = 64`, `MAX_SETTLEMENTS_IN_BOOTSTRAP = 128`, `INTEREST_RADIUS_CHUNKS = 4`.
- **Phase 2c shipped — Lightyear plugin install per NetMode.** `NetPlugin` now branches on `NetMode` and installs the relevant Lightyear plugin groups before adding the protocol + server/client systems. `Local`: loopback only. `DedicatedServer`: `ServerPlugins` (UDP socket from `--bind`) + `ProtocolPlugin` + server systems. `ListenServer`: same as Dedicated **plus** `ClientPlugins` with `NetConfig::Local { id: 1 }` so the host plays through the same Lightyear path as remotes (one codepath). `Client`: `ClientPlugins` (UDP `NetConfig::Netcode` against `--connect` addr) + `ProtocolPlugin` + client systems. `ProtocolPlugin` (`src/net/protocol_plugin.rs`) registers one `OrderedReliableChannel` and every Phase 2 wire message under the correct `ChannelDirection`. `start_server_on_startup_system` / `connect_client_on_startup_system` kick the Lightyear state machine. `NETCODE_PROTOCOL_ID` derives from `PROTOCOL_VERSION` so a version bump auto-rejects old clients.
- **Phase 2c shipped — server systems.** `src/net/server.rs`: `accept_connections_system` (drains `ServerConnectEvent`, allocates an unowned non-household materialised faction via `allocate_free_faction`, adds it to `ControlledFactions` + `ServerConnections`, sends `FactionAssignment` then `BootstrapSnapshot` on `OrderedReliableChannel`). `record_disconnections_system` drains `ServerDisconnectEvent` into `PendingDisconnects` for the disconnect-policy stage. `replicate_tile_overlays_system` deduplicates `TileChangedEvent` (uses real `.tx/.ty`), groups by chunk via `tile_to_chunk_coord`, emits `ChunkOverlayDelta` to `NetworkTarget::All` with Add/Remove ops for walls/doors/bridges/dams + Set/Clear runtime water (idempotent — `Remove*` for tiles never touched is a no-op on the client). `receive_command_frames_system` drains `ServerReceiveMessage<NetCommandFrame>`, validates declared `sender_faction_id` against the connection's `assigned_faction`, re-emits `NetPlayerCommandEvent` (the loopback validator then dispatches), sends `NetCommandAck` (`Accepted` / `OwnershipRejected`). v1 broadcasts to all clients — interest rooms are Phase 3.
- **Phase 2c/d shipped — client systems.** `src/net/client.rs`: `send_client_hello_system` ships `ClientHello { PROTOCOL_VERSION, player_name }` on `ClientConnectEvent`. `apply_bootstrap_snapshot_system` drains `FactionAssignment` (sets `PlayerFaction.faction_id`, adds to `ControlledFactions`), `BootstrapSnapshot` (spawns `Networked(NetId)` stubs for every overlay entity via `NetIdMap::bind`, then `apply_bootstrap_snapshot` rebuilds calendar/maps), `ChunkOverlayDelta` (per-op application with stub auto-spawn), `NetCommandAck` (folded into bounded `ClientAckLog`, cap 32). `send_command_frames_system` reads `NetPlayerCommandEvent`, stamps a monotonic `ClientCommandSequencer.next()` id, wraps in `NetCommandFrame`, ships via `ConnectionManager::send_message`. `observe_disconnect_system` logs `ClientDisconnectEvent` (reconnect path is Phase 3).
- **Phase 2e shipped — disconnect policy + CLI flag + speed lock.** `DisconnectPolicy::{AiTakeover (default), Pause, DropFaction}` resource. `--on-disconnect=ai-takeover|pause|drop-faction` CLI flag parsed in `cli.rs` (`CliError::UnknownDisconnectPolicy(String)`, 3 tests). `apply_disconnect_policy_system` drains `PendingDisconnects` per policy — AiTakeover/DropFaction strip the faction from `ControlledFactions` so the chief AI reclaims agency; Pause flips `Time<Virtual>::pause()`. `ConnectedRemotes { count }` resource + `speed_lock_system` clamps `Time<Virtual>::relative_speed → 1.0` and unpauses when any remote client connected. Both `Option<ResMut<Time<Virtual>>>` so headless test fixture (no MinimalPlugins) doesn't panic.
- **Net-id bind path** — `NetIdMap::bind(entity, server_chosen_id)` adopts a server-allocated id for a client-spawned stub, bumping `next` past it to prevent local-allocation collisions. Consumed by `client::apply_bootstrap_snapshot_system::ensure_stub`.
- **Phase 2 complete: 1144 tests green.**
- **Phase 3a (min) shipped** — `PROTOCOL_VERSION = 2`. `EntityKindWire` (Person / Animal(species) / Vehicle), `EntityStateEntry`, `EntityStateDelta { server_tick, chunk, entries }` in `net/protocol.rs`. Server `replicate_entity_state_system` (Update, every 3 ticks ≈ 50ms) samples all networked Person/Vehicle/Wolf/Deer/Horse/Cow/Pig/Cat in parallel queries, batches per-chunk via `tile_to_chunk_coord`. Client `apply_entity_state_delta_system` drains messages and auto-spawns stubs with `Networked`, `ReplicatedEntityKind`, `ReplicatedEntity` bookkeeping, `Transform`; Phase 3d will hang sprite trees off the marker. `ReplicationStats` resource + 5-s `info!` line is the 3g foothold (deltas / entries / bytes via `bincode::serialized_size`).
- **Per-entity remove signal shipped** — `EntityRemoved { net_ids: Vec<NetId> }` Server→Client. `NetworkedRemovedEvent` in `net_id.rs` populated by `release_net_ids_on_despawn` before the map drops the mapping. Server drains + broadcasts; client looks up via `NetIdMap` and `despawn_recursive`s. Closes the 3a loop — despawned entities no longer linger as stubs on clients.
- **Phase 3b (interest rooms) shipped** — `ConnectionState.interest_chunks` per client. `compute_interest_system` (Update, every 30 ticks ≈ 500ms) rebuilds each frame from `compute_interest_chunks(faction.home, INTEREST_RADIUS_CHUNKS=4)` ∪ every owned settlement's `market_tile` ring. `replicate_tile_overlays_system` + `replicate_entity_state_system` send per-chunk via `NetworkTarget::Only(recipients)`; chunks no one's watching skip the wire.
- **Phase 3c (rate tiering) shipped** — `InterestTier { Owned, Neighbour, Far }`; chunks classified by distance-from-anchor (Owned ±1, Neighbour ±`NEIGHBOUR_TIER_RADIUS=2`, Far fills rest of `INTEREST_RADIUS_CHUNKS=4`). `tier_cadence(tier)` → 1/2/10. `replicate_entity_state_system` walks a monotonic `send_index: Local<u32>` and uses `clients_for_entity_chunk(chunk, send_index)` to filter per-client by `send_index % cadence == 0` — Owned every 50ms, Neighbour every 100ms, Far every 500ms. Tile overlays don't tier.
- **Interest-gated removes shipped** — `LastKnownChunkMap` (`NetId → chunk`) populated by `replicate_entity_state_system`, drained by `replicate_entity_removals_system` which coalesces by sorted recipient set (`client_id_sort_key` for stable hashing) and ships `NetworkTarget::Only(recipients)` per group. Ids with no recorded chunk fall back to broadcast.
- **Camera focus shipped** — `ClientCameraFocus { tile }` Client→Server message. Client `send_camera_focus_system` (Update, every 30 ticks; only on chunk change) reads `Camera2d.Transform`. Server `receive_camera_focus_system` stashes on `ConnectionState.camera_focus_chunk`; `compute_interest_system` folds it into the anchor set so scouting expeditions outside the settlement ring still pull `Owned`-tier replication.
- **Phase 3e (reconnect-restoration) shipped** — `accept_connections_system` reduced to a logger; faction allocation moved into new `handle_client_hello_system` which validates `protocol_version` then either reclaims via `PendingReconnect.take(&player_name)` or allocates fresh. `ConnectionState` carries `player_name`; `record_disconnections_system` stashes `PendingReconnect { faction_id, expires_tick }` keyed on name with `RECONNECT_GRACE_TICKS = 1200` (~60 s at 20 Hz SimClock). `expire_pending_reconnects_system` GCs expired entries.
- **Active-military interest shipped** — `compute_interest_system` walks `faction.raid_party` and folds each member's current chunk in as a **Neighbour**-tier anchor (war bands shouldn't burn the same budget as settlements). Hunt parties / drafted defenders deferred.
- **Phase 3g (full) shipped** — `ReplicationStats` extended with per-channel raw accumulators + `*_per_sec` snapshot fields (`entity_*`, `tile_overlay_*`, `entity_removed_*`). New `report_replication_stats_system` (Update, every `STATS_REPORT_INTERVAL_UPDATES = 60` ≈ 1 s) computes rates, emits `debug!` line, zeros accumulators. Tile-overlay + entity-removed replicators now instrument bytes via `bincode::serialized_size`. Follows the project's Resource-based diagnostic convention (cf. `BackgroundWorkDiagnostics`) rather than pulling in `bevy_diagnostic`.
- **Hunt + drafted defender interest shipped** — `compute_interest_system` folds `HuntOrder::Hunt.mustered` plus any `Drafted` Person into the Neighbour-tier military anchor set alongside `raid_party`. Drafted defenders use a per-system `Query<(&Transform, &FactionMember), With<Drafted>>` (the sim doesn't expose a Vec).
- **Phase 3d shipped — client-side FogMap recompute.** `PROTOCOL_VERSION = 3`. `WireWallEntry` + `TileOverlayOp::AddWall` carry `owner_faction: Option<u32>` so client-side wall stubs stamp a faux `Wall { material: Palisade (placeholder), owner_faction }` via `ensure_wall_stub`. `fog_update_system` (`rendering/fog.rs`) gains a `Query<&ReplicatedEntity>` and runs the same per-agent LOS sweep on every `ReplicatedEntityKind::Person` stub belonging to the player faction — material is placeholder (fog reads only `owner_faction`; wall destruction stays server-side). Live-LOD treated as Full (no `LodLevel` on stubs); active-lookout state not yet replicated, so client agents use standard view radius.
- **Phase 3f shipped — per-client netcode tokens.** Shared `DEV_NETCODE_KEY: [u8; 32]` used by both server `NetcodeConfig` and client `Authentication::Manual` (replaces the broken `generate_key()` + zeroed-client-key handshake — they could never agree). Client `client_id` derived deterministically from `--player NAME` via `derive_client_id` (ahash with a fixed `RandomState::with_seeds` quad — process-independent; `AHasher::default()` keys off per-process randomness and would re-roll the id every restart, breaking reconnect-by-name). Reserved `0` + `HOST_SERVER_LOCAL_CLIENT_ID` bumped to avoid impersonating the host slot. Two clients sharing a name collide on `client_id`; netcode rejects the second as `AlreadyConnected`. Production deployments should still mint per-deployment `ConnectToken`s out-of-band; the manual-handshake path is the LAN/dev/playtest foothold.
- **Phase 3 complete. 1150 tests green.**

## Phase 1 — pipeline unification (no remote networking)

After Phase 1, `cargo run` uses the full server-auth pipeline locally; Phase 2 just exposes it over a socket.

### 1a. Stable network IDs (`src/net_id.rs`)
- `NetId(u32)` newtype with serde, monotonic.
- `NetIdMap` resource — bidirectional `Entity ↔ NetId` (server side).
- `Networked` component carrying `NetId`. Inserted at spawn for: `Person`, animals (Wolf/Deer/Horse/Cow/Pig/Cat), `GroundItem`, `Vehicle`, `Wall`, `Dam`, `Bridge`, `Door`, `Blueprint`, `Corpse`, `Plant`.
- `assign_net_ids_system` in `PostUpdate` covers all kinds via `Added<T>`.
- On `RemovedComponents<Networked>`, free id (no in-session reuse).

### 1b. Refactor entity-carrying `PlayerCommand` variants
Convert 7 variants from `Entity` to `NetId`: `PickUpItem`, `PickUpCorpse`, `AttackEntity`, `Teach`, `MilitaryAttack`, `VehicleOrder`, `DebugSpawnTestVehicle`. Server resolves `NetId → Entity` via `NetIdMap` before stamping `Commanded`. Failed resolution → ack `Failed("target gone")`.

### 1c. `ControlledFactions` + faction-scoped commands
- Keep `PlayerFaction { faction_id }` as local UI focus.
- Add `ControlledFactions { ids: SmallVec<[u32; 4]> }` resource — `Local`/`ListenServer` seed with player's faction; `DedicatedServer` populates per-connection.
- Faction-level command variants (vehicle queue, encode tablet, muster, migration) take `faction_id: u32` in payload instead of reading `player_faction.faction_id`. UI fills from `PlayerFaction`.

### 1d. Lightyear local-transport plumbing
- Add `lightyear = "0.19.1"`.
- `src/net/{mod, protocol, server, client, bootstrap}.rs`.
- `Local` mode installs both `ClientPlugin` + `ServerPlugin` in same App over `LocalChannel`. Server runs `WorldPlugin` + `SimulationPlugin` + `EconomyPlugin` + `PathfindingPlugin`. Client runs `RenderingPlugin` + `UiPlugin` + interpolation. May need sub-Apps if same-App doesn't compose cleanly — verify with hello-world first.
- **Fallback:** if Lightyear local transport is awkward in same-App, route `NetPlayerCommand` → `PlayerCommandEvent` via `command_loopback_system` that serializes/validates but skips the transport hop. Preserves one-path property at minor MP-fidelity cost.

### 1e. Tile-overlay snapshot helpers
Add `snapshot()` / `apply_snapshot()` to `DamMap`, `BridgeMap`, `WallMap`, `DoorMap`, `RuntimeWater` returning `Vec<(tile, payload)>` keyed by `NetId` for entity refs.

### 1f. Replace UI `EventWriter<PlayerCommandEvent>` with `CommandSender`
Touched UI files: `ui/{orders, selection, inspector, vehicle_designer, tech_panel, migration_panel, job_board, debug_panel, hud, activity_log}.rs`. `CommandSender` SystemParam always builds `NetPlayerCommand` and pushes through (local) transport. UI no longer touches `PlayerCommandEvent`; only the server's receive-and-validate system does.

**Deliverable:** `cargo run` works identically to today from player's POV; internally every command round-trips through serialize → server validation → dispatch. `cargo test --bin civgame` green.

## Phase 2 — remote networking

### 2a. CLI
```
cargo run                                       # Local
cargo run -- --listen --bind 0.0.0.0:5000       # ListenServer
cargo run -- --server --bind 0.0.0.0:5000       # DedicatedServer
cargo run -- --connect host:5000 --player NAME  # Client
```
Hand-rolled parsing, no new crate.

### 2b. Protocol expansion
- `ClientHello { player_name, protocol_version }`
- `FactionAssignment { faction_id, world_seed }`
- `BootstrapSnapshot { calendar, factions, owned_settlements, overlay_tiles, interest_chunks }`
- `ChunkOverlayDelta { chunk, ops: Vec<TileOverlayOp> }` — Add/Remove Wall/Dam/Bridge/Door/SetRuntimeWater
- `NetCommandAck { command_id, status, reason }`

Channels: `OrderedReliable` (commands/acks/hello/snapshot/tile-diffs), `UnreliableSequenced` (transform/task status).

### 2c. Server systems
- `accept_connections_system` — on connect, allocate unclaimed `FactionData`, respond with `FactionAssignment` + `BootstrapSnapshot`.
- `compute_interest_system` — per connection, chunks within `INTEREST_RADIUS_CHUNKS = 4` of camera/home/owned-settlements/owned-military. Diff vs last frame; update Lightyear rooms.
- `replicate_tile_overlays_system` — observe `TileChangedEvent`; push `ChunkOverlayDelta` ops to interested connections via 1e helpers.
- `replicate_entity_state_system` — Lightyear per-tick replication for `Networked` entities in interest rooms (Transform, FacingDir, task summary, health, carried, faction relation). Tier rate: owned at sim rate, neighbors 10 Hz, distant LOD 2 Hz.

### 2d. Client systems (`Client` mode only)
- `BootstrapSnapshot` handler spawns `Globe` + `ChunkMap` from `world_seed` (deterministic), applies overlays, spawns `Networked` stubs, sets `PlayerFaction`/`ControlledFactions`.
- Replication populates components on stubs.
- `FogMap` recomputes locally from replicated agents + walls.
- **Pause/speed:** `Local` zero-remote → allowed; ≥1 remote → locked to Normal. HUD shows lock.

### 2e. Disconnect/reconnect
- Disconnect: chief AI takes over. Configurable `--on-disconnect=ai-takeover|pause|drop-faction`.
- Reconnect: re-bootstrap fresh snapshot. No rollback.

**Deliverable:** `--listen` + `--connect` work over UDP loopback; host + remote client each drive a faction; entities and tile changes replicate; commands acknowledged.

## Phase 3 — interest tuning, bandwidth, robustness

- **Bandwidth budget.** Overlay deltas << 1 KB/tick steady-state. Entity rep dominates: ~200 visible × 32 B × 20 Hz ≈ 128 KB/s/client. Target <200 KB/s/client. Drop rate-tiers if exceeded.
- **Room transitions.** Smooth room moves on camera/settlement growth; avoid bursty mass-spawn frames.
- **Snapshot trimming.** Bootstrap scope = owned chunks + 1-ring; rely on streamed replication for rest.
- **Auth.** Player-name + session token, no accounts. Known limitation.

## Files

**New (Phase 1):** `src/net_id.rs`, `src/net/{mod, protocol, server, client, bootstrap, command_sender}.rs`.

**Modified (Phase 1):** `Cargo.toml` (lightyear), `src/main.rs` (CLI, NetMode, plugin topology), `simulation/player_command.rs` (NetId variants + faction_id fields + drain reads from validator), `simulation/faction.rs` (ControlledFactions), spawn sites for replicated entity kinds (`construction.rs`, `farm.rs`, `well.rs`, `terraform.rs`, `vehicle.rs`, `corpse.rs`, `items.rs`, `animals.rs`, `person.rs`, `plants.rs`) add `Networked` (or `NeedsNetId` marker → centralized system), UI files swap `EventWriter<PlayerCommandEvent>` → `CommandSender`.

**Modified (Phase 2):** `main.rs` skips render/sim plugins per NetMode, `net/protocol.rs` adds bootstrap + overlay-delta + ack, `net/server.rs` adds interest/replication/accept-connections, `net/client.rs` adds bootstrap/fog/ack handlers.

## Verification

**Phase 1:**
- `cargo check` + `cargo test --bin civgame` green.
- `cargo run` behaves identically; pick-up, attack, vehicle-order, build, dig all round-trip through NetId pipeline.
- Test: NetId-routed `PickUpItem` lands `Commanded` on actor and picks up item within N ticks.
- Test: `ControlledFactions = {1}` + command for faction-2 actor → dropped, ack `Failed("ownership")`.
- Test: 7 entity-carrying variants round-trip serde without loss.

**Phase 2:**
- Headless test: `DedicatedServer` + two `Client` Apps in one process on `127.0.0.1`. 200 ticks. Each client sees only own home chunks; cross-faction command rejected; wall built at server tick T appears in client snapshot by T+2.
- Manual: terminal 1 `--server --bind 127.0.0.1:5000`; terminals 2+3 `--connect ... --player A/B`. Independent move/build; no cross-command; correct HUD; pause/speed disabled.

## Open questions

- Does Lightyear 0.19 `LocalChannel` let `ServerPlugin` + `ClientPlugin` coexist in **one App**, or need sub-Apps? If sub-Apps, document choice in `src/net/mod.rs`.
- Single-process test: separate `App`s in one process (faster, needs LocalChannel multi-client) or subprocesses (CI-friendly)?
- `--sandbox --listen` should work for MP smoke tests; confirm spawn-radius assumptions hold.
