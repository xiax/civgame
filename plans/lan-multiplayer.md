# LAN Multiplayer, Lobby, and Multi-Start

## Context

Builds on the already-shipped server-authoritative core (Phases 1+2+3 of
`plans/multiplayer.md` and `plans/multiplayer-phase3.md`, 1150 tests
green). This plan adds the runtime LAN host/join lobby, multi-human-
start map selection, and the protocol/auth adjustments to support more
than one player faction.

V1 scope is LAN-first with separate factions only. Federation /
shared-faction co-op already has a skeleton plan
(`plans/diplomacy-federations.md`) and is intentionally out of scope —
forking multiplayer-v1 to chase it would block the simpler LAN path.

## Architecture summary

- **Lifecycle:** `MainMenu → {SpawnSelect | MultiplayerLobby} → Playing`.
  Singleplayer keeps today's SpawnSelect path verbatim. Multiplayer
  goes through a new lobby state.
- **Topology:** re-launch the binary on Host/Join (CLI args carry the
  intent). Lightyear 0.19's `ServerPlugins::new(config)` consumes the
  config at install time, so there is no supported way to swap
  transports after `App::run`. Re-launch is the smallest diff and
  matches every other indie LAN game.
- **Discovery:** UDP broadcast on port 5001 (separate from game port
  5000). Manual `host:port` fallback for silent/firewall'd networks.
- **Spawning:** every human slot resolves to a starting megachunk via
  the existing `region::pick_player_home_in_megachunk`; AI rivals
  score against the union of human homes; abstract factions fill the
  rest of the globe (existing `seed_abstract_factions_system`).
- **Authority:** server owns sim; clients run only render / UI / fog /
  chunk-streaming. The existing `command_loopback_system` +
  `NetPlayerCommandEvent` path stays the single command channel.
- **Reconnect:** existing `PendingReconnect` (60 s grace, keyed on
  `player_name`) covers in-game disconnects; lobby uses the same
  key for slot reclaim.

## Phase 0 — verify

Run `cargo test --bin civgame net` from a clean tree. The previous
draft of this plan claimed `GatherTarget` tuple/struct mismatches in
`gather.rs:532`, `htn.rs:6053`, `test_fixture.rs:14765`; those line
numbers don't match the live files and the in-tree progress log says
1150 tests green. If actually red: produce the real error first.
Otherwise skip and proceed.

## Phase 1 — lifecycle + game-state plumbing (no networking yet)

**Critical files:** `src/game_state.rs`, `src/main.rs`,
`src/ui/spawn_select.rs`, `src/ui/main_menu.rs` (new),
`src/ui/lobby.rs` (new), `src/simulation/test_fixture.rs`.

- Add `GameState::{MainMenu, MultiplayerLobby, Playing}`; keep
  `SpawnSelect` as the singleplayer sub-screen. Default flips from
  `SpawnSelect` to `MainMenu`. Gate `WorldPlugin`'s globe-gen on
  `OnEnter(SpawnSelect)` and `OnEnter(MultiplayerLobby)` so the first
  paint isn't waiting on the ~600 ms globe build.
- Replace `PendingSpawn(Option<(i32, i32)>)` with `PendingStarts`:
  ```
  Resource PendingStarts {
      primary_start: Option<(i32, i32)>,        // camera anchor / SP path
      slots: Vec<PlayerStartSlot>,
  }
  PlayerStartSlot {
      slot_id: u8,
      player_name: String,
      client_id: u64,
      megachunk: Option<(i32, i32)>,
      lifestyle: Lifestyle,
      ready: bool,
      faction_id: Option<u32>,                  // assigned at game start
  }
  ```
  Singleplayer fills `slots` with one entry whose `client_id` is the
  reserved `HOST_SERVER_LOCAL_CLIENT_ID`. `PendingSpawn` becomes a
  compat view written by a `legacy_pending_spawn_compat_system`
  mirroring `slots[0].megachunk` so the test fixture and any
  un-migrated reader keep working.
