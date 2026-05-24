# Multiplayer Phase 3 — Entity Replication, Interest, Hardening

Phase 2 shipped wire protocol + Lightyear plugin install + bootstrap snapshot
+ tile-overlay delta replication + command flow + disconnect policy + speed
lock. Phase 3 finishes the live world.

## Progress

- **Open question resolved.** Custom `EntityStateDelta` batching wins over
  Lightyear's per-component `Replicate` registration. Mirrors the existing
  `ChunkOverlayDelta` pattern, keeps interest gating natural (group by
  chunk on the way out), avoids registering ~20 component types, and gives
  us per-tier rate control by changing one cadence constant.
- **3a (min) shipped.** `PROTOCOL_VERSION = 2`. New wire types
  `EntityKindWire { Person, Animal(AnimalSpeciesWire), Vehicle }` +
  `EntityStateEntry { net_id, kind, tile, z, facing, health_current,
  health_max, faction_id }` + `EntityStateDelta { server_tick, chunk,
  entries }` in `net/protocol.rs`; registered Server→Client on
  `OrderedReliableChannel`; bincode round-trip test
  (`entity_state_delta_round_trips`). `server::replicate_entity_state_system`
  (Update, every `ENTITY_REP_INTERVAL_UPDATES = 3` ≈ 50 ms) samples every
  Networked Person / Vehicle / Wolf / Deer / Horse / Cow / Pig / Cat via
  parallel read-only queries, batches per chunk via
  `tile_to_chunk_coord`, broadcasts `NetworkTarget::All`. Vehicle health
  totals from `VehicleHealth.cells`; animal Z defaults 0 (animal Z not
  modelled on PersonAI-style component). `client::apply_entity_state_delta_system`
  drains messages, auto-spawns `Networked` stubs via `ensure_replicated_stub`,
  upserts `Transform` + `ReplicatedEntity { kind, last_tick, tile, z, facing,
  health_current, health_max, faction_id }` + `ReplicatedEntityKind` marker.
  Marker stays component-side so future kind splits don't bump protocol.
- **3g foothold.** `server::ReplicationStats { entity_deltas_sent,
  entity_entries_sent, entity_bytes_sent }` resource + `info!` line every
  `STATS_REPORT_INTERVAL_UPDATES = 300` Update ticks (~5 s). Bytes counted
  via `bincode::serialized_size` on the delta — rough but actionable.
- **Per-entry remove signal shipped.** `EntityRemoved { net_ids: Vec<NetId> }`
  protocol message + registered Server→Client. `NetworkedRemovedEvent`
  event in `net_id.rs` populated by `release_net_ids_on_despawn` (in
  PostUpdate, before the map drops the mapping — necessary so we can read
  the NetId at all). Server `replicate_entity_removals_system` (Update)
  drains the event and broadcasts. Client `apply_entity_removed_system`
  looks up via `NetIdMap::entity_of` and `despawn_recursive`s the stub;
  unresolvable ids drop silently. Removes still broadcast even after
  Phase 3b's interest-room work — they're tiny and "remove an entity I
  never saw" is a no-op lookup.
- **3b (interest rooms) shipped.** `ConnectionState.interest_chunks:
  AHashSet<(i32, i32)>` per client. `compute_interest_system` (Update,
  every `INTEREST_REBUILD_INTERVAL_UPDATES = 30` ≈ 500 ms) rebuilds each
  client's frame as the union of (a) `compute_interest_chunks` over the
  assigned faction's home (`INTEREST_RADIUS_CHUNKS = 4`) and (b) every
  owned settlement's `market_tile` ring. `ServerConnections::clients_interested_in(chunk)`
  is the inverse lookup. `replicate_tile_overlays_system` and
  `replicate_entity_state_system` now send per-chunk via
  `NetworkTarget::Only(recipients)` instead of `NetworkTarget::All`;
  empty recipient list skips the wire entirely. `EntityRemoved` stays
  broadcast (no chunk context — see above).
