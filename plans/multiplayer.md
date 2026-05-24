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
