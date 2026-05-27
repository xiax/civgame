//! Server-side Lightyear systems for Phase 2.
//!
//! Three responsibilities:
//!
//! 1. **Accept connections** (`accept_connections_system`): on
//!    Lightyear `ServerConnectEvent`, allocate an unowned `FactionData`,
//!    add it to `ControlledFactions`, send `FactionAssignment` +
//!    `BootstrapSnapshot` to that client.
//! 2. **Replicate tile overlays** (`replicate_tile_overlays_system`):
//!    drain `TileChangedEvent`s, coalesce per chunk, broadcast
//!    `ChunkOverlayDelta` to all connected clients. The client applies
//!    via the same `apply_*_snapshot` machinery used at bootstrap.
//! 3. **Receive commands** (`receive_command_frames_system`): drain
//!    `ServerReceiveMessage<NetCommandFrame>`, validate the claimed
//!    `sender_faction_id` against the connection's authorized faction,
//!    re-emit as `NetPlayerCommandEvent` for `command_loopback_system`,
//!    and reply with `NetCommandAck`.
//!
//! Disconnect handling lives in `mod.rs` (`apply_disconnect_policy_system`)
//! since it touches `ControlledFactions` which is shared with the
//! single-player path.

use bevy::prelude::*;
use lightyear::connection::id::ClientId;
use lightyear::prelude::server::{
    ConnectEvent as ServerConnectEvent, ConnectionManager as ServerConnectionManager,
    DisconnectEvent as ServerDisconnectEvent,
};
use lightyear::prelude::{MessageSend, NetworkTarget, ServerReceiveMessage};

use crate::net::bootstrap::{build_bootstrap_snapshot, compute_interest_chunks, INTEREST_RADIUS_CHUNKS};
use crate::net::protocol::{
    AnimalSpeciesWire, BootstrapSnapshot, ChunkOverlayDelta, ClientCameraFocus, EntityKindWire,
    EntityRemoved, EntityStateDelta, EntityStateEntry, FactionAssignment, NetCommandAck,
    NetCommandAckStatus, NetCommandFrame, NetPlayerCommandEvent, TileOverlayOp,
};
use crate::net::protocol_plugin::OrderedReliableChannel;
use crate::net_id::{Networked, NetworkedRemovedEvent};
use crate::rendering::entity_sprites::FacingDirection;
use crate::simulation::animals::{Cat, Cow, Deer, Horse, Pig, Wolf};
use crate::simulation::combat::Health;
use crate::simulation::construction::{BridgeMap, DamMap, Door, DoorMap, WallMap};
use crate::simulation::faction::{ControlledFactions, FactionMember, FactionRegistry, HuntOrder};
use crate::simulation::person::{Drafted, Person, PersonAI};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{Settlement, SettlementMap};
use crate::simulation::vehicle::{Vehicle, VehicleHealth};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::seasons::Calendar;
use crate::world::terrain::TILE_SIZE;
use crate::world::water_runtime::RuntimeWater;
use crate::game_state::WorldSeed;
use crate::net::bootstrap::tile_to_chunk_coord;

/// Per-connection authority record. The server keeps one of these for
/// every connected client; when a `NetCommandFrame` arrives, the receiver
/// looks up the connection's `assigned_faction` and rejects any frame
/// whose `sender_faction_id` doesn't match.
#[derive(Resource, Default, Debug, Clone)]
pub struct ServerConnections {
    pub by_client: ahash::AHashMap<ClientId, ConnectionState>,
}

/// LOD-style classifier for one chunk within a client's interest frame.
/// Drives per-tier entity-rep cadence: `Owned` ships every send, `Neighbour`
/// every other, `Far` every tenth. Tile-overlay deltas ignore tiers
/// (sparse + reliable; the bandwidth doesn't justify gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterestTier {
    /// Chunks containing the assigned faction's home or one of its owned
    /// settlement market tiles, plus the immediate 1-chunk ring. This is
    /// the player's primary view — full rate.
    Owned,
    /// Within `NEIGHBOUR_TIER_RADIUS` chunks of any Owned chunk — the
    /// player might glance here often (caravan routes, encroaching
    /// rivals); needs decent fidelity but not full rate.
    Neighbour,
    /// Outer ring, still within `INTEREST_RADIUS_CHUNKS`. Replicates at
    /// trickle rate so the player sees something rather than nothing.
    Far,
}

/// Chunks-from-Owned for the `Neighbour` ring. `Far` is anything within
/// `INTEREST_RADIUS_CHUNKS` but past this radius.
pub const NEIGHBOUR_TIER_RADIUS: i32 = 2;