- **3c (rate tiering) shipped.** `InterestTier { Owned, Neighbour, Far }`
  classifies each chunk in a client's frame. `compute_interest_system`
  layers anchors (faction home + every owned settlement market) — `Owned`
  in the ±1-chunk ring of each anchor, `Neighbour` extends to
  ±`NEIGHBOUR_TIER_RADIUS=2`, `Far` fills the rest of
  `INTEREST_RADIUS_CHUNKS=4`. `ConnectionState.interest_chunks` is now
  `AHashMap<chunk, InterestTier>`. `tier_cadence(tier)` returns
  `1 / 2 / 10` — `replicate_entity_state_system` walks a monotonic
  `send_index: Local<u32>` and consults
  `ServerConnections::clients_for_entity_chunk(chunk, send_index)`,
  which filters per-client by `send_index % cadence == 0`. Net effect:
  Owned chunks ship every 50ms, Neighbour every 100ms, Far every 500ms.
  Tile-overlay deltas keep "any tier" gating via `clients_interested_in`
  — sparse + reliable, doesn't justify gating.
- **Interest-gated removes shipped.** New `LastKnownChunkMap` resource
  (`AHashMap<NetId, (i32,i32)>`) populated by
  `replicate_entity_state_system` each send. `replicate_entity_removals_system`
  coalesces removed ids by recipient set (sorted via `client_id_sort_key`
  for stable hashing) and ships `NetworkTarget::Only(recipients)` per
  group; ids with no recorded chunk fall back to broadcast. Map entry
  evicted on remove so the table doesn't grow unbounded.
- **Test status.** 1146 total tests; 1145 pass, 1 pre-existing sim flake
  (`funded_household_in_market_preset_acquires_plot`) that passes alone
  deterministically but fails under multi-test load — caused by the
  pre-session staged simulation changes in the working tree, unrelated
  to net code (which never runs in test mode: `NetMode::Local` default,
  `install_server_systems` only wired for `ListenServer/DedicatedServer`).
  All 16 `net::protocol` round-trip tests green.

- **Camera focus shipped.** New `ClientCameraFocus { tile }` Client→Server
  message on `OrderedReliableChannel`. Client
  `send_camera_focus_system` (Update, every
  `CAMERA_FOCUS_SEND_INTERVAL_UPDATES = 30` ≈ 500 ms) reads `Camera2d`
  transform, converts to a tile, and only sends when the **chunk** has
  changed since the last successful send (no spam from sub-chunk drift).
  Server `receive_camera_focus_system` stashes the focus chunk on
  `ConnectionState.camera_focus_chunk`. `compute_interest_system` folds
  it into the anchor set, so the camera position promotes its chunk to
  `Owned` (±1 ring) regardless of where the faction home sits.
- **3e (reconnect-restoration) shipped.** `accept_connections_system`
  reduced to a log line — faction allocation now waits for the
  corresponding `ClientHello` (which carries `player_name`). New
  `handle_client_hello_system` drains `ServerReceiveMessage<ClientHello>`,
  validates protocol version, and either (a) reclaims the prior
  `faction_id` via `PendingReconnect.take(&player_name)` if the
  disconnected client is still inside the grace window, or (b) allocates
  a fresh faction via `allocate_free_faction`. Both paths ship
  `FactionAssignment` + `BootstrapSnapshot` identically. `ConnectionState`
  now carries `player_name`. `record_disconnections_system` stashes a
  `PendingReconnect { faction_id, expires_tick = now + RECONNECT_GRACE_TICKS }`
  entry keyed on `player_name`; `expire_pending_reconnects_system` (Update,
  every 60 ticks ≈ 1 Hz) drops expired entries.
  `RECONNECT_GRACE_TICKS = 1200` ≈ 60 s at 20 Hz `SimClock`.
- **Build green: 1147 tests** (baseline 1144 + 3 round-trips so far:
  entity-state / entity-removed / client-camera-focus).

- **Active-military interest shipped.** `compute_interest_system` walks
  `faction.raid_party: Vec<Entity>` (via `transform_q`) and folds each
  member's current chunk into the anchor set at **Neighbour** tier (not
  Owned — a roving war band shouldn't burn the same bandwidth budget
  as the player's settlement). Raid parties cap at `RAID_MAX_PARTY_ABS`
  so the iteration cost stays trivial. Hunt parties + drafted defenders
  still pending.
