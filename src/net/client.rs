//! Client-side Lightyear systems for Phase 2.
//!
//! Three responsibilities:
//!
//! 1. **Handshake** (`send_client_hello_system`): on `ClientConnectEvent`,
//!    ship `ClientHello { protocol_version, player_name }`.
//! 2. **Apply bootstrap** (`apply_bootstrap_snapshot_system`): drain
//!    `ClientReceiveMessage<BootstrapSnapshot>` + `FactionAssignment`,
//!    populate Calendar / ControlledFactions / PlayerFaction / tile
//!    overlay maps, spawn `NetId`-keyed stubs for every overlay entity
//!    so future per-tick replication can find them.
//! 3. **Send commands** (`send_command_frames_system`): drain
//!    `NetPlayerCommandEvent`, wrap in `NetCommandFrame`, send to server.
//!    Also handle `NetCommandAck` for UI feedback.

use bevy::prelude::*;
use lightyear::prelude::client::{
    ConnectEvent as ClientConnectEvent, ConnectionManager as ClientConnectionManager,
    DisconnectEvent as ClientDisconnectEvent,
};
use lightyear::prelude::ClientReceiveMessage;

use crate::game_state::{
    EconomyPreset, GameStartOptions, GameState, PendingStarts, PlayerStartSlot,
    RegenerateWorldRequest, StartSettlementMaturity, WorldSeed,
};
use crate::net::bootstrap::apply_bootstrap_snapshot;
use crate::net::cli::NetConfig;
use crate::net::protocol::{
    BootstrapSnapshot, ChunkOverlayDelta, ClientCameraFocus, ClientHello, CommandId,
    EntityKindWire, EntityRemoved, EntityStateDelta, EntityStateEntry, FactionAssignment,
    LobbyReject, LobbySnapshot, LobbyStartGame, NetCommandAck, NetCommandFrame,
    NetPlayerCommandEvent, TileOverlayOp, PROTOCOL_VERSION,
};
use crate::simulation::faction::Lifestyle;
use crate::simulation::technology::Era;
use crate::net::protocol_plugin::OrderedReliableChannel;
use crate::net_id::{NetId, NetIdMap, Networked};
use crate::simulation::construction::{
    BridgeMap, DamMap, DoorEntry, DoorMap, Wall, WallMap, WallMaterial,
};
use crate::simulation::faction::{ControlledFactions, PlayerFaction};
use crate::world::seasons::Calendar;
use crate::world::terrain::TILE_SIZE;
use crate::world::water_runtime::RuntimeWater;

/// Per-client outbound command sequencer. `CommandId(0)` is the LOCAL
/// sentinel; the live wire id starts at 1 and increments each `send`.
#[derive(Resource, Debug, Default)]
pub struct ClientCommandSequencer {
    next_id: u32,
}

impl ClientCommandSequencer {
    pub fn next(&mut self) -> CommandId {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        CommandId(self.next_id)
    }
}

/// Tracks the last `NetCommandAck` per command id so the UI / logs can
/// surface failures. Bounded; oldest entries evicted at `CAP`.
#[derive(Resource, Debug, Default)]
pub struct ClientAckLog {
    entries: Vec<NetCommandAck>,
}

impl ClientAckLog {
    pub const CAP: usize = 32;

    pub fn push(&mut self, ack: NetCommandAck) {
        if self.entries.len() >= Self::CAP {
            self.entries.remove(0);
        }
        self.entries.push(ack);
    }

    pub fn latest(&self) -> Option<&NetCommandAck> {
        self.entries.last()
    }

    pub fn iter(&self) -> impl Iterator<Item = &NetCommandAck> {
        self.entries.iter()
    }
}

/// Send `ClientHello` on connection establishment.
pub fn send_client_hello_system(
    mut events: EventReader<ClientConnectEvent>,
    mut conn_mgr: ResMut<ClientConnectionManager>,
    net_config: Res<NetConfig>,
) {
    for _ in events.read() {
        let hello = ClientHello {
            protocol_version: PROTOCOL_VERSION,
            player_name: net_config
                .player_name
                .clone()
                .unwrap_or_else(|| "Player".into()),
        };
        match conn_mgr.send_message::<OrderedReliableChannel, _>(&hello) {
            Ok(_) => info!("sent ClientHello (protocol v{})", PROTOCOL_VERSION),
            Err(err) => warn!("ClientHello send failed: {:?}", err),
        }
    }
}