- Add `ui::main_menu` with three buttons: Singleplayer
  (→ `SpawnSelect`), Host LAN Game (→ `MultiplayerLobby` host-role),
  Join LAN Game (→ `MultiplayerLobby` join-role).

## Phase 2 — LAN host/join via re-launch

**Critical files:** `src/net/cli.rs`, `src/net/lan.rs` (new),
`src/ui/lobby.rs` (new), `src/main.rs`.

- **MainMenu → Host:** show a config sub-panel (seed, era, economy,
  maturity, max players, bind port). On "Open Lobby": re-launch
  `civgame` with `--listen --bind 0.0.0.0:<port> --seed <seed>
  --player <name>` plus a new `--lobby-config <path>` pointing at a
  temp RON of `GameStartOptions` + slot caps. The relaunched process
  boots straight into `MultiplayerLobby` host-role.
- **MainMenu → Join:** show LAN browser (live list from `net::lan`) +
  manual `host:port` field. Manual entry takes precedence over the
  browser selection (no auto-fill races). On Join: re-launch with
  `--connect <addr> --player <name>`. Boots into `MultiplayerLobby`
  join-role.
- **`net::lan`:** UDP broadcast on port 5001, every 1 s. Payload:
  ```
  LanAdvert { protocol_version, game_name, host_name, addr, game_port,
              players, max_players, phase, world_seed }
  ```
  bincode-serialised. Listener TTL 3 s. Broadcast to 255.255.255.255
  with `SO_BROADCAST`; no multicast (firewall friction on
  Windows/macOS). Silent network → manual address path. Bind failures
  retry once after 500 ms (covers same-machine relaunches).
- Reusable later: when upstream Lightyear exposes mutable `NetConfig`,
  the lobby UI doesn't change — only the relaunch shells out to
  in-process transport swap.

## Phase 3 — lobby protocol

**Critical files:** `src/net/protocol.rs`, `src/net/protocol_plugin.rs`,
`src/net/server.rs`, `src/net/client.rs`.

- Bump `PROTOCOL_VERSION` (5 → 6) and register the new lobby messages
  on the existing `OrderedReliableChannel`:
  - **Client → Server:** `LobbyJoin { protocol_version, player_name }`
    (replaces `ClientHello` while in `MultiplayerLobby`),
    `LobbySelectStart { megachunk }`, `LobbySetReady { ready }`,
    `LobbyLeave`.
  - **Server → Client:** `LobbySnapshot { game_name, world_seed, era,
    economy, maturity, max_players, slots: Vec<LobbySlotPublic> }`,
    `LobbyReject { reason: LobbyRejectReason }`,
    `LobbyStartGame { slot_assignments }`.
  - **Host-only (server-internal, not on the wire):** `HostSetConfig`,
    `HostKickSlot`, `HostStartGame`. Route through `CommandSender` so
    single-process tests cover them.
- Server lobby state machine: `Hosting → SelectingStarts → Starting →
  InGame`. Start validation: habitable, unclaimed, ≥
  `min_human_megachunk_distance = 3` from every other slot
  (re-use `region::pick_player_home_in_megachunk` + new
  slots-aware validator).