- **3g (full) shipped.** `ReplicationStats` extended with per-channel
  raw accumulators (`entity_*`, `tile_overlay_*`, `entity_removed_*`)
  plus matching `*_per_sec` snapshot fields. New
  `report_replication_stats_system` (Update, every
  `STATS_REPORT_INTERVAL_UPDATES = 60` ≈ 1 s) computes per-second rates,
  emits a single `debug!` line, and zeros accumulators. Follows the
  project's existing Resource-based diagnostics convention
  (cf. `BackgroundWorkDiagnostics` / `PathfindingDiagnostics`) rather
  than pulling in `bevy_diagnostic`. Tile-overlay + entity-removed
  replicators now instrument their byte counts via
  `bincode::serialized_size`.

## Deferred (still pending)

- **Hunt parties + drafted defenders shipped.** `compute_interest_system`
  now folds `HuntOrder::Hunt.mustered` + any `Drafted` Person member into
  the Neighbour-tier anchor set alongside `raid_party`. Drafted defenders
  use a per-system `Query<(&Transform, &FactionMember), With<Drafted>>` —
  the sim doesn't expose a Vec, so we scan the (small) `Drafted` query.
- **3d (FogMap client recompute) shipped.** `WireWallEntry` /
  `TileOverlayOp::AddWall` carry `owner_faction: Option<u32>`; client
  `ensure_wall_stub` stamps a `Wall { material: Palisade (placeholder),
  owner_faction }` on each wall stub. `fog_update_system` gains a
  `Query<&ReplicatedEntity>` and runs the same per-agent LOS sweep on
  every player-faction `ReplicatedEntityKind::Person` stub. `PROTOCOL_VERSION`
  bumped 2→3. Material on stub is a placeholder (Palisade); fog reads
  `owner_faction` only and wall destruction is server-side, so the
  fake material never surfaces. Live-LOD treated as Full (no `LodLevel`
  on stubs); active-lookout state not yet replicated, so client agents
  use standard view radius.
- **3f (per-client netcode tokens) shipped.** Shared dev key
  `DEV_NETCODE_KEY = [0; 32]` used on both ends (server `NetcodeConfig`
  + client `Authentication::Manual`) — previously the server generated
  a fresh random key per startup, so no remote could actually connect.
  Client `client_id` derived deterministically from `--player NAME` via
  `derive_client_id(name)` (ahash with a fixed seed quad so the value is
  process-independent — `ahash::RandomState::with_seeds(...)`, not
  `AHasher::default()` which keys off process-local randomness). Two
  clients sharing a name collide on `client_id`; netcode rejects the
  second as `AlreadyConnected`. Reserved `0` + `HOST_SERVER_LOCAL_CLIENT_ID`
  bumped to avoid impersonating the host slot. Production deployments
  should still mint per-deployment `ConnectToken`s out-of-band — the
  manual-handshake path is the LAN/dev/playtest foothold.

## 3a. Per-tick entity component replication

Lightyear `register_component<C>(ChannelDirection::ServerToClient)` for each
component, plus `Replicate` bundle inserted on every server-side spawn site.

**Components to register:**
- `Transform` (`SyncMode::Full` + interpolation)
- `simulation::person::PersonAI` (subset — `state`, `current_z`, `target_tile`)
- `simulation::combat::Health`, `Body`
- `simulation::faction::FactionMember`
- `simulation::nomad::FollowingBand`
- `simulation::vehicle::{Vehicle, VehicleHealth, VehiclePathFollow, VehicleInventory}`
- `world::tile::TileChangedEvent` is NOT a component — already covered by
  `ChunkOverlayDelta`.