/// Bundle the lobby-transition resources `apply_bootstrap_snapshot_system`
/// touches when it promotes the client into `Playing`. Bevy caps a system
/// at 16 parameters; folding these into one `SystemParam` keeps room for
/// the overlay-map mut-resources the snapshot apply path also needs.
#[derive(bevy::ecs::system::SystemParam)]
pub struct BootstrapStartParams<'w> {
    pub world_seed: ResMut<'w, WorldSeed>,
    pub regen: EventWriter<'w, RegenerateWorldRequest>,
    pub pending_starts: ResMut<'w, PendingStarts>,
    pub state: Res<'w, State<GameState>>,
    pub next_state: ResMut<'w, NextState<GameState>>,
}

/// Bundle the client-side tile-overlay maps mutated by
/// `apply_overlay_delta`. Folded into one SystemParam so callers stay
/// under Bevy's 16-arg ceiling.
#[derive(bevy::ecs::system::SystemParam)]
pub struct OverlayApplyMaps<'w> {
    pub wall_map: ResMut<'w, WallMap>,
    pub door_map: ResMut<'w, DoorMap>,
    pub bridge_map: ResMut<'w, BridgeMap>,
    pub dam_map: ResMut<'w, DamMap>,
    pub runtime_water: ResMut<'w, RuntimeWater>,
    pub plant_map: ResMut<'w, ReplicatedPlantMap>,
    pub structure_map: ResMut<'w, ReplicatedStructureMap>,
}

/// Drain `FactionAssignment` first (sets `PlayerFaction`), then any
/// `BootstrapSnapshot` (overwrites Calendar/maps), then `ChunkOverlayDelta`s.
/// `ClientDisconnectEvent` is a placeholder for reconnect handling.
#[allow(clippy::too_many_arguments)]
pub fn apply_bootstrap_snapshot_system(
    mut assignments: EventReader<ClientReceiveMessage<FactionAssignment>>,
    mut snapshots: EventReader<ClientReceiveMessage<BootstrapSnapshot>>,
    mut deltas: EventReader<ClientReceiveMessage<ChunkOverlayDelta>>,
    mut acks: EventReader<ClientReceiveMessage<NetCommandAck>>,
    mut ack_log: ResMut<ClientAckLog>,
    mut calendar: ResMut<Calendar>,
    mut player_faction: ResMut<PlayerFaction>,
    mut controlled: ResMut<ControlledFactions>,
    mut maps: OverlayApplyMaps,
    mut net_ids: ResMut<NetIdMap>,
    mut start: BootstrapStartParams,
    mut commands: Commands,
) {
    for ev in assignments.read() {
        let m = ev.message();
        info!(
            "FactionAssignment: faction {}, world_seed {:#x}",
            m.faction_id, m.world_seed
        );
        player_faction.faction_id = m.faction_id;
        controlled.add(m.faction_id);
        // World seed must match the server's so locally-regenerated
        // terrain / globe is deterministic. Fire `RegenerateWorldRequest`
        // so `spawn_world_system` rebuilds against the new seed.
        if start.world_seed.0 != m.world_seed {
            start.world_seed.0 = m.world_seed;
            start.regen.send(RegenerateWorldRequest);
        }
    }

    for ev in snapshots.read() {
        let snap = ev.message();
        // Spawn stubs for every overlay entity before applying — the
        // apply path needs `NetIdMap::entity_of` to succeed. Walls also
        // stamp a `Wall { owner_faction }` so the client-side
        // `fog_update_system` (Phase 3d) reads the same owner check as
        // the server via `has_vision_los`.
        for entry in &snap.overlay_tiles.walls {
            ensure_wall_stub(
                &mut commands,
                &mut net_ids,
                entry.entity_net_id,
                entry.owner_faction,
            );
        }
        for entry in &snap.overlay_tiles.doors {
            ensure_stub(&mut commands, &mut net_ids, entry.entity_net_id);
        }
        for entry in &snap.overlay_tiles.bridges {
            ensure_stub(&mut commands, &mut net_ids, entry.entity_net_id);
        }
        for entry in &snap.overlay_tiles.dams {
            ensure_stub(&mut commands, &mut net_ids, entry.entity_net_id);
        }

        apply_bootstrap_snapshot(
            snap,
            &mut calendar,
            &mut controlled,
            &mut maps.wall_map,
            &mut maps.door_map,
            &mut maps.bridge_map,
            &mut maps.dam_map,
            &mut maps.runtime_water,
            &net_ids,
        );
        info!(
            "bootstrap applied: tick {}, {} factions, {} settlements, {} walls",
            snap.server_tick,
            snap.factions.len(),
            snap.settlements.len(),
            snap.overlay_tiles.walls.len()
        );

        // Anchor `PendingStarts.primary_start` on the assigned faction's
        // home tile so the camera centres on the right place. This is the
        // canonical write path — `apply_lobby_start_game_system` ALSO sets
        // it from the lobby `slot_assignments`, but a reconnecting client
        // skips the lobby entirely and only sees `BootstrapSnapshot`.
        if let Some(faction) = snap
            .factions
            .iter()
            .find(|f| f.faction_id == player_faction.faction_id)
        {
            start.pending_starts.primary_start = Some(faction.home_tile);
        }

        // Transition the client into `Playing` if it's still showing the
        // lobby (i.e. lobby was bypassed via mid-game reconnect, or this
        // is the first bootstrap after `LobbyStartGame`). A repeat
        // bootstrap (resync) on an already-`Playing` client is a no-op.
        if !matches!(start.state.get(), GameState::Playing) {
            start.next_state.set(GameState::Playing);
        }
    }

    for ev in deltas.read() {
        let delta = ev.message();
        apply_overlay_delta(delta, &mut commands, &mut net_ids, &mut maps);
    }

    for ev in acks.read() {
        ack_log.push(ev.message().clone());
    }
}

