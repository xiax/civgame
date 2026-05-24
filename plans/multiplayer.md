# Server-Authoritative Multiplayer For Player Factions

## Summary
- Add internet multiplayer as a **server-authoritative simulation**: one server runs the full CivGame ECS sim, each client controls one assigned `FactionData` entry, and clients receive only the world state their faction should observe.
- Use **Lightyear pinned to the Bevy 0.15-compatible line**, preferably `lightyear = "0.19.1"` with fallback to `0.19.0` if resolution/docs force it. Lightyear’s current docs describe server-authoritative Bevy networking, input/message handling, replication, interpolation, and interest management; its compatibility table shows current latest targets newer Bevy, while `0.18-0.19` targets Bevy 0.15.
- First milestone is playable online faction control, not full deterministic lockstep. The server coordinates thousands of agents; clients send compact intent and receive filtered snapshots.

## Key Changes
- Add a new `net` module with:
  - `NetMode::{Offline, ListenServer, DedicatedServer, Client}` selected from CLI flags like `--server`, `--listen`, `--connect <addr>`, `--player <name>`.
  - Shared Lightyear protocol registration for messages, replicated components, and channels.
  - Stable IDs: `NetEntityId`, `NetFactionId`, `NetCommandId`, plus server-side maps between stable IDs and Bevy `Entity`.
- Replace network-facing use of raw Bevy entities in player orders with serializable command DTOs:
  - `NetPlayerCommand { faction_id, command_id, actors, command }`
  - actor references use stable IDs or server-owned selection groups, never client-local `Entity`.
  - server validates faction ownership, resolves actors to local entities, then emits the existing `PlayerCommandEvent`.
- Generalize current single-player ownership:
  - Keep `PlayerFaction` for local/offline UI, but add `ControlledFactions`/`ClientFactionAssignments` so each connection maps to exactly one top-level faction.
  - Make faction-level commands currently hardwired to `player_faction.faction_id` apply to the authenticated/assigned faction on the server.
- Add replicated “view model” components instead of replicating full sim internals:
  - high-rate selected/controlled units: stable id, faction id, tile/z, transform, current task, command status, health, carried item summary.
  - medium-rate visible entities: unit/animal/item/structure summary, tile/z, sprite kind/facing, hostile/neutral/friendly relation.
  - low-rate faction/settlement summaries: stockpiles, population, alerts, market/civic state.
  - runtime tile diffs: roads, walls, doors, blueprints, water/bridge/dam/well deltas, keyed by tile/z.
- Implement interest management with Lightyear rooms:
  - rooms keyed by chunk or mega-chunk around each client’s faction vision/camera/settled regions.
  - selected actors and owned faction summaries are always relevant to their controlling client.
  - fog-of-war-hidden enemy units are not replicated; distant factions degrade to abstract summaries.
- Networking behavior:
  - client UI remains responsive locally for menus/selection, but all game-changing orders go to the server.
  - server returns command acknowledgements/failures using `NetCommandId`.
  - movement/combat/building simulation is not predicted in v1; use interpolation for replicated transforms/status.
  - optional later phase can add prediction for direct military movement only.

## Public Interfaces
- Add serializable network command/event types using `serde`:
  - `NetCommandPayload` mirroring safe `PlayerCommand` variants.
  - `NetCommandAck { command_id, status, reason }`.
  - `ClientHello { player_name, protocol_version }`, `FactionAssignment { faction_id }`.
  - `ChunkDeltaBatch`, `EntitySnapshot`, `FactionSnapshot`.
- Add CLI:
  - `cargo run -- --server --bind 0.0.0.0:5000`
  - `cargo run -- --listen --bind 0.0.0.0:5000`
  - `cargo run -- --connect host:5000`
  - existing `cargo run` stays offline single-player.
- Add feature gate:
  - `multiplayer` enables Lightyear and networking code.
  - offline builds continue to compile without starting networking.

## Test Plan
- `cargo check --features multiplayer`
- `cargo test --bin civgame --features multiplayer`
- Unit tests:
  - stable ID allocation/removal does not reuse live IDs.
  - network command DTO converts to `PlayerCommandEvent` only for authorized faction actors.
  - invalid actor/faction combinations are rejected.
  - entity references in command payloads resolve through server maps, not raw client `Entity`.
- Integration-style Bevy app tests:
  - spawn two factions, assign two mock clients, send commands for each, verify only owned actors receive `Commanded`.
  - verify faction-level vehicle/craft/camp commands apply to the client’s assigned faction.
  - verify interest rooms include nearby visible entities and exclude fog-hidden enemy units.
  - verify runtime tile diffs reproduce roads/walls/blueprints on a lightweight client world.
- Manual smoke test:
  - run dedicated server plus two local clients.
  - each client controls a different faction, issues move/build/muster orders, sees command acks, sees nearby visible agents update, and cannot command the other faction.

## Assumptions
- We will use Lightyear, not lockstep. Lightyear docs describe a server-authoritative client-server architecture and support messages, replication, interpolation, input handling, rooms-based interest management, and bandwidth controls: [Lightyear book](https://cbournhonesque.github.io/lightyear/book/), [interest management](https://cbournhonesque.github.io/lightyear/book/concepts/advanced_replication/interest_management.html), [docs.rs 0.19.0](https://docs.rs/crate/lightyear/0.19.0).
- The repo stays on Bevy 0.15 for this work, so we pin the compatible Lightyear line instead of upgrading Bevy.
- v1 does not support joining an already-running world from scratch without a bootstrap snapshot. The first implementation will support new clients by sending faction assignment, seed/config, relevant entity snapshots, and runtime tile diffs for the client’s current interest area.
- Security model is pragmatic: server validates faction ownership and visibility, but authentication is a simple player/session token until a later account/lobby layer exists.