/// Per-tier send divisor — entity rep ships a chunk on send `n` iff
/// `n % cadence == 0`. Owned every tick, Neighbour every other, Far every
/// tenth. Tile-overlay deltas don't consult this.
pub const fn tier_cadence(tier: InterestTier) -> u32 {
    match tier {
        InterestTier::Owned => 1,
        InterestTier::Neighbour => 2,
        InterestTier::Far => 10,
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionState {
    pub assigned_faction: u32,
    /// `ClientHello.player_name` recorded at faction-assignment time.
    /// Used by `record_disconnections_system` to stash a
    /// `PendingReconnect` entry so a returning client with the same
    /// name reclaims the same faction within the grace window.
    pub player_name: String,
    /// Chunks this client cares about right now, each tagged with its
    /// `InterestTier`. Rebuilt by `compute_interest_system` every
    /// `INTEREST_REBUILD_INTERVAL_UPDATES`. Per-tick replicators (tile +
    /// entity) gate per-chunk sends on membership here; chunks no one is
    /// interested in skip the wire entirely.
    pub interest_chunks: ahash::AHashMap<(i32, i32), InterestTier>,
    /// Last chunk this client reported its camera was focused on (via
    /// `ClientCameraFocus`). `compute_interest_system` folds this in as
    /// an additional `Owned`-tier anchor so scouting expeditions outside
    /// the faction's settlement ring still pull live replication.
    pub camera_focus_chunk: Option<(i32, i32)>,
}

impl ServerConnections {
    pub fn faction_for(&self, client: ClientId) -> Option<u32> {
        self.by_client.get(&client).map(|s| s.assigned_faction)
    }

    /// Collect every client that has `chunk` in their interest frame, at
    /// any tier. Used by tile-overlay replication (cheap + reliable, so
    /// no rate tiering).
    pub fn clients_interested_in(&self, chunk: (i32, i32)) -> Vec<ClientId> {
        self.by_client
            .iter()
            .filter_map(|(&client, state)| {
                if state.interest_chunks.contains_key(&chunk) {
                    Some(client)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tier-aware recipient list for an entity-rep batch on chunk `chunk`
    /// at monotonic send index `send_index`. A client is included only
    /// when `send_index % tier_cadence(tier) == 0`. Empty result skips
    /// the wire entirely.
    pub fn clients_for_entity_chunk(
        &self,
        chunk: (i32, i32),
        send_index: u32,
    ) -> Vec<ClientId> {
        self.by_client
            .iter()
            .filter_map(|(&client, state)| {
                let tier = state.interest_chunks.get(&chunk)?;
                if send_index % tier_cadence(*tier) == 0 {
                    Some(client)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Log fresh client connections. Faction allocation + bootstrap snapshot
/// now wait for the corresponding `ClientHello` (carries `player_name` —
/// load-bearing for reconnect-restoration). A client that connects but
/// never says hello stays unowned; the upstream Lightyear state machine
/// times it out.
pub fn accept_connections_system(mut connects: EventReader<ServerConnectEvent>) {
    for ev in connects.read() {
        info!("client {:?} connected; awaiting ClientHello", ev.client_id);
    }
}

/// Drain `ClientHello`s, allocate (or reclaim) a faction, and ship the
/// `FactionAssignment` + `BootstrapSnapshot`. Reconnect path:
/// `PendingReconnect.take(&player_name)` returns `Some(entry)` when a
/// disconnected client with this name is still inside the grace window
/// (`RECONNECT_GRACE_TICKS`), in which case the same `faction_id` is
/// handed back instead of allocating a fresh one. The pending entry is
/// consumed.
/// Bundle the read-only overlay maps + queries that `handle_client_hello_system`
/// needs to build a `BootstrapSnapshot`. Bevy caps a system at 16 parameters;
/// folding these into one `SystemParam` keeps room for the live mut-resources
/// the hello path also touches.
#[derive(bevy::ecs::system::SystemParam)]
pub struct BootstrapParams<'w, 's> {
    pub wall_map: Res<'w, WallMap>,
    pub door_map: Res<'w, DoorMap>,
    pub bridge_map: Res<'w, BridgeMap>,
    pub dam_map: Res<'w, DamMap>,
    pub runtime_water: Res<'w, RuntimeWater>,
    pub networked_q: Query<'w, 's, &'static Networked>,
    pub wall_component_q: Query<'w, 's, &'static crate::simulation::construction::Wall>,
}

#[allow(clippy::too_many_arguments)]
pub fn handle_client_hello_system(
    mut hellos: EventReader<lightyear::prelude::ServerReceiveMessage<crate::net::protocol::ClientHello>>,
    mut server_conns: ResMut<ServerConnections>,
    mut controlled: ResMut<ControlledFactions>,
    mut pending_reconnect: ResMut<PendingReconnect>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    world_seed: Res<WorldSeed>,
    calendar: Res<Calendar>,
    factions: Res<FactionRegistry>,
    federation_map: Res<crate::simulation::federation::FederationMap>,
    settlement_map: Res<SettlementMap>,
    settlement_q: Query<&Settlement>,
    bootstrap: BootstrapParams,
) {
    for ev in hellos.read() {
        let client = ev.from;
        let hello = ev.message();

        if hello.protocol_version != crate::net::protocol::PROTOCOL_VERSION {
            warn!(
                "client {:?} ClientHello protocol v{} != server v{}; ignoring",
                client,
                hello.protocol_version,
                crate::net::protocol::PROTOCOL_VERSION
            );
            continue;
        }

        // Reclaim if this player_name has a live PendingReconnect entry.
        let (faction_id, reclaimed) = if let Some(entry) =
            pending_reconnect.take(&hello.player_name)
        {
            (entry.faction_id, true)
        } else {
            let Some(id) = allocate_free_faction(&server_conns, &factions, &controlled) else {
                warn!(
                    "no free faction for client {:?} (name={:?}); refusing assignment",
                    client, hello.player_name
                );
                continue;
            };
            (id, false)
        };

        server_conns.by_client.insert(
            client,
            ConnectionState {
                assigned_faction: faction_id,
                player_name: hello.player_name.clone(),
                interest_chunks: Default::default(),
                camera_focus_chunk: None,
            },
        );
        controlled.add(faction_id);

        let assignment = FactionAssignment {
            faction_id,
            world_seed: world_seed.0,
        };
        if let Err(err) = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &assignment,
            NetworkTarget::Single(client),
        ) {
            warn!("send FactionAssignment to {:?} failed: {:?}", client, err);
            continue;
        }

        let snapshot = build_bootstrap_snapshot(
            0, // TODO: thread real server tick from TickManager
            &[faction_id],
            &calendar,
            &factions,
            &settlement_map,
            &settlement_q,
            &bootstrap.wall_map,
            &bootstrap.door_map,
            &bootstrap.bridge_map,
            &bootstrap.dam_map,
            &bootstrap.runtime_water,
            &bootstrap.networked_q,
            &bootstrap.wall_component_q,
            &federation_map,
        );
        if let Err(err) = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &snapshot,
            NetworkTarget::Single(client),
        ) {
            warn!("send BootstrapSnapshot to {:?} failed: {:?}", client, err);
            continue;
        }

        if reclaimed {
            info!(
                "client {:?} ({}) reclaimed faction {} via PendingReconnect",
                client, hello.player_name, faction_id
            );
        } else {
            info!(
                "client {:?} ({}) → faction {} (bootstrap: {} walls, {} doors)",
                client,
                hello.player_name,
                faction_id,
                snapshot.overlay_tiles.walls.len(),
                snapshot.overlay_tiles.doors.len()
            );
        }
    }
}

/// Pick the lowest-id materialised `FactionData` not already in
/// `ControlledFactions` and not already handed to another connection.
/// `None` means every faction is taken.
fn allocate_free_faction(
    conns: &ServerConnections,
    factions: &FactionRegistry,
    controlled: &ControlledFactions,
) -> Option<u32> {
    let already_assigned: ahash::AHashSet<u32> = conns
        .by_client
        .values()
        .map(|s| s.assigned_faction)
        .collect();
    let mut ids: Vec<u32> = factions
        .factions
        .iter()
        .filter(|(_, data)| data.materialized && data.parent_faction.is_none())
        .map(|(&id, _)| id)
        .filter(|id| !already_assigned.contains(id) && !controlled.contains(*id))
        .collect();
    ids.sort();
    ids.first().copied()
}

/// On client disconnect, queue the faction for the policy stage
/// (`apply_disconnect_policy_system` in `mod.rs` honours `--on-disconnect`)
/// AND stash a `PendingReconnect` entry so a returning client with the
/// same `player_name` can reclaim the faction inside the grace window.
pub fn record_disconnections_system(
    mut disconnects: EventReader<ServerDisconnectEvent>,
    mut server_conns: ResMut<ServerConnections>,
    mut pending: ResMut<PendingDisconnects>,
    mut pending_reconnect: ResMut<PendingReconnect>,
    clock: Res<SimClock>,
) {
    for ev in disconnects.read() {
        if let Some(state) = server_conns.by_client.remove(&ev.client_id) {
            pending.0.push(state.assigned_faction);
            // Stash for reconnect — even when the policy is DropFaction,
            // a returning client gets one chance to come back inside the
            // grace window. After expiry, the entry GCs and the faction
            // is available for fresh allocation.
            if !state.player_name.is_empty() {
                pending_reconnect.stash(
                    state.player_name.clone(),
                    ReconnectEntry {
                        faction_id: state.assigned_faction,
                        expires_tick: clock.tick.saturating_add(RECONNECT_GRACE_TICKS),
                    },
                );
            }
            info!(
                "client {:?} ({}) disconnected (was driving faction {}); reconnect grace = {} ticks",
                ev.client_id,
                state.player_name,
                state.assigned_faction,
                RECONNECT_GRACE_TICKS
            );
        }
    }
}

/// Disconnect queue drained by `apply_disconnect_policy_system`. Decoupled
/// so the policy system doesn't have to read Lightyear events directly —
/// keeps tests for the policy free of Lightyear setup.
#[derive(Resource, Default, Debug)]
pub struct PendingDisconnects(pub Vec<u32>);

/// Grace window (in `SimClock` ticks at 20Hz FixedUpdate) within which a
/// disconnected client can reconnect by sending the same `player_name`
/// and reclaim their previous faction. ~60 seconds at the default sim
/// rate. After this, the stashed entry is GC'd and the faction is
/// available for fresh allocation.
pub const RECONNECT_GRACE_TICKS: u64 = 1200;

/// `player_name → ReconnectEntry` so a `ClientHello` from a recently-
/// disconnected client can reclaim its prior faction. Populated by
/// `record_disconnections_system`, consumed by `handle_client_hello_system`,
/// expired by `expire_pending_reconnects_system`.
#[derive(Resource, Default, Debug)]
pub struct PendingReconnect {
    pub by_name: ahash::AHashMap<String, ReconnectEntry>,
}

#[derive(Debug, Clone)]
pub struct ReconnectEntry {
    pub faction_id: u32,
    pub expires_tick: u64,
}

impl PendingReconnect {
    pub fn take(&mut self, name: &str) -> Option<ReconnectEntry> {
        self.by_name.remove(name)
    }

    pub fn stash(&mut self, name: String, entry: ReconnectEntry) {
        self.by_name.insert(name, entry);
    }
}

/// Drop any `PendingReconnect` entries whose grace window has passed.
/// Runs once per FixedUpdate via `Calendar::tick`-derived `SimClock` —
/// once-per-second cadence is fine, entries TTL in seconds.
pub fn expire_pending_reconnects_system(
    mut tick_counter: Local<u32>,
    clock: Res<SimClock>,
    mut pending: ResMut<PendingReconnect>,
) {
    *tick_counter = tick_counter.wrapping_add(1);
    // 60Hz Update / 60 ≈ 1Hz — cheap enough.
    if *tick_counter % 60 != 0 {
        return;
    }
    if pending.by_name.is_empty() {
        return;
    }
    let now = clock.tick;
    pending.by_name.retain(|_name, entry| entry.expires_tick > now);
}

/// Drain Lightyear's `ServerReceiveMessage<NetCommandFrame>`, validate
/// ownership, re-emit as `NetPlayerCommandEvent`, send `NetCommandAck`.
///
/// Three failure modes:
/// 1. Connection unknown — log + drop (race vs. disconnect).
/// 2. Sender faction mismatch — ack `OwnershipRejected`.
/// 3. Empty actors after NetId resolution — let the loopback decide
///    (faction-level commands legitimately ship empty).
pub fn receive_command_frames_system(
    mut reader: EventReader<ServerReceiveMessage<NetCommandFrame>>,
    mut out: EventWriter<NetPlayerCommandEvent>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    server_conns: Res<ServerConnections>,
) {
    for event in reader.read() {
        let frame = event.message();
        let client = event.from;
        let Some(assigned) = server_conns.faction_for(client) else {
            warn!("command from unknown client {:?}; dropping", client);
            continue;
        };
        if assigned != frame.sender_faction_id {
            warn!(
                "ownership reject: client {:?} drives {} but frame claims {}",
                client, assigned, frame.sender_faction_id
            );
            let ack = NetCommandAck {
                command_id: frame.command_id,
                status: NetCommandAckStatus::OwnershipRejected,
                reason: Some(format!(
                    "client drives faction {} but frame claimed {}",
                    assigned, frame.sender_faction_id
                )),
            };
            let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
                &ack,
                NetworkTarget::Single(client),
            );
            continue;
        }

        out.send(NetPlayerCommandEvent {
            sender_faction_id: frame.sender_faction_id,
            actors: frame.actors.clone(),
            command: frame.command.clone(),
        });

        let ack = NetCommandAck {
            command_id: frame.command_id,
            status: NetCommandAckStatus::Accepted,
            reason: None,
        };
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &ack,
            NetworkTarget::Single(client),
        );
    }
}

/// Drain `TileChangedEvent` and emit per-chunk `ChunkOverlayDelta`
/// messages reflecting wall/door/bridge/dam/runtime-water state at the
/// changed tile.
///
/// Naive v1: broadcasts to every connected client. Phase 3 will gate on
/// per-connection interest rooms.
#[allow(clippy::too_many_arguments)]
/// Bundle the tile-overlay maps + queries `push_ops_for_tile` needs.
/// Folded into a single SystemParam to stay under Bevy's 16-arg ceiling
/// once plant + structure indexes joined the cast.
#[derive(bevy::ecs::system::SystemParam)]
pub struct TileOverlayParams<'w, 's> {
    pub wall_map: Res<'w, WallMap>,
    pub door_map: Res<'w, DoorMap>,
    pub bridge_map: Res<'w, BridgeMap>,
    pub dam_map: Res<'w, DamMap>,
    pub runtime_water: Res<'w, RuntimeWater>,
    pub plant_map: Res<'w, crate::simulation::plants::PlantMap>,
    pub structure_index: Res<'w, crate::simulation::construction::StructureIndex>,
    pub networked_q: Query<'w, 's, &'static Networked>,
    pub door_q: Query<'w, 's, &'static Door>,
    pub wall_q: Query<'w, 's, &'static crate::simulation::construction::Wall>,
    pub plant_q: Query<'w, 's, &'static crate::simulation::plants::Plant>,
    pub structure_q: Query<
        'w,
        's,
        (
            &'static crate::simulation::construction::StructureLabel,
            Option<&'static crate::simulation::faction::FactionMember>,
        ),
    >,
}

pub fn replicate_tile_overlays_system(
    mut changes: EventReader<TileChangedEvent>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    server_conns: Res<ServerConnections>,
    mut stats: ResMut<ReplicationStats>,
    overlay: TileOverlayParams,
) {
    // Group ops by chunk to keep ChunkOverlayDelta tight.
    let mut by_chunk: ahash::AHashMap<(i32, i32), Vec<TileOverlayOp>> = ahash::AHashMap::new();
    let mut seen_tiles: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();

    for ev in changes.read() {
        let tile = (ev.tx, ev.ty);
        if !seen_tiles.insert(tile) {
            continue;
        }
        let chunk = tile_to_chunk_coord(tile);
        let bucket = by_chunk.entry(chunk).or_default();
        push_ops_for_tile(tile, bucket, &overlay);
    }

    for (chunk, ops) in by_chunk.into_iter() {
        if ops.is_empty() {
            continue;
        }
        let recipients = server_conns.clients_interested_in(chunk);
        if recipients.is_empty() {
            continue;
        }
        let delta = ChunkOverlayDelta { chunk, ops };
        let approx_bytes = bincode::serialized_size(&delta).unwrap_or(0);
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &delta,
            NetworkTarget::Only(recipients),
        );
        stats.tile_overlay_deltas_sent += 1;
        stats.tile_overlay_bytes_sent += approx_bytes;
    }
}

fn push_ops_for_tile(
    tile: (i32, i32),
    out: &mut Vec<TileOverlayOp>,
    overlay: &TileOverlayParams,
) {
    let wall_map = &overlay.wall_map;
    let door_map = &overlay.door_map;
    let bridge_map = &overlay.bridge_map;
    let dam_map = &overlay.dam_map;
    let runtime_water = &overlay.runtime_water;
    let networked_q = &overlay.networked_q;
    let door_q = &overlay.door_q;
    let wall_q = &overlay.wall_q;
    // Walls
    match wall_map.0.get(&tile) {
        Some(&entity) => {
            if let Ok(n) = networked_q.get(entity) {
                out.push(TileOverlayOp::AddWall {
                    tile,
                    entity_net_id: n.0,
                    owner_faction: wall_q.get(entity).ok().and_then(|w| w.owner_faction),
                });
            }
        }
        None => {
            // We can't distinguish "never had wall" from "removed wall"
            // cheaply; emit Remove unconditionally and let the client's
            // tile map ignore unknowns. Idempotent.
            out.push(TileOverlayOp::RemoveWall { tile });
        }
    }

    // Doors
    match door_map.0.get(&tile) {
        Some(entry) => {
            if let Ok(n) = networked_q.get(entry.entity) {
                out.push(TileOverlayOp::AddDoor {
                    tile,
                    entity_net_id: n.0,
                    open: entry.open,
                    faction_id: entry.faction_id,
                });
            }
            // Also push open-state diff in case AddDoor was already sent
            // and only `open` changed (Door entity persists; we don't
            // track which subset of fields drifted).
            if let Ok(door) = door_q.get(entry.entity) {
                let _ = door;
                out.push(TileOverlayOp::SetDoorOpen {
                    tile,
                    open: entry.open,
                });
            }
        }
        None => {
            out.push(TileOverlayOp::RemoveDoor { tile });
        }
    }

    // Bridges
    if let Some(&entity) = bridge_map.0.get(&tile) {
        if let Ok(n) = networked_q.get(entity) {
            out.push(TileOverlayOp::AddBridge {
                tile,
                entity_net_id: n.0,
            });
        }
    } else {
        out.push(TileOverlayOp::RemoveBridge { tile });
    }

    // Dams
    if let Some(&entity) = dam_map.0.get(&tile) {
        if let Ok(n) = networked_q.get(entity) {
            out.push(TileOverlayOp::AddDam {
                tile,
                entity_net_id: n.0,
            });
        }
    } else {
        out.push(TileOverlayOp::RemoveDam { tile });
    }

    // Runtime water
    if let Some(cell) = runtime_water.cells.get(&tile) {
        out.push(TileOverlayOp::SetRuntimeWater {
            tile,
            cell: *cell,
        });
    } else {
        out.push(TileOverlayOp::ClearRuntimeWater { tile });
    }

    // Plants
    match overlay.plant_map.0.get(&tile) {
        Some(&entity) => {
            if let (Ok(n), Ok(plant)) = (networked_q.get(entity), overlay.plant_q.get(entity)) {
                out.push(TileOverlayOp::AddPlant {
                    tile,
                    entity_net_id: n.0,
                    kind: plant_kind_to_wire(plant.kind),
                    stage: plant_stage_to_wire(plant.stage),
                });
            }
        }
        None => {
            out.push(TileOverlayOp::RemovePlant { tile });
        }
    }

    // Structures (Bed / Workshop / Campfire / Storage / civic anchors).
    // Wall/Door/Bridge/Dam already covered above; only Person-style
    // structure entities live in `StructureIndex`.
    match overlay.structure_index.0.get(&tile) {
        Some(&entity) => {
            if let (Ok(n), Ok((label, faction))) =
                (networked_q.get(entity), overlay.structure_q.get(entity))
            {
                out.push(TileOverlayOp::AddStructure {
                    tile,
                    entity_net_id: n.0,
                    kind: structure_kind_wire_from_label(label.0),
                    owner_faction: faction.map(|f| f.faction_id).unwrap_or(0),
                    label_id: label_hash_u16(label.0),
                });
            }
        }
        None => {
            out.push(TileOverlayOp::RemoveStructure { tile });
        }
    }

    let _ = door_q; // suppresses unused-binding when Door query isn't sampled per-op
}

/// Translate plant + structure spawn/despawn into `TileChangedEvent` so
/// the existing `replicate_tile_overlays_system` picks them up. Without
/// this, plants and structures only replicate when something else
/// (chunk reload, wall change) happens on the same tile.
#[allow(clippy::type_complexity)]
pub fn emit_tile_changed_for_replicated_entities_system(
    added_plants: Query<
        (&Transform, ()),
        bevy::prelude::Added<crate::simulation::plants::Plant>,
    >,
    added_structures: Query<
        (&Transform, ()),
        bevy::prelude::Added<crate::simulation::construction::StructureLabel>,
    >,
    mut removed_plants: RemovedComponents<crate::simulation::plants::Plant>,
    mut removed_structures: RemovedComponents<crate::simulation::construction::StructureLabel>,
    last_known: Res<LastKnownChunkMap>,
    networked_q: Query<&Networked>,
    mut events: EventWriter<TileChangedEvent>,
) {
    for (tf, _) in &added_plants {
        let tile = translation_to_tile(tf.translation.truncate());
        events.send(TileChangedEvent { tx: tile.0, ty: tile.1 });
    }
    for (tf, _) in &added_structures {
        let tile = translation_to_tile(tf.translation.truncate());
        events.send(TileChangedEvent { tx: tile.0, ty: tile.1 });
    }
    // Removals: we no longer have a Transform; fall back to the
    // `LastKnownChunkMap` to recover an approximate tile (every replicated
    // entity passes through `replicate_entity_state_system`, which keys
    // the map by `NetId`). If the entity was never replicated yet, the
    // remove falls through — the client never saw the add either.
    for e in removed_plants.read() {
        if let Ok(n) = networked_q.get(e) {
            if let Some(&(cx, cy)) = last_known.0.get(&n.0) {
                // chunk coord → emit one event for the chunk centre so
                // `replicate_tile_overlays_system` re-runs `push_ops_for_tile`
                // for a representative tile (it'll emit RemovePlant
                // for the actual ex-tile too if the plant map already
                // dropped it).
                let tx = cx * crate::world::chunk::CHUNK_SIZE as i32;
                let ty = cy * crate::world::chunk::CHUNK_SIZE as i32;
                events.send(TileChangedEvent { tx, ty });
            }
        }
    }
    for e in removed_structures.read() {
        if let Ok(n) = networked_q.get(e) {
            if let Some(&(cx, cy)) = last_known.0.get(&n.0) {
                let tx = cx * crate::world::chunk::CHUNK_SIZE as i32;
                let ty = cy * crate::world::chunk::CHUNK_SIZE as i32;
                events.send(TileChangedEvent { tx, ty });
            }
        }
    }
}

fn plant_kind_to_wire(
    kind: crate::simulation::plants::PlantKind,
) -> crate::net::protocol::PlantKindWire {
    use crate::net::protocol::PlantKindWire;
    use crate::simulation::plants::PlantKind;
    match kind {
        PlantKind::Grain => PlantKindWire::Grain,
        PlantKind::BerryBush => PlantKindWire::BerryBush,
        PlantKind::Tree => PlantKindWire::Tree,
    }
}

fn plant_stage_to_wire(
    stage: crate::simulation::plants::GrowthStage,
) -> crate::net::protocol::PlantStageWire {
    use crate::net::protocol::PlantStageWire;
    use crate::simulation::plants::GrowthStage;
    match stage {
        GrowthStage::Seed => PlantStageWire::Seed,
        GrowthStage::Seedling => PlantStageWire::Seedling,
        GrowthStage::Mature => PlantStageWire::Mature,
        GrowthStage::Overripe => PlantStageWire::Overripe,
        GrowthStage::Harvested => PlantStageWire::Harvested,
    }
}

/// Bucket the &'static label string into one of five wire categories the
/// client uses to pick rendering. Falls back to `CivicAnchor` for
/// monument-class anchors the client doesn't need a distinct sprite for.
fn structure_kind_wire_from_label(label: &'static str) -> crate::net::protocol::StructureKindWire {
    use crate::net::protocol::StructureKindWire;
    match label {
        "Bed" | "Bedroll" | "Tent" | "Yurt" => StructureKindWire::Bed,
        "Campfire" => StructureKindWire::Campfire,
        "Granary" => StructureKindWire::Storage,
        "Workbench" | "Loom" | "Table" | "Chair" | "Pen" | "Stable"
        | "Feed Trough" | "Hitching Post" | "Vehicle Yard" => StructureKindWire::Workshop,
        _ => StructureKindWire::CivicAnchor,
    }
}

/// Hash the structure label into a stable `u16` so the client can match
/// `label_id` to a specific variant without sending the &'static str
/// over the wire.
fn label_hash_u16(label: &'static str) -> u16 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let state = ahash::RandomState::with_seeds(
        0x4356_4944_5f4c_414e,
        0x5354_5255_4354_5552,
        0x0000_0000_0000_0001,
        0x0000_0000_0000_0002,
    );
    let mut h = state.build_hasher();
    label.hash(&mut h);
    (h.finish() & 0xFFFF) as u16
}

// ============================================================================
// Phase 3a — per-tick entity state replication
// ============================================================================

/// How many Update ticks between `EntityStateDelta` broadcasts. 60Hz Update
/// × 3 ≈ 50ms — lines up with `NET_TICK_INTERVAL`. Phase 3c will replace
/// this single global cadence with per-entity rate tiering (owned tick /
/// neighbour 2-tick / far 10-tick).
pub const ENTITY_REP_INTERVAL_UPDATES: u32 = 3;

/// How many Update ticks between interest-room rebuilds. Cheap per-pass but
/// rebuilding every tick is wasted work — chunk membership only changes
/// when a faction relocates a settlement or the player pans across a
/// chunk boundary, neither of which fires at frame rate. ~500 ms feels
/// snappy without burning cycles.
pub const INTEREST_REBUILD_INTERVAL_UPDATES: u32 = 30;

/// Server-side cache of "the last chunk we replicated entity X from."
/// Populated by `replicate_entity_state_system`, consumed by
/// `replicate_entity_removals_system` so removes ride the same interest
/// gating as state deltas. NetIds that despawn before they were ever
/// replicated stay absent — the remove falls back to broadcast.
#[derive(Resource, Default, Debug)]
pub struct LastKnownChunkMap(pub ahash::AHashMap<crate::net_id::NetId, (i32, i32)>);

/// Per-channel bandwidth + send-rate counters for the replication
/// pipeline (Phase 3g). Follows the project's existing diagnostic-resource
/// pattern (cf. `BackgroundWorkDiagnostics`). The `*_sent` fields are
/// raw accumulators reset each reporting window; the `*_per_sec` fields
/// are the rates from the most-recent completed window. UI / log /
/// dashboards should read the `_per_sec` fields.
#[derive(Resource, Default, Debug)]
pub struct ReplicationStats {
    // Raw accumulators — bumped by the per-channel replicators each tick,
    // zeroed by `report_replication_stats_system` once per window.
    pub entity_deltas_sent: u64,
    pub entity_entries_sent: u64,
    pub entity_bytes_sent: u64,
    pub tile_overlay_deltas_sent: u64,
    pub tile_overlay_bytes_sent: u64,
    pub entity_removed_msgs_sent: u64,
    pub entity_removed_bytes_sent: u64,

    // Snapshot of the last completed window (read-only consumer surface).
    pub entity_deltas_per_sec: f32,
    pub entity_entries_per_sec: f32,
    pub entity_bytes_per_sec: f32,
    pub tile_overlay_deltas_per_sec: f32,
    pub tile_overlay_bytes_per_sec: f32,
    pub entity_removed_msgs_per_sec: f32,
    pub entity_removed_bytes_per_sec: f32,

    // Internal: report-window heartbeat counter.
    pub report_cooldown_updates: u32,
}

const STATS_REPORT_INTERVAL_UPDATES: u32 = 60; // ~1s at 60Hz Update

/// Snapshot every `Networked` Person / Animal / Vehicle once per
/// `ENTITY_REP_INTERVAL_UPDATES`, group by chunk, broadcast
/// `EntityStateDelta` to `NetworkTarget::All`.
///
/// v1 ships every entity to every client. Phase 3b will replace
/// `NetworkTarget::All` with per-connection `RoomManager` membership keyed
/// by `entity_chunk ∈ ServerConnections.interest_chunks[client]`.
#[allow(clippy::too_many_arguments)]
pub fn replicate_entity_state_system(
    mut tick_counter: Local<u32>,
    mut send_index: Local<u32>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    server_conns: Res<ServerConnections>,
    clock: Res<SimClock>,
    mut stats: ResMut<ReplicationStats>,
    mut last_chunks: ResMut<LastKnownChunkMap>,
    person_q: Query<
        (
            &Networked,
            &Transform,
            &PersonAI,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Person>,
    >,
    vehicle_q: Query<(&Networked, &Vehicle, &VehicleHealth)>,
    wolf_q: Query<
        (
            &Networked,
            &Transform,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Wolf>,
    >,
    deer_q: Query<
        (
            &Networked,
            &Transform,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Deer>,
    >,
    horse_q: Query<
        (
            &Networked,
            &Transform,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Horse>,
    >,
    cow_q: Query<
        (
            &Networked,
            &Transform,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Cow>,
    >,
    pig_q: Query<
        (
            &Networked,
            &Transform,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Pig>,
    >,
    cat_q: Query<
        (
            &Networked,
            &Transform,
            Option<&FacingDirection>,
            Option<&Health>,
            Option<&FactionMember>,
        ),
        With<Cat>,
    >,
) {
    *tick_counter = tick_counter.wrapping_add(1);
    if *tick_counter % ENTITY_REP_INTERVAL_UPDATES != 0 {
        return;
    }
    // Monotonic per-send counter — drives per-tier cadence gating in
    // `ServerConnections::clients_for_entity_chunk`. Wraps after ~4B
    // sends (~7 years at 50ms cadence — fine).
    *send_index = send_index.wrapping_add(1);

    let mut by_chunk: ahash::AHashMap<(i32, i32), Vec<EntityStateEntry>> =
        ahash::AHashMap::new();

    for (n, t, ai, facing, health, member) in person_q.iter() {
        let entry = build_entry(
            n.0,
            EntityKindWire::Person,
            translation_to_tile(t.translation.truncate()),
            ai.current_z,
            facing,
            health,
            member.map(|m| m.faction_id),
        );
        by_chunk
            .entry(tile_to_chunk_coord(entry.tile))
            .or_default()
            .push(entry);
    }

    for (n, vehicle, vehealth) in vehicle_q.iter() {
        let (cur, max) = vehicle_health_total(vehealth);
        let entry = EntityStateEntry {
            net_id: n.0,
            kind: EntityKindWire::Vehicle,
            tile: vehicle.anchor_tile,
            z: vehicle.z,
            facing: heading_to_facing(vehicle.heading),
            health_current: cur,
            health_max: max,
            faction_id: vehicle.owner_faction,
        };
        by_chunk
            .entry(tile_to_chunk_coord(entry.tile))
            .or_default()
            .push(entry);
    }

    push_animal(
        &mut by_chunk,
        wolf_q.iter(),
        AnimalSpeciesWire::Wolf,
    );
    push_animal(
        &mut by_chunk,
        deer_q.iter(),
        AnimalSpeciesWire::Deer,
    );
    push_animal(
        &mut by_chunk,
        horse_q.iter(),
        AnimalSpeciesWire::Horse,
    );
    push_animal(&mut by_chunk, cow_q.iter(), AnimalSpeciesWire::Cow);
    push_animal(&mut by_chunk, pig_q.iter(), AnimalSpeciesWire::Pig);
    push_animal(&mut by_chunk, cat_q.iter(), AnimalSpeciesWire::Cat);

    let server_tick = clock.tick;
    for (chunk, entries) in by_chunk.into_iter() {
        if entries.is_empty() {
            continue;
        }
        // Record last-known chunk for every entry — feeds
        // `replicate_entity_removals_system` so removes can ride the
        // same interest gating as state deltas.
        for entry in &entries {
            last_chunks.0.insert(entry.net_id, chunk);
        }
        let recipients = server_conns.clients_for_entity_chunk(chunk, *send_index);
        if recipients.is_empty() {
            continue;
        }
        let entry_count = entries.len() as u64;
        let delta = EntityStateDelta {
            server_tick,
            chunk,
            entries,
        };
        let approx_bytes = bincode::serialized_size(&delta).unwrap_or(0);
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &delta,
            NetworkTarget::Only(recipients),
        );
        stats.entity_deltas_sent += 1;
        stats.entity_entries_sent += entry_count;
        stats.entity_bytes_sent += approx_bytes;
    }
}

/// Once per `STATS_REPORT_INTERVAL_UPDATES` (≈ 1 s at 60 Hz Update),
/// snapshot the raw accumulators on `ReplicationStats` into the
/// `_per_sec` rate fields and reset the accumulators. Also emits a
/// single `debug!` line so an operator can `RUST_LOG=civgame::net=debug`
/// to follow steady-state bandwidth without burning info-level chatter.
pub fn report_replication_stats_system(mut stats: ResMut<ReplicationStats>) {
    stats.report_cooldown_updates = stats.report_cooldown_updates.wrapping_add(1);
    if stats.report_cooldown_updates < STATS_REPORT_INTERVAL_UPDATES {
        return;
    }
    let window_secs = STATS_REPORT_INTERVAL_UPDATES as f32 / 60.0;
    let inv = if window_secs > 0.0 {
        1.0 / window_secs
    } else {
        0.0
    };
    stats.entity_deltas_per_sec = stats.entity_deltas_sent as f32 * inv;
    stats.entity_entries_per_sec = stats.entity_entries_sent as f32 * inv;
    stats.entity_bytes_per_sec = stats.entity_bytes_sent as f32 * inv;
    stats.tile_overlay_deltas_per_sec = stats.tile_overlay_deltas_sent as f32 * inv;
    stats.tile_overlay_bytes_per_sec = stats.tile_overlay_bytes_sent as f32 * inv;
    stats.entity_removed_msgs_per_sec = stats.entity_removed_msgs_sent as f32 * inv;
    stats.entity_removed_bytes_per_sec = stats.entity_removed_bytes_sent as f32 * inv;

    debug!(
        "net rep/s: entity {:.0} deltas / {:.0} entries / {:.0} B; overlay {:.0} deltas / {:.0} B; removes {:.0} msgs / {:.0} B",
        stats.entity_deltas_per_sec,
        stats.entity_entries_per_sec,
        stats.entity_bytes_per_sec,
        stats.tile_overlay_deltas_per_sec,
        stats.tile_overlay_bytes_per_sec,
        stats.entity_removed_msgs_per_sec,
        stats.entity_removed_bytes_per_sec,
    );

    stats.entity_deltas_sent = 0;
    stats.entity_entries_sent = 0;
    stats.entity_bytes_sent = 0;
    stats.tile_overlay_deltas_sent = 0;
    stats.tile_overlay_bytes_sent = 0;
    stats.entity_removed_msgs_sent = 0;
    stats.entity_removed_bytes_sent = 0;
    stats.report_cooldown_updates = 0;
}

type AnimalRow<'a> = (
    &'a Networked,
    &'a Transform,
    Option<&'a FacingDirection>,
    Option<&'a Health>,
    Option<&'a FactionMember>,
);

fn push_animal<'a, I>(
    by_chunk: &mut ahash::AHashMap<(i32, i32), Vec<EntityStateEntry>>,
    iter: I,
    species: AnimalSpeciesWire,
) where
    I: IntoIterator<Item = AnimalRow<'a>>,
{
    for (n, t, facing, health, member) in iter {
        let entry = build_entry(
            n.0,
            EntityKindWire::Animal(species),
            translation_to_tile(t.translation.truncate()),
            0, // animals don't expose Z; rendering reads Transform.translation.z
            facing,
            health,
            member.map(|m| m.faction_id),
        );
        by_chunk
            .entry(tile_to_chunk_coord(entry.tile))
            .or_default()
            .push(entry);
    }
}

fn build_entry(
    net_id: crate::net_id::NetId,
    kind: EntityKindWire,
    tile: (i32, i32),
    z: i8,
    facing: Option<&FacingDirection>,
    health: Option<&Health>,
    faction_id: Option<u32>,
) -> EntityStateEntry {
    EntityStateEntry {
        net_id,
        kind,
        tile,
        z,
        facing: facing.map(|f| *f as u8).unwrap_or(0),
        health_current: health.map(|h| h.current as u16).unwrap_or(0),
        health_max: health.map(|h| h.max as u16).unwrap_or(0),
        faction_id: faction_id.unwrap_or(0),
    }
}

fn translation_to_tile(pos: bevy::math::Vec2) -> (i32, i32) {
    (
        (pos.x / TILE_SIZE).floor() as i32,
        (pos.y / TILE_SIZE).floor() as i32,
    )
}

fn vehicle_health_total(h: &VehicleHealth) -> (u16, u16) {
    let cur: u32 = h.cells.iter().map(|(_, hp)| *hp as u32).sum();
    // No design-side max here; approximate "destroyed cell count" as
    // `cells.len() * 100` so the bar reads as cells-alive ratio. Phase 3
    // will plumb per-cell max via `VehicleHealth`.
    let max = (h.cells.len() as u32) * 100;
    (cur.min(u16::MAX as u32) as u16, max.min(u16::MAX as u32) as u16)
}

/// Stable sort key for `ClientId` so identical recipient sets hash the
/// same in `replicate_entity_removals_system`'s coalescing map.
/// Discriminant first (Netcode=0/Steam=1/Local=2), inner id second.
fn client_id_sort_key(c: ClientId) -> (u8, u64) {
    match c {
        ClientId::Netcode(id) => (0, id),
        ClientId::Steam(id) => (1, id),
        ClientId::Local(id) => (2, id),
        // Lightyear added a Server pseudo-id variant in a point release.
        // It shouldn't appear in `by_client` (the server isn't its own
        // client), but cover it so the match stays exhaustive.
        _ => (255, 0),
    }
}

/// Drain `ServerReceiveMessage<ClientCameraFocus>` and stash the reported
/// focus chunk on the corresponding `ConnectionState`. `compute_interest_system`
/// reads this on its next rebuild so the camera position participates in
/// interest classification alongside faction home + settlements.
pub fn receive_camera_focus_system(
    mut reader: EventReader<lightyear::prelude::ServerReceiveMessage<ClientCameraFocus>>,
    mut server_conns: ResMut<ServerConnections>,
) {
    for ev in reader.read() {
        let chunk = tile_to_chunk_coord(ev.message().tile);
        if let Some(state) = server_conns.by_client.get_mut(&ev.from) {
            state.camera_focus_chunk = Some(chunk);
        }
    }
}

/// Rebuild every connection's `interest_chunks` set every
/// `INTEREST_REBUILD_INTERVAL_UPDATES` ticks. Each client's frame is the
/// union of:
///
/// - chunks within `INTEREST_RADIUS_CHUNKS` of the assigned faction's
///   `home_tile` (via `bootstrap::compute_interest_chunks`),
/// - chunks within the same radius of every owned settlement's
///   `market_tile`.
///
/// Phase 3c follow-on: camera focus + active military groups should also
/// feed interest. For now, faction home + settlements catch the steady
/// state.
#[allow(clippy::too_many_arguments)]
pub fn compute_interest_system(
    mut tick_counter: Local<u32>,
    mut server_conns: ResMut<ServerConnections>,
    factions: Res<FactionRegistry>,
    settlement_map: Res<SettlementMap>,
    settlement_q: Query<&Settlement>,
    transform_q: Query<&Transform>,
    drafted_q: Query<(&Transform, &FactionMember), With<Drafted>>,
) {
    *tick_counter = tick_counter.wrapping_add(1);
    if *tick_counter % INTEREST_REBUILD_INTERVAL_UPDATES != 0 {
        return;
    }

    for (_client, state) in server_conns.by_client.iter_mut() {
        // Collect the anchor chunks (faction home + every owned
        // settlement's market + reported camera focus). `Owned` rings
        // inflate around each anchor by ±1; `Neighbour` extends to
        // NEIGHBOUR_TIER_RADIUS; the rest of the INTEREST_RADIUS_CHUNKS
        // frame is `Far`.
        let mut anchors: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
        if let Some(faction) = factions.factions.get(&state.assigned_faction) {
            anchors.insert(tile_to_chunk_coord(faction.home_tile));
        }
        if let Some(set_ids) = settlement_map.by_faction.get(&state.assigned_faction) {
            for &sid in set_ids {
                let Some(&entity) = settlement_map.by_id.get(&sid) else {
                    continue;
                };
                let Ok(settlement) = settlement_q.get(entity) else {
                    continue;
                };
                anchors.insert(tile_to_chunk_coord(settlement.market_tile));
            }
        }
        if let Some(camera_chunk) = state.camera_focus_chunk {
            anchors.insert(camera_chunk);
        }

        // Active military: fold each raid-party member, hunt-party
        // muster member, and drafted defender's current chunk in as a
        // Neighbour-tier anchor so marching war bands, scouting hunters,
        // and rallied defenders stay visible to the controlling player
        // even when they're beyond the settlement ring. Each cohort is
        // bounded (raid ≤ RAID_MAX_PARTY_ABS, hunt ≤ target_party_size,
        // defenders by `Drafted` insertion sites), so the per-rebuild
        // iteration cost stays trivial.
        let mut military_anchors: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
        if let Some(faction) = factions.factions.get(&state.assigned_faction) {
            let push_entity_chunk =
                |anchors: &mut ahash::AHashSet<(i32, i32)>, entity: bevy::prelude::Entity| {
                    let Ok(tf) = transform_q.get(entity) else {
                        return;
                    };
                    let tile = (
                        (tf.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32,
                        (tf.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32,
                    );
                    anchors.insert(tile_to_chunk_coord(tile));
                };
            for &member in &faction.raid_party {
                push_entity_chunk(&mut military_anchors, member);
            }
            if let Some(HuntOrder::Hunt { mustered, .. }) = faction.hunt_order.as_ref() {
                for &member in mustered {
                    push_entity_chunk(&mut military_anchors, member);
                }
            }
        }
        // Drafted defenders: any same-faction member with `Drafted` is a
        // muster-rallied combatant who should stay live for the player.
        // Faction-side bookkeeping doesn't expose them as a Vec, so we
        // scan the (small) `Drafted` query and filter on faction.
        for (tf, member) in drafted_q.iter() {
            if member.faction_id != state.assigned_faction {
                continue;
            }
            let tile = (
                (tf.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32,
                (tf.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32,
            );
            military_anchors.insert(tile_to_chunk_coord(tile));
        }

        let mut tiered: ahash::AHashMap<(i32, i32), InterestTier> = ahash::AHashMap::new();
        // Seed `Owned` (±1 of each settled anchor) and `Neighbour`
        // (±NEIGHBOUR_TIER_RADIUS) first so `Far` only fills cells the
        // tighter tiers didn't already claim. Military anchors only
        // promote to Neighbour — a roving war band shouldn't burn the
        // full-rate budget the player's settlement gets.
        for &(ax, ay) in anchors.iter() {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    tiered.insert((ax + dx, ay + dy), InterestTier::Owned);
                }
            }
        }
        for &(ax, ay) in anchors.iter().chain(military_anchors.iter()) {
            for dy in -NEIGHBOUR_TIER_RADIUS..=NEIGHBOUR_TIER_RADIUS {
                for dx in -NEIGHBOUR_TIER_RADIUS..=NEIGHBOUR_TIER_RADIUS {
                    tiered
                        .entry((ax + dx, ay + dy))
                        .or_insert(InterestTier::Neighbour);
                }
            }
        }
        // `Far` fills the rest of `INTEREST_RADIUS_CHUNKS` reuse — we lean
        // on `compute_interest_chunks` as the source of truth for the
        // outer frame so the bootstrap snapshot and runtime stay aligned.
        for c in
            compute_interest_chunks(&[state.assigned_faction], &factions, INTEREST_RADIUS_CHUNKS)
        {
            tiered.entry(c).or_insert(InterestTier::Far);
        }

        state.interest_chunks = tiered;
    }
}

/// Drain `NetworkedRemovedEvent`s emitted by `release_net_ids_on_despawn`
/// (`net_id.rs`, PostUpdate) and ship interest-gated `EntityRemoved`
/// frames. NetIds whose last-known chunk is recorded in
/// `LastKnownChunkMap` ride to clients interested in that chunk (at any
/// tier — removes ignore rate tiering, they're tiny and must not be
/// dropped). NetIds with no recorded chunk (entity despawned before ever
/// being replicated) fall back to broadcast.
pub fn replicate_entity_removals_system(
    mut removed: EventReader<NetworkedRemovedEvent>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    server_conns: Res<ServerConnections>,
    mut last_chunks: ResMut<LastKnownChunkMap>,
    mut stats: ResMut<ReplicationStats>,
) {
    let ids: Vec<crate::net_id::NetId> = removed.read().map(|ev| ev.net_id).collect();
    if ids.is_empty() {
        return;
    }
    if server_conns.by_client.is_empty() {
        // No remote clients listening — drain and drop. Still need to
        // evict from last-known-chunk map so it doesn't grow unbounded.
        for id in &ids {
            last_chunks.0.remove(id);
        }
        return;
    }

    // Group ids by recipient set so a single message goes to each
    // interested client.
    let mut by_recipients: ahash::AHashMap<Vec<ClientId>, Vec<crate::net_id::NetId>> =
        ahash::AHashMap::new();
    let mut unknown_chunk: Vec<crate::net_id::NetId> = Vec::new();
    for id in ids {
        match last_chunks.0.remove(&id) {
            Some(chunk) => {
                let mut recipients = server_conns.clients_interested_in(chunk);
                recipients.sort_by_key(|c| client_id_sort_key(*c));
                if recipients.is_empty() {
                    continue; // no client ever saw this entity; safe to drop
                }
                by_recipients.entry(recipients).or_default().push(id);
            }
            None => unknown_chunk.push(id),
        }
    }

    for (recipients, net_ids) in by_recipients {
        let msg = EntityRemoved { net_ids };
        let approx_bytes = bincode::serialized_size(&msg).unwrap_or(0);
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &msg,
            NetworkTarget::Only(recipients),
        );
        stats.entity_removed_msgs_sent += 1;
        stats.entity_removed_bytes_sent += approx_bytes;
    }

    if !unknown_chunk.is_empty() {
        let msg = EntityRemoved {
            net_ids: unknown_chunk,
        };
        let approx_bytes = bincode::serialized_size(&msg).unwrap_or(0);
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &msg,
            NetworkTarget::All,
        );
        stats.entity_removed_msgs_sent += 1;
        stats.entity_removed_bytes_sent += approx_bytes;
    }
}

// ============================================================================
// Phase 8.1 — Server lobby handlers
// ============================================================================
//
// Server-side drains for `LobbyJoin / LobbySelectStart / LobbySetReady /
// LobbyLeave`, the snapshot broadcaster, and the start-game transition.
// Mounted only in `ListenServer` / `DedicatedServer`. The host's own UI
// emits commands through `LocalLobbyCommand` to bypass the wire while
// reusing the same handlers.

use crate::game_state::{
    EconomyPreset, GameStartOptions, GameState, PendingStarts, PlayerStartSlot,
    StartSettlementMaturity,
};
use crate::net::lobby_state::{LobbyPhase, LobbyState, ServerLobbySlot};
use crate::net::protocol::{
    LobbyJoin, LobbyLeave, LobbyReject, LobbyRejectReason, LobbySelectStart, LobbySetReady,
    LobbySlotAssignment, LobbySnapshot, LobbyStartGame,
};
use crate::simulation::faction::Lifestyle;
use crate::simulation::technology::Era;

/// Locally-issued lobby command for the `ListenServer` host (its own UI
/// edits the lobby without going through Lightyear). One event per command
/// kind keeps the wire/local path symmetric: the host UI writes
/// `LocalLobbyCommand`, the handler drains it the same way it drains the
/// `ServerReceiveMessage<_>` variant.
#[derive(bevy::ecs::event::Event, Debug, Clone)]
pub enum LocalLobbyCommand {
    Join { player_name: String, client_id: u64 },
    SelectStart { client_id: u64, megachunk: (i32, i32) },
    SetReady { client_id: u64, ready: bool },
    Leave { client_id: u64 },
}

/// Drain `LobbyJoin` (wire + local). Validates `protocol_version`, then
/// either reclaims an existing slot by `player_name` or appends a fresh
/// one. Always calls `lobby.bump()` so the snapshot broadcaster ships
/// next tick.
pub fn handle_lobby_join_system(
    mut wire: EventReader<lightyear::prelude::ServerReceiveMessage<LobbyJoin>>,
    mut local: EventReader<LocalLobbyCommand>,
    mut lobby: ResMut<LobbyState>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
) {
    // Wire joins.
    for ev in wire.read() {
        let msg = ev.message();
        let client = ev.from;
        let client_id_u64 = client_id_to_u64(client);
        if msg.protocol_version != crate::net::protocol::PROTOCOL_VERSION {
            let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
                &LobbyReject {
                    reason: LobbyRejectReason::ProtocolMismatch,
                    detail: None,
                },
                NetworkTarget::Single(client),
            );
            continue;
        }
        if !lobby.accepts_join(&msg.player_name) {
            let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
                &LobbyReject {
                    reason: LobbyRejectReason::LobbyFull,
                    detail: None,
                },
                NetworkTarget::Single(client),
            );
            continue;
        }
        apply_lobby_join(&mut lobby, &msg.player_name, client_id_u64);
    }

    // Local joins (host UI).
    for ev in local.read() {
        if let LocalLobbyCommand::Join { player_name, client_id } = ev {
            if !lobby.accepts_join(player_name) {
                continue;
            }
            apply_lobby_join(&mut lobby, player_name, *client_id);
        }
    }
}

fn apply_lobby_join(lobby: &mut LobbyState, player_name: &str, client_id: u64) {
    if let Some(slot) = lobby
        .slots
        .iter_mut()
        .find(|s| s.player_name == player_name)
    {
        slot.client_id = client_id;
    } else {
        let slot_id = lobby.next_slot_id();
        lobby.slots.push(ServerLobbySlot {
            slot_id,
            player_name: player_name.to_string(),
            client_id,
            megachunk: None,
            lifestyle: Lifestyle::Settled,
            ready: false,
            faction_id: None,
        });
    }
    lobby.bump();
}

/// Drain `LobbySelectStart`. Validates megachunk distance against
/// `MIN_HUMAN_MEGACHUNK_DISTANCE`; globe habitability is checked here
/// too via `region::is_megachunk_habitable`. On accept writes the
/// slot's megachunk; on reject ships `LobbyReject::StartInvalid`.
pub fn handle_lobby_select_start_system(
    mut wire: EventReader<lightyear::prelude::ServerReceiveMessage<LobbySelectStart>>,
    mut local: EventReader<LocalLobbyCommand>,
    mut lobby: ResMut<LobbyState>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    globe: Option<Res<crate::world::globe::Globe>>,
) {
    for ev in wire.read() {
        let msg = ev.message();
        let client = ev.from;
        let client_id_u64 = client_id_to_u64(client);
        if !megachunk_acceptable(&lobby, client_id_u64, msg.megachunk, globe.as_deref()) {
            let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
                &LobbyReject {
                    reason: LobbyRejectReason::StartInvalid,
                    detail: None,
                },
                NetworkTarget::Single(client),
            );
            continue;
        }
        if let Some(slot) = lobby.slot_for_client_mut(client_id_u64) {
            slot.megachunk = Some(msg.megachunk);
        }
        lobby.bump();
    }

    for ev in local.read() {
        if let LocalLobbyCommand::SelectStart {
            client_id,
            megachunk,
        } = ev
        {
            if !megachunk_acceptable(&lobby, *client_id, *megachunk, globe.as_deref()) {
                continue;
            }
            if let Some(slot) = lobby.slot_for_client_mut(*client_id) {
                slot.megachunk = Some(*megachunk);
            }
            lobby.bump();
        }
    }
}

fn megachunk_acceptable(
    lobby: &LobbyState,
    client_id: u64,
    megachunk: (i32, i32),
    globe: Option<&crate::world::globe::Globe>,
) -> bool {
    if !lobby.is_select_acceptable(megachunk, client_id) {
        return false;
    }
    // Habitability check (when globe loaded). Mirror the relief filter
    // `region::pick_player_home_in_megachunk` uses — centre tile of the
    // mega-chunk must not be mountain / ocean.
    if let Some(globe) = globe {
        let (cx, cy) = crate::simulation::region::MegaChunkCoord::center_tile(megachunk.0, megachunk.1);
        if globe.sample_relief(cx, cy).class.rejects_settlement() {
            return false;
        }
    }
    true
}

/// Drain `LobbySetReady`. Writes the slot's `ready` flag and bumps; the
/// `bump()` path may auto-advance into `Starting`.
pub fn handle_lobby_set_ready_system(
    mut wire: EventReader<lightyear::prelude::ServerReceiveMessage<LobbySetReady>>,
    mut local: EventReader<LocalLobbyCommand>,
    mut lobby: ResMut<LobbyState>,
) {
    for ev in wire.read() {
        let msg = ev.message();
        let client_id_u64 = client_id_to_u64(ev.from);
        if let Some(slot) = lobby.slot_for_client_mut(client_id_u64) {
            slot.ready = msg.ready;
        }
        lobby.bump();
    }
    for ev in local.read() {
        if let LocalLobbyCommand::SetReady { client_id, ready } = ev {
            if let Some(slot) = lobby.slot_for_client_mut(*client_id) {
                slot.ready = *ready;
            }
            lobby.bump();
        }
    }
}

/// Drain `LobbyLeave` (wire + local) plus `ServerDisconnectEvent` while
/// the phase is pre-`InGame`. Removes the slot. Mid-game reconnect-by-
/// name is handled by `record_disconnections_system` + `PendingReconnect`
/// — lobby slots aren't stashed (the player can rejoin from a clean slot).
pub fn handle_lobby_leave_system(
    mut wire: EventReader<lightyear::prelude::ServerReceiveMessage<LobbyLeave>>,
    mut local: EventReader<LocalLobbyCommand>,
    mut disconnects: EventReader<ServerDisconnectEvent>,
    mut lobby: ResMut<LobbyState>,
) {
    if lobby.phase == LobbyPhase::InGame {
        return;
    }
    let mut changed = false;
    for ev in wire.read() {
        let client_id_u64 = client_id_to_u64(ev.from);
        if remove_slot(&mut lobby, client_id_u64) {
            changed = true;
        }
    }
    for ev in local.read() {
        if let LocalLobbyCommand::Leave { client_id } = ev {
            if remove_slot(&mut lobby, *client_id) {
                changed = true;
            }
        }
    }
    for ev in disconnects.read() {
        let client_id_u64 = client_id_to_u64(ev.client_id);
        if remove_slot(&mut lobby, client_id_u64) {
            changed = true;
        }
    }
    if changed {
        lobby.bump();
    }
}

fn remove_slot(lobby: &mut LobbyState, client_id: u64) -> bool {
    let before = lobby.slots.len();
    lobby.slots.retain(|s| s.client_id != client_id);
    lobby.slots.len() != before
}

/// Broadcast the current `LobbyState` to every connected client whenever
/// the version bumps past the last sent version. Lobby payload is small
/// (slot count bounded by `max_players`) so we always ship the whole
/// snapshot rather than diffing.
pub fn broadcast_lobby_snapshot_system(
    mut last_sent_version: Local<u32>,
    lobby: Res<LobbyState>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    server_conns: Res<ServerConnections>,
) {
    if lobby.version == *last_sent_version {
        return;
    }
    let snapshot = LobbySnapshot {
        game_name: lobby.config.game_name.clone(),
        world_seed: lobby.config.world_seed,
        era_index: lobby.config.era as u8,
        economy_index: economy_preset_to_index(lobby.config.economy),
        maturity_index: maturity_to_index(lobby.config.maturity),
        max_players: lobby.config.max_players,
        slots: lobby.public_snapshot(),
    };
    // Ship to every connected client.
    for &client in server_conns.by_client.keys() {
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &snapshot,
            NetworkTarget::Single(client),
        );
    }
    *last_sent_version = lobby.version;
}

/// When the lobby's auto-bump pushes phase into `Starting`, allocate a
/// `faction_id` per slot, broadcast `LobbyStartGame`, populate
/// `PendingStarts` + `GameStartOptions` from the lobby config, then flip
/// `GameState` into `Playing`. After this the lobby phase is `InGame`
/// and new joins are rejected (`PendingReconnect` is the only re-entry).
#[allow(clippy::too_many_arguments)]
pub fn start_game_transition_system(
    mut lobby: ResMut<LobbyState>,
    mut conn_mgr: ResMut<ServerConnectionManager>,
    server_conns: Res<ServerConnections>,
    mut pending_starts: ResMut<PendingStarts>,
    mut options: ResMut<GameStartOptions>,
    mut next_state: ResMut<NextState<GameState>>,
    mut world_seed: ResMut<WorldSeed>,
) {
    if lobby.phase != LobbyPhase::Starting {
        return;
    }
    // Allocate faction ids monotonically by slot_id; first slot wins
    // faction 0, next slot 1, etc. Faction 0 historically maps to the
    // player faction in single-player; this preserves that contract for
    // the host's own slot when it's the first ready slot.
    let mut assignments: Vec<LobbySlotAssignment> = Vec::with_capacity(lobby.slots.len());
    let mut player_slots: Vec<PlayerStartSlot> = Vec::with_capacity(lobby.slots.len());
    let mut primary_start: Option<(i32, i32)> = None;
    // Sort by slot_id for deterministic faction allocation.
    let mut slots_sorted: Vec<usize> = (0..lobby.slots.len()).collect();
    slots_sorted.sort_by_key(|&i| lobby.slots[i].slot_id);
    for (faction_idx, &slot_idx) in slots_sorted.iter().enumerate() {
        let slot = &mut lobby.slots[slot_idx];
        let Some(mega) = slot.megachunk else {
            continue;
        };
        let faction_id = faction_idx as u32;
        slot.faction_id = Some(faction_id);
        assignments.push(LobbySlotAssignment {
            slot_id: slot.slot_id,
            client_id: slot.client_id,
            faction_id,
            megachunk: mega,
        });
        let center_tile = megachunk_center_tile(mega);
        if primary_start.is_none()
            && slot.client_id == crate::net::HOST_SERVER_LOCAL_CLIENT_ID
        {
            primary_start = Some(center_tile);
        }
        player_slots.push(PlayerStartSlot {
            slot_id: slot.slot_id,
            player_name: slot.player_name.clone(),
            client_id: slot.client_id,
            megachunk: Some(mega),
            lifestyle: slot.lifestyle,
            ready: slot.ready,
            faction_id: Some(faction_id),
        });
    }
    // Fall back to first slot's start as the camera anchor when the host
    // wasn't itself in the lobby (pure dedicated server case).
    let primary_start = primary_start.or_else(|| {
        assignments
            .first()
            .map(|a| megachunk_center_tile(a.megachunk))
    });

    let start_msg = LobbyStartGame {
        slot_assignments: assignments,
        world_seed: lobby.config.world_seed,
    };
    for &client in server_conns.by_client.keys() {
        let _ = conn_mgr.send_message_to_target::<OrderedReliableChannel, _>(
            &start_msg,
            NetworkTarget::Single(client),
        );
    }

    // Server-side `PendingStarts` + `GameStartOptions` so the OnEnter
    // chain spawns every slot's faction in the right megachunk.
    pending_starts.primary_start = primary_start;
    pending_starts.slots = player_slots;
    options.era = lobby.config.era;
    options.economy = lobby.config.economy;
    options.maturity = lobby.config.maturity;
    world_seed.0 = lobby.config.world_seed;

    lobby.phase = LobbyPhase::InGame;
    next_state.set(GameState::Playing);
    info!(
        "lobby → Playing: {} slots, world_seed {:#x}",
        lobby.slots.len(),
        lobby.config.world_seed
    );
}

fn megachunk_center_tile(megachunk: (i32, i32)) -> (i32, i32) {
    crate::simulation::region::MegaChunkCoord::center_tile(megachunk.0, megachunk.1)
}

fn economy_preset_to_index(preset: EconomyPreset) -> u8 {
    match preset {
        EconomyPreset::Subsistence => 0,
        EconomyPreset::Mixed => 1,
        EconomyPreset::Market => 2,
    }
}

fn maturity_to_index(maturity: StartSettlementMaturity) -> u8 {
    match maturity {
        StartSettlementMaturity::Founder => 0,
        StartSettlementMaturity::Established => 1,
        StartSettlementMaturity::Developed => 2,
    }
}

/// `ClientId` decomposes into a `u64` for our `ServerLobbySlot.client_id`
/// keying. Lightyear's `ClientId` is an enum (`Netcode(u64)` /
/// `Local(u64)` etc.) — we only ever care about the raw inner id. The
/// host's local-transport id is `HOST_SERVER_LOCAL_CLIENT_ID`, derived
/// at `install_host_client` time.
fn client_id_to_u64(client: ClientId) -> u64 {
    match client {
        ClientId::Netcode(id) => id,
        ClientId::Steam(id) => id,
        ClientId::Local(id) => id,
        _ => 0,
    }
}

#[allow(dead_code)]
fn heading_to_facing(heading: u8) -> u8 {
    // `Vehicle.heading` is `0..=3` (back/right/front/left). Map onto the
    // 8-way `FacingDirection` u8 (South/SE/E/NE/N/NW/W/SW). For now the
    // client just decodes cardinal; chariot rendering uses its own
    // `view_for_heading`, not `FacingDirection`.
    match heading & 0b11 {
        0 => 0, // South
        1 => 2, // East
        2 => 4, // North
        3 => 6, // West
        _ => 0,
    }
}