fn ensure_stub(commands: &mut Commands, ids: &mut NetIdMap, net_id: NetId) {
    if ids.entity_of(net_id).is_some() {
        return;
    }
    let entity = commands.spawn(Networked(net_id)).id();
    ids.bind(entity, net_id);
}

/// Spawn (or upgrade) a wall stub that carries the wall's owner faction.
/// Material defaults to `Palisade` — the client-side `fog_update_system`
/// reads `owner_faction` only, so the placeholder material is harmless
/// (wall HP / destruction lives server-side and the client never decides
/// when a wall dies). Idempotent: a pre-existing stub gets the `Wall`
/// component inserted over its bare `Networked`-only state.
fn ensure_wall_stub(
    commands: &mut Commands,
    ids: &mut NetIdMap,
    net_id: NetId,
    owner_faction: Option<u32>,
) {
    let wall = Wall {
        material: WallMaterial::Palisade,
        owner_faction,
    };
    if let Some(entity) = ids.entity_of(net_id) {
        commands.entity(entity).insert(wall);
        return;
    }
    let entity = commands.spawn((Networked(net_id), wall)).id();
    ids.bind(entity, net_id);
}

fn apply_overlay_delta(
    delta: &ChunkOverlayDelta,
    commands: &mut Commands,
    ids: &mut NetIdMap,
    maps: &mut OverlayApplyMaps,
) {
    let wall_map = &mut maps.wall_map;
    let door_map = &mut maps.door_map;
    let bridge_map = &mut maps.bridge_map;
    let dam_map = &mut maps.dam_map;
    let runtime_water = &mut maps.runtime_water;
    let plant_map = &mut maps.plant_map;
    let structure_map = &mut maps.structure_map;
    for op in &delta.ops {
        match op {
            TileOverlayOp::AddWall {
                tile,
                entity_net_id,
                owner_faction,
            } => {
                ensure_wall_stub(commands, ids, *entity_net_id, *owner_faction);
                if let Some(entity) = ids.entity_of(*entity_net_id) {
                    wall_map.0.insert(*tile, entity);
                }
            }
            TileOverlayOp::RemoveWall { tile } => {
                wall_map.0.remove(tile);
            }
            TileOverlayOp::AddDoor {
                tile,
                entity_net_id,
                open,
                faction_id,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
                if let Some(entity) = ids.entity_of(*entity_net_id) {
                    door_map.0.insert(
                        *tile,
                        DoorEntry {
                            entity,
                            open: *open,
                            faction_id: *faction_id,
                        },
                    );
                }
            }
            TileOverlayOp::RemoveDoor { tile } => {
                door_map.0.remove(tile);
            }
            TileOverlayOp::SetDoorOpen { tile, open } => {
                if let Some(entry) = door_map.0.get_mut(tile) {
                    entry.open = *open;
                }
            }
            TileOverlayOp::AddBridge {
                tile,
                entity_net_id,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
                if let Some(entity) = ids.entity_of(*entity_net_id) {
                    bridge_map.0.insert(*tile, entity);
                }
            }
            TileOverlayOp::RemoveBridge { tile } => {
                bridge_map.0.remove(tile);
            }
            TileOverlayOp::AddDam {
                tile,
                entity_net_id,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
                if let Some(entity) = ids.entity_of(*entity_net_id) {
                    dam_map.0.insert(*tile, entity);
                }
            }
            TileOverlayOp::RemoveDam { tile } => {
                dam_map.0.remove(tile);
            }
            TileOverlayOp::SetRuntimeWater { tile, cell } => {
                runtime_water.cells.insert(*tile, *cell);
            }
            TileOverlayOp::ClearRuntimeWater { tile } => {
                runtime_water.cells.remove(tile);
            }
            // Phase 7 — plant + structure ops. v1 stub-only: bind a
            // `Networked` entity for the new server-side id so future
            // requests (inspector summary, etc.) can resolve it; the
            // client doesn't yet maintain a `PlantMap` / structure
            // index off the wire. Once those indexes land, populate
            // them here the same way wall/door already do.
            TileOverlayOp::AddPlant {
                tile,
                entity_net_id,
                kind: _,
                stage: _,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
                if let Some(entity) = ids.entity_of(*entity_net_id) {
                    plant_map.0.insert(*tile, entity);
                }
            }
            TileOverlayOp::RemovePlant { tile } => {
                plant_map.0.remove(tile);
            }
            TileOverlayOp::PlantStageChange { tile: _, stage: _ } => {
                // Stage flips: stub already exists; rendering picks up
                // when per-stage replication lands.
            }
            TileOverlayOp::AddStructure {
                tile,
                entity_net_id,
                kind: _,
                owner_faction: _,
                label_id: _,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
                if let Some(entity) = ids.entity_of(*entity_net_id) {
                    structure_map.0.insert(*tile, entity);
                }
            }
            TileOverlayOp::RemoveStructure { tile } => {
                structure_map.0.remove(tile);
            }
        }
    }
}