- Reconnect: existing `PendingReconnect` + `ClientHello`-keyed-on-
  `player_name` path keeps working in `Playing`. In
  `MultiplayerLobby` the same name reclaims the same slot (faction
  isn't assigned until game start).

## Phase 4 — multi-start spawn rework

**Critical files:** `src/simulation/person.rs` (`spawn_population`,
`spawn_faction_band`), `src/world/terrain.rs` (`spawn_world_system`),
`src/simulation/region.rs`, `src/main.rs` (OnEnter chain).

Today `spawn_population` (`src/simulation/person.rs:373`) early-binds
`near_factions = NEARBY_RIVAL_COUNT + 1 = 4` and treats `group_idx == 0`
as "the player faction." For K human slots the contract becomes:

1. Loop each human slot: resolve home via
   `pick_player_home_in_megachunk(slot.megachunk, ...)`; push onto
   `spawned_homes`; create faction; assign econ / lifestyle /
   maturity from `GameStartOptions` (lifestyle per-slot); if the
   slot's `client_id` matches the local client, set `PlayerFaction`;
   always add to `ControlledFactions`.
2. Loop up to `(NEARBY_RIVAL_COUNT + 1) − human_count` AI rivals:
   same best-of-200 search using existing `score_home_candidate`,
   but `spawned_homes` already contains every human start so
   `faction_spacing_score` arbitrates correctly.
3. `seed_abstract_factions_system` is already globe-aware; verify it
   skips megachunks any human slot occupies.

Spacing note: `MEGACHUNK_SIZE_CHUNKS * CHUNK_SIZE = 512 tiles` per
megachunk and `NEAR_FACTION_TARGET_SPACING = 280 tiles`, so two humans
on adjacent megachunks already land outside the spacing saturation
distance — no extra human-vs-human spacing rule required at the
spawn-population layer (the lobby validator owns the floor).

`spawn_world_system`: pre-generate the **union** of 32×32-chunk
windows around every human start, not just `pending.0`. Memory: one
window ≈ 1024 chunks × ~4 KB ≈ 4 MB; four players ≈ 16 MB. After
OnEnter, normal streaming takes over.

Sandbox stays single-start (asserted at the call site). Tests stay on
the one-slot compat path.

## Phase 5 — client/server authority cleanup

**Critical files:** `src/net/mod.rs` (run conditions),
`src/simulation/mod.rs` (system gating), `src/main.rs`, `src/ui/hud.rs`.

- Gate `command_loopback_system` to
  `NetMode::{Local, ListenServer, DedicatedServer}` (drains nothing in
  `Client` mode; tidies hygiene).
- Run-condition `SimulationPlugin`, `EconomyPlugin`, and
  `PathfindingPlugin`'s mutating systems on `not(NetMode::Client)`.
  Read-only systems (`world::chunk_streaming_system`, fog, sprite,
  camera) keep running. The `DedicatedServer` plugin gate
  (`MinimalPlugins + LogPlugin`) already proves the topology pattern.
- Extend `client::apply_bootstrap_snapshot_system` to set `WorldSeed`,
  fire `RegenerateWorldRequest`, write
  `PendingStarts.primary_start` from the assigned faction's
  `home_tile`, then `NextState(Playing)`.
- Fix `ConnectedRemotes`: today only bumped on `ServerConnectEvent`;
  decrement in `apply_disconnect_policy_system` so the existing
  `speed_lock_system` releases correctly when the last remote leaves.
- HUD: one line — when `connected_remotes.count > 0`, render
  "MP — speed locked to 1×" alongside the existing speed controls so
  players understand why pause / 2× / 5× are inert.
- Reject non-reconnect joins after `LobbyStartGame` in v1; reconnect
  by player_name continues to work (already shipped).

## Phase 6 — replication completeness via central observers

**Critical files:** `src/net_id.rs`, no spawn-site edits.

- Add per-archetype `auto_tag_replicable_system<T>`:
  ```
  fn auto_tag_replicable<T: Component>(
      mut commands: Commands,
      added: Query<Entity, (Added<T>, Without<NeedsNetId>, Without<Networked>)>,
  ) { for e in &added { commands.entity(e).insert(NeedsNetId); } }
  ```
  Register in `NetIdPlugin` for `Person`, `Vehicle`, `GroundItem`,
  `Plant`, `Corpse`, `Blueprint`, `Bed`, `Door`, `Wall`, `Workshop`,
  `Campfire`, `Settlement`, `Camp`. PostUpdate, before
  `assign_net_ids_system`. Negligible cost (cheap `Added<T>` filter).
  Spawn sites stay untouched; replication completeness becomes a
  property of one file.
- Extend `EntityStateDelta` to cover `GroundItem` (sparse,
  appear/move/despawn at low rate-tier) and `Corpse` (low rate).
- Plants are tile-resident: extend `ChunkOverlayDelta` with
  `AddPlant / RemovePlant / StageChange` ops keyed on tile.
  Consistent with the wall/door pipeline.
- Workshops / beds / campfires / civic anchors: durable single-tile
  entities. Extend `ChunkOverlayDelta` with
  `AddStructure { kind, tile, owner_faction, label_id }`. One
  protocol bump covers every variant.
- **Inspector summary protocol** (new request/response): client sends
  `InspectorSummaryRequest { net_id }` on `OrderedReliableChannel`;
  server responds with `InspectorSummaryResponse { task, goal, wage,
  knowledge_summary }`. Avoids replicating every internal field.

## Phase 7 — verification

**Unit tests** (in-tree, headless):
- `lan_advert_round_trip` — bincode encode/decode.
- `lobby_start_validation_rejects_duplicates`,
  `lobby_start_validation_enforces_min_distance`,
  `lobby_readiness_rules`.
- `pending_starts_compat_legacy_single_slot` — one-slot
  `PendingStarts` populates `PendingSpawn` via compat.
- `spawn_population_two_human_slots` — fixture with two slots spawns
  two factions in `ControlledFactions`; AI rivals score against both.
- bincode round-trip tests for every new wire message
  (`LobbyJoin / LobbySnapshot / LobbyReject / LobbyStartGame /
  LobbySelectStart / LobbySetReady / LobbyLeave /
  InspectorSummaryRequest / InspectorSummaryResponse` + the new
  `ChunkOverlayDelta::AddStructure / AddPlant / …` ops).

**Headless integration** (single process, `127.0.0.1`): host App + two
client Apps, 200 ticks; assert each client only sees own-faction
chunks at `Owned` tier; cross-faction command rejected with
`OwnershipRejected`; wall built at server tick T appears on both
clients by T+2; removed entity drops both client stubs within
`RECONNECT_GRACE_TICKS / 60` ticks of removal.

**Manual acceptance** (catches firewall / discovery / relaunch
friction):
1. Two machines on the same LAN.
2. Host clicks Host LAN Game, sets seed = 4242, max_players = 2.
3. Join machine clicks Join, sees host in browser within 2 s, picks
   a non-conflicting megachunk.
4. Both ready up; host clicks Start.
5. Each controls only their own faction; right-click on the other
   faction's chunks rejects with `OwnershipRejected` (visible in
   debug overlay).