**Spawn sites needing `Replicate` bundle insertion:**
- `simulation/person.rs::spawn_faction_band` — Person + Body + Carrier
- `simulation/animals.rs::spawn_animals` + `spawn_tamed_at_tile`
- `simulation/vehicle.rs::spawn_vehicle_at`
- `simulation/items.rs::spawn_ground_drop` / `spawn_ground_item`
- `simulation/construction.rs::convert_blueprint_to_*` + wall/door/bridge/dam
  finalize paths (currently covered by tile-overlay delta — entity replication
  adds visual sprite sync, health bar, etc.)
- `simulation/plants.rs::spawn_plant`
- `simulation/corpse.rs::spawn_corpse`

**Lightyear `Replicate` bundle:**
```rust
commands.spawn((Person { .. }, Body::default(), ..., Replicate::default()));
```
`Replicate` resolves `NetId` via Lightyear's own entity-id remap; reconcile
with our `NetIdMap` by registering `Networked` as a replicated component or
by relying on Lightyear's `RemoteEntityMap`.

## 3b. Interest rooms

Replace `NetworkTarget::All` in `server::replicate_tile_overlays_system`
+ entity replicate sets with `RoomManager` membership:

- `compute_interest_system` (Sequential, after camera updates): per
  `ServerConnections.by_client` entry, compute chunks within
  `INTEREST_RADIUS_CHUNKS=4` of the connection's faction `home_tile` +
  any owned settlements + any owned military groups. Diff vs last
  frame's room membership; `RoomManager::add_client_to_room` /
  `remove_client_from_room`.
- `Room` keyed per chunk; replicated entities attach to chunk's room
  via their `Transform`-projected `ChunkCoord`.

## 3c. Per-tick rate tiering

`SendUpdatesMode` tier:
- Owned faction entities: every tick (50ms)
- Neighboring faction entities: every 2 ticks (100ms)
- Far-LOD entities: every 10 ticks (500ms)

Use `Replicate { send_frequency, .. }` or per-entity throttle resource.

## 3d. FogMap client-side recompute

Client doesn't trust replicated `FogMap`; instead recomputes from replicated
agents + walls each tick via the existing `cached_vision_set` machinery on
replicated entities. `rendering::fog::fog_update_system` already drives
this; needs Replicated agent positions to feed it.

## 3e. Reconnect-restoration

On `ClientDisconnectEvent`:
- Save `PendingReconnect { faction_id, client_id, expires_at }` server-side
- Within `RECONNECT_GRACE_SECS=60`, a returning client with matching
  `player_name` reclaims the same faction (skip `allocate_free_faction`)
- After grace, faction stays Detached / under AI per `DisconnectPolicy`

## 3f. Per-client netcode tokens

Replace `Authentication::Manual { private_key: [0; 32], client_id: 1001 }`
with server-minted `ConnectToken`s:
- Client connects out-of-band (HTTP/auth server) and gets a signed token
- Server validates token in `ConnectionRequestHandler` (`NetcodeConfig`)
- Tokens carry per-player client_id + permissions

## 3g. Bandwidth budget + diagnostics

Target <200 KB/s/client per `plans/multiplayer.md` Phase 3 budget.
- `bevy_diagnostic` counters for serialized bytes/sec/client
- Per-channel breakdown (commands vs entity rep vs overlay deltas)
- Drop rate-tiers if exceeded; warn on overshoot

## Verification

- Headless integration test in `tests/integration/` (new dir): spawn
  `DedicatedServer` + 2 `Client` Apps in one process on `127.0.0.1`,
  step 200 ticks, assert: each client sees only own home chunks;
  cross-faction command rejected; wall built at server tick T appears
  on both clients by T+2.
- Manual 3-process smoke: `--server --bind 127.0.0.1:5000`,
  two `--connect 127.0.0.1:5000 --player A/B`. Independent build,
  speed locked, HUD shows lock.

## Open questions

- Bevy `Replicate` vs custom snapshot batching — Lightyear's per-component
  replication is convenient but registers ~20 component types. Custom
  entity-snapshot message (`EntityStateDelta { net_id, transform, health, ai_state }`)
  might be cheaper on the wire and easier to interest-gate. Decide before
  starting 3a.
- Do we want client-side prediction / rollback for own-faction agents?
  Out-of-scope for 3a; revisit after baseline rep works.