/// Translate `NetPlayerCommandEvent` (the in-process channel CommandSender
/// writes to) into a `NetCommandFrame` and ship to server. Local mode's
/// loopback consumes the same event before this runs, so on a Client App
/// this is the only consumer.
pub fn send_command_frames_system(
    mut events: EventReader<NetPlayerCommandEvent>,
    mut conn_mgr: ResMut<ClientConnectionManager>,
    mut seq: ResMut<ClientCommandSequencer>,
) {
    for ev in events.read() {
        let frame = NetCommandFrame {
            command_id: seq.next(),
            sender_faction_id: ev.sender_faction_id,
            actors: ev.actors.clone(),
            command: ev.command.clone(),
        };
        if let Err(err) = conn_mgr.send_message::<OrderedReliableChannel, _>(&frame) {
            warn!("send NetCommandFrame failed: {:?}", err);
        }
    }
}

/// How many Update ticks between camera-focus reports. 30 ≈ 500ms at
/// 60Hz Update — matches the server's `INTEREST_REBUILD_INTERVAL_UPDATES`
/// so a freshly-reported focus lands in the next interest rebuild without
/// drift. Each report is suppressed unless the focus chunk has changed
/// since the last successful send.
pub const CAMERA_FOCUS_SEND_INTERVAL_UPDATES: u32 = 30;