6. Force-quit join machine; reconnect within 60 s reclaims same
   faction. After 60 s, `AiTakeover` strips control.

## Files touched

**New:** `src/net/lan.rs`, `src/ui/main_menu.rs`, `src/ui/lobby.rs`.

**Modified:** `src/game_state.rs` (`GameState` + `PendingStarts`),
`src/main.rs` (CLI flags + state default + plugin gating),
`src/net/{cli,mod,protocol,protocol_plugin,server,client}.rs` (lobby
messages, run conditions, `apply_bootstrap_snapshot_system` extension,
`ConnectedRemotes` decrement, auto-tag observers),
`src/net_id.rs` (auto-tag observers),
`src/simulation/{person.rs, region.rs}` (multi-start
`spawn_population`, slots-aware home validator),
`src/world/terrain.rs` (multi-window pre-gen),
`src/ui/{spawn_select,hud,world_map}.rs` (speed-lock indicator;
shared `build_globe_image` consumed by lobby).

## Assumptions

- LAN-only with manual IP fallback; no NAT traversal, no public
  browser, no accounts, no production auth.
- Separate factions only; shared-faction co-op is a later mode.
- New joins rejected after `LobbyStartGame` except reconnect-by-name.
- No new crates — `lightyear`, `serde`, `bincode`, `bevy_egui`,
  `std::net::UdpSocket` cover everything.

