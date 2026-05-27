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

use crate::net::bootstrap::apply_bootstrap_snapshot;
use crate::net::cli::NetConfig;
use crate::net::protocol::{
    BootstrapSnapshot, ChunkOverlayDelta, ClientCameraFocus, ClientHello, CommandId,
    EntityKindWire, EntityRemoved, EntityStateDelta, EntityStateEntry, FactionAssignment,
    NetCommandAck, NetCommandFrame, NetPlayerCommandEvent, TileOverlayOp, PROTOCOL_VERSION,
};
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
    mut wall_map: ResMut<WallMap>,
    mut door_map: ResMut<DoorMap>,
    mut bridge_map: ResMut<BridgeMap>,
    mut dam_map: ResMut<DamMap>,
    mut runtime_water: ResMut<RuntimeWater>,
    mut net_ids: ResMut<NetIdMap>,
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
            &mut wall_map,
            &mut door_map,
            &mut bridge_map,
            &mut dam_map,
            &mut runtime_water,
            &net_ids,
        );
        info!(
            "bootstrap applied: tick {}, {} factions, {} settlements, {} walls",
            snap.server_tick,
            snap.factions.len(),
            snap.settlements.len(),
            snap.overlay_tiles.walls.len()
        );
    }

    for ev in deltas.read() {
        let delta = ev.message();
        apply_overlay_delta(
            delta,
            &mut commands,
            &mut net_ids,
            &mut wall_map,
            &mut door_map,
            &mut bridge_map,
            &mut dam_map,
            &mut runtime_water,
        );
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
    wall_map: &mut WallMap,
    door_map: &mut DoorMap,
    bridge_map: &mut BridgeMap,
    dam_map: &mut DamMap,
    runtime_water: &mut RuntimeWater,
) {
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
                tile: _,
                entity_net_id,
                kind: _,
                stage: _,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
            }
            TileOverlayOp::RemovePlant { tile: _ } => {
                // No client-side PlantMap yet — the matching stub will
                // GC via `EntityRemoved` separately.
            }
            TileOverlayOp::PlantStageChange { tile: _, stage: _ } => {
                // Stage flips: stub already exists; rendering picks up
                // when stage replication lands.
            }
            TileOverlayOp::AddStructure {
                tile: _,
                entity_net_id,
                kind: _,
                owner_faction: _,
                label_id: _,
            } => {
                ensure_stub(commands, ids, *entity_net_id);
            }
            TileOverlayOp::RemoveStructure { tile: _ } => {}
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