/// Periodically report the camera's current world-space focus tile to the
/// server. Only fires when the focus chunk has shifted (chunk-level
/// granularity is what the server cares about anyway), keeping wire
/// chatter minimal.
pub fn send_camera_focus_system(
    mut tick: Local<u32>,
    mut last_sent_chunk: Local<Option<(i32, i32)>>,
    mut conn_mgr: Option<ResMut<ClientConnectionManager>>,
    camera_q: Query<&Transform, With<bevy::prelude::Camera2d>>,
) {
    *tick = tick.wrapping_add(1);
    if *tick % CAMERA_FOCUS_SEND_INTERVAL_UPDATES != 0 {
        return;
    }
    let Some(conn_mgr) = conn_mgr.as_mut() else {
        return;
    };
    let Ok(tf) = camera_q.get_single() else {
        return;
    };
    let tile = (
        (tf.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32,
        (tf.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32,
    );
    let chunk_size = crate::world::chunk::CHUNK_SIZE as i32;
    let chunk = (tile.0.div_euclid(chunk_size), tile.1.div_euclid(chunk_size));
    if *last_sent_chunk == Some(chunk) {
        return;
    }
    let msg = ClientCameraFocus { tile };
    if conn_mgr
        .send_message::<OrderedReliableChannel, _>(&msg)
        .is_ok()
    {
        *last_sent_chunk = Some(chunk);
    }
}

/// Drain client disconnect events; logging only for now. Phase 2e
/// follow-up: trigger reconnect / fall to local AI.
pub fn observe_disconnect_system(mut events: EventReader<ClientDisconnectEvent>) {
    for ev in events.read() {
        info!("client disconnected: {:?}", ev);
    }
}

// ============================================================================
// Phase 3a — client-side EntityStateDelta apply
// ============================================================================

/// Tag inserted on every client-side replicated entity stub so renderer +
/// HUD systems can branch on the kind without re-querying the wire enum.
/// Mirrors `EntityKindWire` but stays out of the wire protocol so adding a
/// component-side variant doesn't bump `PROTOCOL_VERSION`.
#[derive(bevy::prelude::Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicatedEntityKind {
    Person,
    Animal,
    Vehicle,
}

/// Client-side tile→`Entity` index for replicated plants. Mirrors the
/// server's `PlantMap` over the wire. Populated by
/// `apply_overlay_delta`'s `AddPlant` branch and torn down by
/// `RemovePlant`.
#[derive(bevy::prelude::Resource, Default, Debug)]
pub struct ReplicatedPlantMap(pub ahash::AHashMap<(i32, i32), bevy::prelude::Entity>);

/// Client-side tile→`Entity` index for replicated structures.
#[derive(bevy::prelude::Resource, Default, Debug)]
pub struct ReplicatedStructureMap(pub ahash::AHashMap<(i32, i32), bevy::prelude::Entity>);

/// Replicated-entity bookkeeping carried on every stub. Stores the latest
/// server tick that touched the stub so a future LOD-aware draw can age
/// out stale agents (client → server hopefully always within seconds).
#[derive(bevy::prelude::Component, Debug, Clone, Copy)]
pub struct ReplicatedEntity {
    pub kind: ReplicatedEntityKind,
    pub last_tick: u64,
    pub tile: (i32, i32),
    pub z: i8,
    pub facing: u8,
    pub health_current: u16,
    pub health_max: u16,
    pub faction_id: u32,
}

/// Drain `ClientReceiveMessage<EntityStateDelta>` and apply each entry to
/// its `Networked`-stub entity, auto-spawning a stub on first sighting.
///
/// v1 just updates `Transform` + the `ReplicatedEntity` bookkeeping. The
/// client renderer (Phase 3d) will read `ReplicatedEntityKind` to attach
/// the right sprite tree on `Added<ReplicatedEntityKind>`.
pub fn apply_entity_state_delta_system(
    mut deltas: EventReader<ClientReceiveMessage<EntityStateDelta>>,
    mut net_ids: ResMut<NetIdMap>,
    mut commands: Commands,
    mut stubs: Query<(&mut Transform, &mut ReplicatedEntity)>,
) {
    for ev in deltas.read() {
        let delta = ev.message();
        for entry in &delta.entries {
            let entity = ensure_replicated_stub(&mut commands, &mut net_ids, entry);
            // The stub query may not see the freshly-spawned entity until
            // next frame; bookkeeping for it lands in the spawn bundle.
            if let Ok((mut transform, mut rep)) = stubs.get_mut(entity) {
                transform.translation.x = entry.tile.0 as f32 * TILE_SIZE;
                transform.translation.y = entry.tile.1 as f32 * TILE_SIZE;
                rep.last_tick = delta.server_tick;
                rep.tile = entry.tile;
                rep.z = entry.z;
                rep.facing = entry.facing;
                rep.health_current = entry.health_current;
                rep.health_max = entry.health_max;
                rep.faction_id = entry.faction_id;
            }
        }
    }
}

fn ensure_replicated_stub(
    commands: &mut Commands,
    ids: &mut NetIdMap,
    entry: &EntityStateEntry,
) -> bevy::prelude::Entity {
    if let Some(existing) = ids.entity_of(entry.net_id) {
        // First sighting from a tile-overlay path (wall stub already
        // spawned with bare `Networked` only) — upgrade to a replicated
        // stub by inserting the bookkeeping. Idempotent re-inserts are
        // fine here because we only ever Add, not Replace.
        commands.entity(existing).insert((
            replicated_entity_kind(entry.kind),
            ReplicatedEntity {
                kind: replicated_entity_kind(entry.kind),
                last_tick: 0,
                tile: entry.tile,
                z: entry.z,
                facing: entry.facing,
                health_current: entry.health_current,
                health_max: entry.health_max,
                faction_id: entry.faction_id,
            },
            Transform::from_xyz(
                entry.tile.0 as f32 * TILE_SIZE,
                entry.tile.1 as f32 * TILE_SIZE,
                0.0,
            ),
        ));
        return existing;
    }
    let entity = commands
        .spawn((
            Networked(entry.net_id),
            replicated_entity_kind(entry.kind),
            ReplicatedEntity {
                kind: replicated_entity_kind(entry.kind),
                last_tick: 0,
                tile: entry.tile,
                z: entry.z,
                facing: entry.facing,
                health_current: entry.health_current,
                health_max: entry.health_max,
                faction_id: entry.faction_id,
            },
            Transform::from_xyz(
                entry.tile.0 as f32 * TILE_SIZE,
                entry.tile.1 as f32 * TILE_SIZE,
                0.0,
            ),
        ))
        .id();
    ids.bind(entity, entry.net_id);
    entity
}

/// Drain `EntityRemoved` and despawn the matching stubs. Unresolvable
/// `NetId`s (entity already gone, or stub never spawned because the client
/// missed the corresponding `EntityStateDelta`) are silently ignored.
pub fn apply_entity_removed_system(
    mut events: EventReader<ClientReceiveMessage<EntityRemoved>>,
    net_ids: Res<NetIdMap>,
    mut commands: Commands,
) {
    for ev in events.read() {
        for id in &ev.message().net_ids {
            if let Some(entity) = net_ids.entity_of(*id) {
                commands.entity(entity).despawn_recursive();
            }
        }
    }
}

fn replicated_entity_kind(wire: EntityKindWire) -> ReplicatedEntityKind {
    match wire {
        EntityKindWire::Person => ReplicatedEntityKind::Person,
        EntityKindWire::Animal(_) => ReplicatedEntityKind::Animal,
        EntityKindWire::Vehicle => ReplicatedEntityKind::Vehicle,
    }
}

// ============================================================================
// Phase 8.2 — Client lobby appliers
// ============================================================================

/// Drain `LobbySnapshot` and mirror onto `LobbyUiState` so the lobby UI
/// renders the live server-side player roster. Also reads back the
/// host-side config (world seed / era / economy / maturity) into the
/// client's `WorldSeed` + `GameStartOptions` so the right values show up
/// in the panel even when the host is mid-edit.
#[allow(clippy::too_many_arguments)]
pub fn apply_lobby_snapshot_system(
    mut snapshots: EventReader<ClientReceiveMessage<LobbySnapshot>>,
    mut ui_state: Option<ResMut<crate::ui::lobby::LobbyUiState>>,
    mut world_seed: ResMut<WorldSeed>,
    mut options: ResMut<GameStartOptions>,
) {
    for ev in snapshots.read() {
        let snap = ev.message();
        if let Some(ui) = ui_state.as_deref_mut() {
            ui.remote_slots = snap.slots.clone();
        }
        world_seed.0 = snap.world_seed;
        options.era = match snap.era_index {
            0 => Era::Paleolithic,
            1 => Era::Mesolithic,
            2 => Era::Neolithic,
            3 => Era::Chalcolithic,
            _ => Era::BronzeAge,
        };
        options.economy = match snap.economy_index {
            0 => EconomyPreset::Subsistence,
            1 => EconomyPreset::Mixed,
            _ => EconomyPreset::Market,
        };
        options.maturity = match snap.maturity_index {
            0 => StartSettlementMaturity::Founder,
            1 => StartSettlementMaturity::Established,
            _ => StartSettlementMaturity::Developed,
        };
    }
}

/// Drain `LobbyStartGame`. Build `PendingStarts` from the slot
/// assignments — the slot whose `client_id` matches our derived id
/// becomes the camera anchor (`primary_start`). Then flip into
/// `GameState::Playing`. World terrain comes up via the existing
/// `BootstrapSnapshot` path (Phase 8.3 sets seed / fires
/// `RegenerateWorldRequest`).
pub fn apply_lobby_start_game_system(
    mut events: EventReader<ClientReceiveMessage<LobbyStartGame>>,
    net_config: Res<NetConfig>,
    mut pending_starts: ResMut<PendingStarts>,
    mut world_seed: ResMut<WorldSeed>,
    state: Res<State<GameState>>,
    mut next_state: ResMut<NextState<GameState>>,
) {
    for ev in events.read() {
        let msg = ev.message();
        world_seed.0 = msg.world_seed;

        // Compute our local client id the same way `install_client` did
        // (`derive_client_id(player_name)`) so the matching slot lands as
        // `primary_start`. If the host UI emitted us through `LocalLobbyCommand`,
        // our client id is `HOST_SERVER_LOCAL_CLIENT_ID` instead.
        let local_name = net_config
            .player_name
            .clone()
            .unwrap_or_else(|| "Player".into());
        let local_client_id = crate::net::derive_client_id(&local_name);

        let slots: Vec<PlayerStartSlot> = msg
            .slot_assignments
            .iter()
            .map(|a| PlayerStartSlot {
                slot_id: a.slot_id,
                player_name: String::new(),
                client_id: a.client_id,
                megachunk: Some(a.megachunk),
                lifestyle: Lifestyle::Settled,
                ready: true,
                faction_id: Some(a.faction_id),
            })
            .collect();
        let primary_start = slots
            .iter()
            .find(|s| s.client_id == local_client_id)
            .and_then(|s| s.megachunk.map(megachunk_center_tile_client));
        pending_starts.slots = slots;
        if primary_start.is_some() {
            pending_starts.primary_start = primary_start;
        }

        if !matches!(state.get(), GameState::Playing) {
            next_state.set(GameState::Playing);
        }
    }
}

fn megachunk_center_tile_client(megachunk: (i32, i32)) -> (i32, i32) {
    crate::simulation::region::MegaChunkCoord::center_tile(megachunk.0, megachunk.1)
}

/// Drain `LobbyReject` and log it. Future-Phase: surface to the UI as a
/// transient toast.
pub fn apply_lobby_reject_system(
    mut events: EventReader<ClientReceiveMessage<LobbyReject>>,
) {
    for ev in events.read() {
        let r = ev.message();
        warn!("lobby reject: {:?} ({:?})", r.reason, r.detail);
    }
}