## Deferred (skeleton intent)

- **In-app NetMode switching** (no relaunch): requires upstream
  Lightyear PR exposing `NetConfig` mutability. Lobby UI unchanged
  when it lands.
- **Shared-faction co-op lobby mode**: see
  `plans/diplomacy-federations.md` + `plans/diplomacy-marriage.md`.
- **Internet multiplayer** (NAT traversal, matchmaking, accounts):
  separate `plans/internet-multiplayer.md` later.
- **Late-join faction takeover** (vs. only reconnect-by-name):
  needs "abandoned faction" claim system plus fresh
  `BootstrapSnapshot` mid-game. Interest rooms already work, so
  cheap, but not v1.
- **Inspector summary as subscribe-on-focus** (vs. request/response):
  keyed on `ClientCameraFocus`, would auto-stream the snapshot for
  the entity in view.

## Phase 8 — wire-up completion

Phases 1–7 landed shapes; this phase drains them. Closes: lobby
message handlers, client appliers, `apply_bootstrap_snapshot_system`
state transition, sim-plugin gating on client, plant/structure
delta emission, and the host+2-client integration test.

### 8.1 Server lobby handlers (`src/net/server.rs`, `lobby_state.rs`)

Four drain systems gated on `NetMode::{ListenServer, DedicatedServer}`:
- `handle_lobby_join_system` — validate `protocol_version`; reclaim
  via `lobby.accepts_join(&name)` else append via `next_slot_id()`;
  `bump()`; `LobbyReject` on failure.
- `handle_lobby_select_start_system` — new pure validator
  `is_start_megachunk_acceptable(&lobby, client_id, megachunk,
  &globe)` in `lobby_state.rs` (habitability via
  `region::pick_player_home_in_megachunk`'s checks +
  `MIN_HUMAN_MEGACHUNK_DISTANCE = 3`); write `slot.megachunk` or
  reject.
- `handle_lobby_set_ready_system` — write `slot.ready`; `bump()`.
  Extend `LobbyState::bump()` so `SelectingStarts → Starting` fires
  when every slot has `megachunk.is_some() && ready`.
- `handle_lobby_leave_system` — drain `LobbyLeave` +
  `ServerDisconnectEvent` while pre-`InGame`; remove slot (no
  reconnect stash in lobby).

Plus:
- `broadcast_lobby_snapshot_system` — when `lobby.version` differs
  from `last_sent_version`, ship `LobbySnapshot` to every connected
  client on `OrderedReliableChannel`.
- `start_game_transition_system` — on phase entering `Starting`,
  allocate `faction_id` per slot (monotonic by `slot_id`), broadcast
  `LobbyStartGame { slot_assignments }`, populate
  `PendingStarts.{slots, primary_start}`, `NextState(Playing)`,
  phase → `InGame`.

### 8.2 Client lobby appliers (`src/net/client.rs`, `src/ui/lobby.rs`)

- `apply_lobby_snapshot_system` (NetMode::Client) — mirror snapshot
  into `LobbyUiState`; add `remote_slots: Vec<LobbySlotPublic>`
  (client doesn't write `PendingStarts.slots` until
  `LobbyStartGame`). Lobby UI reads `remote_slots` for the roster.
- `apply_lobby_start_game_system` — drain `LobbyStartGame`, build
  `PendingStarts { primary_start: megachunk→camera tile for local
  client, slots: from slot_assignments }`, `NextState(Playing)`.
  Don't spawn — sim is gated off on Client.
- UI sends `LobbySelectStart / LobbySetReady / LobbyLeave` via a new
  `LobbyCommandSender` SystemParam. ListenServer host bypasses the
  wire via a `LocalLobbyCommand` event drained by the same 8.1
  handlers under `HOST_SERVER_LOCAL_CLIENT_ID`.

### 8.3 Bootstrap extension (`apply_bootstrap_snapshot_system`)

After draining `BootstrapSnapshot`:
- Set `WorldSeed(snapshot.world_seed)` (client has no seed today).
- Fire `RegenerateWorldRequest` so `spawn_world_system` builds the
  windows around assigned home.
- Resolve assigned faction's `home_tile` from
  `snapshot.factions`; write `PendingStarts.primary_start`.
- `NextState(Playing)` if in `MultiplayerLobby` (canonical path on
  reconnect, where lobby was skipped).

### 8.4 Client gating (`simulation/mod.rs`, `economy/mod.rs`, `pathfinding/mod.rs`)

Add `pub fn net_mode_runs_sim(mode: Res<NetMode>) -> bool` in
`net/mod.rs`. Wrap mutating system *sets*
(`SimulationSet::{ParallelA, ParallelB, Sequential, Economy}` and
`Input`, plus equivalents in the other two plugins) with
`.run_if(net_mode_runs_sim)`. Render / fog / camera / world streaming
/ UI keep running. `spawn_world_system` stays ungated — clients
regen terrain from the replicated seed.

### 8.5 Plant + structure replication

Server (`replicate_tile_overlays_system::push_ops_for_tile`):
- Probe `PlantMap` → emit `TileOverlayOp::AddPlant { tile,
  entity_net_id, kind, stage }`; `RemovePlant` from a
  `RemovedComponents<Plant>` reader.
- Probe a new `StructureMap` resource (mirrors `PlantMap`, populated
  by `Added<StructureLabel>` observer + `on_remove` hook) → emit
  `TileOverlayOp::AddStructure { tile, entity_net_id, kind,
  owner_faction, label_id }`.

Client (`apply_overlay_delta`): for both ops, in addition to existing
stub spawn, write a `ReplicatedPlantMap` / `ReplicatedStructureMap`
resource entry so renderer/inspector can resolve tile → entity. Walls
already do this; same pattern.

Verify spawn/stage/despawn sites in `simulation/plants.rs` and the
structure spawners emit `TileChangedEvent` (the existing replicator
cadence drives the per-tick emit; no new schedule).

### 8.6 Headless host+2-client integration test

`src/net/integration_tests.rs` (new, `#[cfg(test)]`). Build
`MultiAppHarness { server: App, clients: Vec<App> }` with
in-memory transport (preferred) or localhost UDP fallback.

Scenario: server `ListenServer` + 2 `Client`s; each client
`LobbyJoin → LobbySelectStart (non-conflicting megachunks) →
LobbySetReady`; host slot injected via `LocalLobbyCommand`. After
auto-transition to `Starting`, step 200 ticks. Assert:
- All three Apps in `GameState::Playing`.
- Each client's `ControlledFactions` contains exactly its assigned
  faction.
- `ReplicationStats` shows nonzero entity + tile-overlay deltas.
- Cross-faction command from client 0 → client 1's home returns
  `OwnershipRejected`.
- Plant spawned on host at tick 50 visible on both clients by ~T+5.
- `ConnectedRemotes.count == 2`; speed lock active.

### Verification

- `cargo test --bin civgame net` green; new unit tests:
  `lobby_select_start_rejects_too_close`,
  `lobby_bump_advances_to_starting_when_all_ready`,
  `apply_bootstrap_sets_pending_starts_primary`,
  `replicate_tile_overlays_emits_add_plant`.
- Phase 8.6 integration test green.
- Two-terminal manual: `cargo run -- --listen --bind 0.0.0.0:5000
  --player host` + `cargo run -- --connect 127.0.0.1:5000 --player
  joiner`; both see lobby, pick starts, ready, host starts; both
  transition into `Playing` rendering the same seed.
