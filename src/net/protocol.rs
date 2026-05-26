//! Wire-shaped types crossing the client↔server boundary.
//!
//! `PlayerCommand` and its sub-types (`BuildSiteKind`, `WallMaterial`,
//! `VehicleGrid`, `VehicleOrderKind`, `MigrationIntent`, …) derive
//! `Serialize` / `Deserialize` so the inner command can ride a Lightyear
//! reliable channel without an intermediate wire-form translation. We
//! still pass `Entity` for the `actors` field in `Local` mode because the
//! in-process client and server share the same World; remote clients will
//! refer to actors by `NetId` and the server will resolve before stamping
//! `Commanded`.
//!
//! The presence of this event type today is the load-bearing one-path
//! property: even in single-player every UI command crosses the network
//! boundary, so the server-auth path never atrophies.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::net_id::NetId;
use crate::simulation::player_command::PlayerCommand;
use crate::world::water_runtime::RuntimeWaterCell;

/// Wire protocol version negotiated at connect time. Bump whenever the
/// shape of any wire message in this module changes; mismatched clients
/// are rejected by `accept_connections_system` before any state transfer.
pub const PROTOCOL_VERSION: u32 = 5;

/// One UI- (or remote-client-)issued command, scoped to the faction that
/// claims to be sending it. The loopback validates `sender_faction_id`
/// against `ControlledFactions` before producing the `PlayerCommandEvent`.
///
/// `actors` carries actor identities as stable `NetId`s — never raw
/// `Entity`s — so the event is fully wire-serializable. `CommandSender`
/// translates entities to `NetId`s at the send boundary (allocating on
/// the fly via `NetIdMap::lookup_or_alloc` for entities that weren't
/// tagged with `NeedsNetId` at spawn). `command_loopback_system` resolves
/// `NetId`s back to entities when re-emitting `PlayerCommandEvent`;
/// unresolvable ids (entity despawned mid-flight) are dropped silently
/// from the actor list, mirroring how entity-target variants surface
/// `CommandFailure::TargetGone` for the same condition.
#[derive(Event, Debug, Clone)]
pub struct NetPlayerCommandEvent {
    pub sender_faction_id: u32,
    pub actors: Vec<NetId>,
    pub command: PlayerCommand,
}

/// Selects which net role this process plays. Read once at startup from
/// the CLI; resource value is stable for the rest of the session.
///
/// Phase 1 always runs `Local`. Phase 2 wires `ListenServer` /
/// `DedicatedServer` / `Client` to swap transports.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// Single-process, in-memory loopback. Same App runs the sim and the
    /// UI; the network boundary is the `NetPlayerCommandEvent` channel,
    /// not a socket.
    Local,
    /// Listen mode: `Local` + a UDP/QUIC socket accepting remote clients.
    /// Phase 2.
    ListenServer,
    /// Headless server: no `RenderingPlugin` / `UiPlugin`. Phase 2.
    DedicatedServer,
    /// Connect-only: no sim, only render/UI + replication. Phase 2.
    Client,
}

impl Default for NetMode {
    fn default() -> Self {
        NetMode::Local
    }
}

impl NetMode {
    /// `true` if this mode runs the authoritative simulation (drains
    /// commands, mutates `FactionRegistry`, owns `TileChangedEvent`).
    pub fn runs_sim(self) -> bool {
        matches!(
            self,
            NetMode::Local | NetMode::ListenServer | NetMode::DedicatedServer
        )
    }

    /// `true` if this mode accepts remote client connections (binds a
    /// socket via Lightyear). `Local` does not; the client App is in-
    /// process.
    pub fn accepts_remote_clients(self) -> bool {
        matches!(self, NetMode::ListenServer | NetMode::DedicatedServer)
    }

    /// `true` if this mode connects to a remote server. Used by client-
    /// only bootstrap paths and the render/UI app to know it must wait
    /// for `BootstrapSnapshot` before showing the world.
    pub fn is_client(self) -> bool {
        matches!(self, NetMode::Client)
    }
}

// ============================================================================
// Phase 2b wire messages
// ============================================================================

/// Monotonic per-connection identifier for an issued `PlayerCommand`. The
/// client stamps a fresh `CommandId` on every `NetPlayerCommandEvent` and
/// the server's `NetCommandAck` echoes the same id so the originator can
/// match acks to requests. Wraps `u32`; rolls over after ~4B issued
/// commands per connection (we'll have reconnected long before then).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandId(pub u32);

/// First message a client sends after a Lightyear connection establishes.
/// The server validates `protocol_version` before any state transfer and
/// disconnects on mismatch with a logged reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    pub player_name: String,
}

/// Server's reply to `ClientHello`. Names the faction this client owns
/// for the session plus the world seed the client needs to deterministically
/// regenerate `Globe` + `ChunkMap` locally (avoids transferring the whole
/// world over the wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactionAssignment {
    pub faction_id: u32,
    pub world_seed: u64,
}

/// One operation in a `ChunkOverlayDelta` batch. Mirrors the durable
/// truth maps (`WallMap`/`DoorMap`/`BridgeMap`/`DamMap`/`RuntimeWater`)
/// the snapshot helpers in `snapshot.rs` produce; the client applies these
/// via the same `apply_*_snapshot` paths.
///
/// `Add*` variants carry the entity's `NetId` so the client can map back
/// to its locally-spawned stub. `Remove*` is keyed purely on tile because
/// the structure is gone — id resolution would be pointless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TileOverlayOp {
    AddWall {
        tile: (i32, i32),
        entity_net_id: NetId,
        /// Owner faction of the constructed wall, or `None` for natural
        /// bedrock fallback. Clients need this to stamp a faux `Wall`
        /// component on the stub so the client-side `fog_update_system`
        /// can treat owner-own walls as transparent to vision via
        /// `has_vision_los`. Bumped `PROTOCOL_VERSION` to 3.
        owner_faction: Option<u32>,
    },
    RemoveWall {
        tile: (i32, i32),
    },
    AddDoor {
        tile: (i32, i32),
        entity_net_id: NetId,
        open: bool,
        faction_id: u32,
    },
    RemoveDoor {
        tile: (i32, i32),
    },
    SetDoorOpen {
        tile: (i32, i32),
        open: bool,
    },
    AddBridge {
        tile: (i32, i32),
        entity_net_id: NetId,
    },
    RemoveBridge {
        tile: (i32, i32),
    },
    AddDam {
        tile: (i32, i32),
        entity_net_id: NetId,
    },
    RemoveDam {
        tile: (i32, i32),
    },
    SetRuntimeWater {
        tile: (i32, i32),
        cell: RuntimeWaterCell,
    },
    ClearRuntimeWater {
        tile: (i32, i32),
    },
}

/// Batched ops for one chunk's worth of tile-overlay changes. The server
/// coalesces every `TileChangedEvent` since the last replication tick into
/// these per-chunk batches and broadcasts to interested clients. Empty
/// `ops` is never sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkOverlayDelta {
    pub chunk: (i32, i32),
    pub ops: Vec<TileOverlayOp>,
}

/// Coarse summary of one faction visible on the world map; used in the
/// bootstrap so the client can render the diplomacy / world-overview UI
/// before any chunks stream in. Per-tick faction state arrives via
/// future-Phase entity replication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactionSummary {
    pub faction_id: u32,
    pub home_tile: (i32, i32),
    pub member_count: u32,
    pub treasury: f32,
    pub materialized: bool,
    pub parent_faction: Option<u32>,
}

/// Coarse summary of one settlement; bootstrap-only so the client's
/// world-map UI has anchor points. `name` is short — the wire cost is
/// bounded by `MAX_SETTLEMENTS_IN_BOOTSTRAP`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementSummary {
    pub settlement_id: u32,
    pub owner_faction: u32,
    pub name: String,
    pub market_tile: (i32, i32),
    pub treasury: f32,
    pub peak_population: u32,
}

/// Snapshot the server sends right after `FactionAssignment` so the
/// client can boot the world. Includes:
///
/// - `calendar` (season/day/year) so calendar-driven UI is correct on tick 1.
/// - `factions` / `settlements` for world-map / diplomacy panels.
/// - `overlay_tiles` (walls/doors/bridges/dams/runtime water) keyed by
///   `NetId` so the client's local maps match the server's source of truth.
/// - `interest_chunks` lists the chunks the server will start streaming
///   replication for (within `INTEREST_RADIUS_CHUNKS` of the owned
///   faction's home / camera focus).
///
/// Once applied, the client unblocks rendering and starts processing
/// per-tick replication messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapSnapshot {
    pub server_tick: u64,
    pub calendar: CalendarWire,
    pub factions: Vec<FactionSummary>,
    pub settlements: Vec<SettlementSummary>,
    pub controlled_factions: Vec<u32>,
    pub overlay_tiles: OverlayTileSnapshot,
    pub interest_chunks: Vec<(i32, i32)>,
}

/// Wire-shaped mirror of `world::seasons::Calendar` (it's not serde-
/// derive-able directly because Bevy's `Resource` macro doesn't cover
/// serde). Constructor + apply helpers in `bootstrap.rs`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CalendarWire {
    pub season: u8,
    pub day: u32,
    pub ticks_this_day: u32,
    pub ticks_per_day: u32,
    pub days_per_season: u32,
    pub year: u32,
}

/// Bootstrap payload mirroring every tile-overlay map. The server packs
/// each via the `snapshot.rs` helpers; the client `apply_*_snapshot`s
/// after spawning stubs for every `entity_net_id` it sees.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OverlayTileSnapshot {
    pub walls: Vec<WireWallEntry>,
    pub doors: Vec<WireDoorEntry>,
    pub bridges: Vec<WireBridgeEntry>,
    pub dams: Vec<WireDamEntry>,
    pub runtime_water: Vec<WireWaterEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireWallEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
    /// Constructed wall's owner faction (`None` for natural bedrock).
    /// Bumped `PROTOCOL_VERSION` to 3. Required by the client-side
    /// `fog_update_system` so own walls don't block own LOS.
    pub owner_faction: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireDoorEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
    pub open: bool,
    pub faction_id: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireBridgeEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireDamEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireWaterEntry {
    pub tile: (i32, i32),
    pub cell: RuntimeWaterCell,
}

/// Server's reply to a client-issued `NetPlayerCommandEvent`. The
/// originator matches on `command_id`. `status` carries the validation
/// outcome; `reason` is human-readable detail for logging / UI feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetCommandAck {
    pub command_id: CommandId,
    pub status: NetCommandAckStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetCommandAckStatus {
    /// Command passed `command_loopback_system` validation and was
    /// emitted as `PlayerCommandEvent`. Does NOT mean the order
    /// completed — only that the server accepted the request.
    Accepted,
    /// Sender claimed a faction it doesn't control.
    OwnershipRejected,
    /// Command shape was valid but every actor NetId failed to resolve.
    /// Faction-level commands (empty actors) never see this.
    AllActorsGone,
}

/// Sentinel for "no client connected to this command" — used in `Local`
/// mode where there's no remote originator to ack back to.
impl CommandId {
    pub const LOCAL: CommandId = CommandId(0);
}

/// One client-to-server command frame; bundles the `NetPlayerCommandEvent`
/// payload with a `CommandId` so the server can ack. Sent on the
/// `OrderedReliable` channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetCommandFrame {
    pub command_id: CommandId,
    pub sender_faction_id: u32,
    pub actors: Vec<NetId>,
    pub command: PlayerCommand,
}

// ============================================================================
// Phase 3a — per-tick entity state replication
// ============================================================================

/// Wire-side discriminant for replicated entity kinds. The client uses this
/// to drive stub composition — picking sprite category, fog-source policy,
/// and which subset of `EntityStateEntry` fields it should believe. Adding a
/// new kind is a protocol bump (`PROTOCOL_VERSION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityKindWire {
    Person,
    Animal(AnimalSpeciesWire),
    Vehicle,
}

/// Mirrors the marker-component-derived species set the server picks for
/// `EntityKindWire::Animal`. `Dog` covers tamed wolves; wild wolves still
/// ride as `Wolf` so client-side combat-AI can distinguish predator-from-pet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnimalSpeciesWire {
    Wolf,
    Deer,
    Horse,
    Cow,
    Pig,
    Cat,
}

/// One entity's snapshot for a given replication tick. Fields are dense
/// (no `Option`) to keep deserialise branches predictable — the cost of one
/// zero byte beats per-field tag overhead at our entity counts. `facing`
/// is the `FacingDirection as u8` (0..=7 round-robin); the client lossily
/// maps anything ≥ 8 back to `South`. `faction_id == 0` is the unfactioned
/// sentinel (e.g. wild animals).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EntityStateEntry {
    pub net_id: NetId,
    pub kind: EntityKindWire,
    pub tile: (i32, i32),
    pub z: i8,
    pub facing: u8,
    pub health_current: u16,
    pub health_max: u16,
    pub faction_id: u32,
}

/// Batched entity state for one chunk. The server groups entries by
/// `tile_to_chunk_coord(entity_tile)` so Phase 3b's interest rooms can
/// gate the entire batch on per-client chunk membership without re-sorting.
/// `entries` is never empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityStateDelta {
    pub server_tick: u64,
    pub chunk: (i32, i32),
    pub entries: Vec<EntityStateEntry>,
}

/// Client→Server hint telling the server where this client's camera is
/// currently looking. Server folds this into the interest computation so
/// scouting expeditions beyond the settlement ring still pull live
/// replication. Sent periodically (only when the focus tile has crossed a
/// chunk boundary since the last send) to keep wire chatter minimal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClientCameraFocus {
    pub tile: (i32, i32),
}

/// Server→Client notice that one or more `Networked` entities despawned
/// authoritatively. Without this, client stubs spawned by
/// `apply_entity_state_delta_system` would linger forever (the client has
/// no other signal for "the server entity is gone"). v1 broadcasts to
/// every connected client; interest gating for removes is deferred — a
/// stray remove on a client that never saw the entity is a no-op lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRemoved {
    pub net_ids: Vec<NetId>,
}

#[cfg(test)]
mod tests {
    use crate::net_id::NetId;
    use crate::simulation::construction::{BuildSiteKind, WallMaterial};
    use crate::simulation::faction::{MigrationIntent, PackedMigrationAutonomy};
    use crate::simulation::player_command::PlayerCommand;
    use crate::simulation::vehicle::{
        VehicleCell, VehicleGrid, VehicleModuleInstance, VehicleModuleId, VehicleModuleDefId,
        VehicleOrderKind, VehiclePartKind, VehiclePurpose,
    };
    use crate::economy::resource_catalog::ResourceId;
    use bevy::math::IVec3;

    fn round_trip(cmd: &PlayerCommand) -> PlayerCommand {
        let bytes = bincode::serialize(cmd).expect("serialize");
        bincode::deserialize(&bytes).expect("deserialize")
    }

    #[test]
    fn move_round_trips() {
        let original = PlayerCommand::Move {
            tile: (12, -3),
            z: 4,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::Move { tile, z } => {
                assert_eq!(tile, (12, -3));
                assert_eq!(z, 4);
            }
            other => panic!("expected Move, got {:?}", other),
        }
    }

    #[test]
    fn netid_entity_target_round_trips() {
        let original = PlayerCommand::PickUpItem {
            item: NetId(42),
            tile: (1, 2),
            z: 0,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::PickUpItem { item, tile, z } => {
                assert_eq!(item, NetId(42));
                assert_eq!(tile, (1, 2));
                assert_eq!(z, 0);
            }
            other => panic!("expected PickUpItem, got {:?}", other),
        }
    }

    #[test]
    fn build_with_wall_material_round_trips() {
        let original = PlayerCommand::Build {
            kind: BuildSiteKind::Wall(WallMaterial::Mudbrick),
            tile: (5, 5),
            z: 0,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::Build {
                kind: BuildSiteKind::Wall(WallMaterial::Mudbrick),
                tile,
                z,
            } => {
                assert_eq!(tile, (5, 5));
                assert_eq!(z, 0);
            }
            other => panic!("expected Build(Wall(Mudbrick)), got {:?}", other),
        }
    }

    #[test]
    fn custom_vehicle_with_grid_round_trips() {
        let mut grid = VehicleGrid::default();
        grid.cells.push((
            IVec3::new(0, 0, 0),
            VehicleCell::plain(VehiclePartKind::Frame, ResourceId(7), 100),
        ));
        grid.modules.push(VehicleModuleInstance {
            id: VehicleModuleId(0),
            def: VehicleModuleDefId(3),
            cells: vec![IVec3::new(0, 0, 0)],
            facing: 1,
        });

        let original = PlayerCommand::QueueCustomVehicle {
            name: "Prototype".to_string(),
            grid,
            purpose: VehiclePurpose::Cargo,
            required_animals: 2,
            faction_id: 5,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::QueueCustomVehicle {
                name,
                grid,
                purpose,
                required_animals,
                faction_id,
            } => {
                assert_eq!(name, "Prototype");
                assert_eq!(grid.cells.len(), 1);
                assert_eq!(grid.modules.len(), 1);
                assert_eq!(grid.modules[0].cells, vec![IVec3::new(0, 0, 0)]);
                assert_eq!(purpose, VehiclePurpose::Cargo);
                assert_eq!(required_animals, 2);
                assert_eq!(faction_id, 5);
            }
            other => panic!("expected QueueCustomVehicle, got {:?}", other),
        }
    }

    #[test]
    fn vehicle_order_round_trips() {
        let original = PlayerCommand::VehicleOrder {
            vehicle: NetId(99),
            kind: VehicleOrderKind::SiegeWall((-4, 7)),
            faction_id: 1,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::VehicleOrder { vehicle, kind, faction_id } => {
                assert_eq!(vehicle, NetId(99));
                assert_eq!(kind, VehicleOrderKind::SiegeWall((-4, 7)));
                assert_eq!(faction_id, 1);
            }
            other => panic!("expected VehicleOrder, got {:?}", other),
        }
    }

    #[test]
    fn migration_intent_round_trips() {
        let original = PlayerCommand::SetMigrationIntent {
            intent: MigrationIntent::SeekWinterShelter,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::SetMigrationIntent { intent } => {
                assert_eq!(intent, MigrationIntent::SeekWinterShelter);
            }
            other => panic!("expected SetMigrationIntent, got {:?}", other),
        }
    }

    #[test]
    fn client_hello_round_trips() {
        use super::ClientHello;
        let original = ClientHello {
            protocol_version: super::PROTOCOL_VERSION,
            player_name: "Alice".to_string(),
        };
        let bytes = bincode::serialize(&original).expect("serialize");
        let restored: ClientHello = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.protocol_version, super::PROTOCOL_VERSION);
        assert_eq!(restored.player_name, "Alice");
    }

    #[test]
    fn faction_assignment_round_trips() {
        use super::FactionAssignment;
        let original = FactionAssignment {
            faction_id: 4,
            world_seed: 0xDEAD_BEEF_CAFE_F00D,
        };
        let bytes = bincode::serialize(&original).expect("serialize");
        let restored: FactionAssignment = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.faction_id, 4);
        assert_eq!(restored.world_seed, 0xDEAD_BEEF_CAFE_F00D);
    }

    #[test]
    fn tile_overlay_ops_round_trip() {
        use super::TileOverlayOp;
        use crate::world::water_runtime::RuntimeWaterCell;
        let ops = vec![
            TileOverlayOp::AddWall {
                tile: (3, 4),
                entity_net_id: NetId(11),
                owner_faction: Some(7),
            },
            TileOverlayOp::RemoveWall { tile: (3, 4) },
            TileOverlayOp::AddDoor {
                tile: (5, 6),
                entity_net_id: NetId(22),
                open: true,
                faction_id: 7,
            },
            TileOverlayOp::SetDoorOpen {
                tile: (5, 6),
                open: false,
            },
            TileOverlayOp::AddBridge {
                tile: (1, 2),
                entity_net_id: NetId(33),
            },
            TileOverlayOp::AddDam {
                tile: (8, 9),
                entity_net_id: NetId(44),
            },
            TileOverlayOp::SetRuntimeWater {
                tile: (10, -2),
                cell: RuntimeWaterCell {
                    ground_z: -3,
                    depth: 1.5,
                    reservoir_id: u32::MAX,
                    salinity: 0.0,
                    source_rate: 0.01,
                },
            },
            TileOverlayOp::ClearRuntimeWater { tile: (10, -2) },
        ];
        let bytes = bincode::serialize(&ops).expect("serialize");
        let restored: Vec<TileOverlayOp> = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.len(), ops.len());
    }

    #[test]
    fn chunk_overlay_delta_round_trips() {
        use super::{ChunkOverlayDelta, TileOverlayOp};
        let delta = ChunkOverlayDelta {
            chunk: (-3, 7),
            ops: vec![TileOverlayOp::RemoveWall { tile: (-90, 224) }],
        };
        let bytes = bincode::serialize(&delta).expect("serialize");
        let restored: ChunkOverlayDelta = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.chunk, (-3, 7));
        assert_eq!(restored.ops.len(), 1);
    }

    #[test]
    fn bootstrap_snapshot_round_trips() {
        use super::{
            BootstrapSnapshot, CalendarWire, FactionSummary, OverlayTileSnapshot,
            SettlementSummary, WireWallEntry,
        };
        let snap = BootstrapSnapshot {
            server_tick: 12345,
            calendar: CalendarWire {
                season: 1,
                day: 12,
                ticks_this_day: 800,
                ticks_per_day: crate::world::seasons::TICKS_PER_DAY,
                days_per_season: 30,
                year: 3,
            },
            factions: vec![FactionSummary {
                faction_id: 2,
                home_tile: (-100, 50),
                member_count: 18,
                treasury: 124.5,
                materialized: true,
                parent_faction: None,
            }],
            settlements: vec![SettlementSummary {
                settlement_id: 5,
                owner_faction: 2,
                name: "Founders' Camp".to_string(),
                market_tile: (-95, 52),
                treasury: 22.0,
                peak_population: 18,
            }],
            controlled_factions: vec![2],
            overlay_tiles: OverlayTileSnapshot {
                walls: vec![WireWallEntry {
                    tile: (-99, 51),
                    entity_net_id: NetId(7),
                    owner_faction: Some(2),
                }],
                ..Default::default()
            },
            interest_chunks: vec![(-4, 1), (-4, 2), (-3, 1)],
        };
        let bytes = bincode::serialize(&snap).expect("serialize");
        let restored: BootstrapSnapshot = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.server_tick, 12345);
        assert_eq!(restored.calendar.year, 3);
        assert_eq!(restored.factions.len(), 1);
        assert_eq!(restored.settlements[0].name, "Founders' Camp");
        assert_eq!(restored.overlay_tiles.walls.len(), 1);
        assert_eq!(restored.interest_chunks.len(), 3);
    }

    #[test]
    fn net_command_ack_round_trips() {
        use super::{CommandId, NetCommandAck, NetCommandAckStatus};
        let ack = NetCommandAck {
            command_id: CommandId(42),
            status: NetCommandAckStatus::OwnershipRejected,
            reason: Some("faction 99 not controlled here".to_string()),
        };
        let bytes = bincode::serialize(&ack).expect("serialize");
        let restored: NetCommandAck = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.command_id, CommandId(42));
        assert_eq!(restored.status, NetCommandAckStatus::OwnershipRejected);
        assert!(restored.reason.is_some());
    }

    #[test]
    fn net_command_frame_round_trips() {
        use super::{CommandId, NetCommandFrame};
        let frame = NetCommandFrame {
            command_id: CommandId(7),
            sender_faction_id: 3,
            actors: vec![NetId(11), NetId(12)],
            command: PlayerCommand::Move {
                tile: (1, 2),
                z: 0,
            },
        };
        let bytes = bincode::serialize(&frame).expect("serialize");
        let restored: NetCommandFrame = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.command_id, CommandId(7));
        assert_eq!(restored.sender_faction_id, 3);
        assert_eq!(restored.actors, vec![NetId(11), NetId(12)]);
    }

    #[test]
    fn client_camera_focus_round_trips() {
        use super::ClientCameraFocus;
        let m = ClientCameraFocus {
            tile: (-1234, 5678),
        };
        let bytes = bincode::serialize(&m).expect("serialize");
        let restored: ClientCameraFocus = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.tile, (-1234, 5678));
    }

    #[test]
    fn entity_removed_round_trips() {
        use super::EntityRemoved;
        let msg = EntityRemoved {
            net_ids: vec![NetId(7), NetId(99), NetId(123_456)],
        };
        let bytes = bincode::serialize(&msg).expect("serialize");
        let restored: EntityRemoved = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.net_ids.len(), 3);
        assert_eq!(restored.net_ids[2], NetId(123_456));
    }

    #[test]
    fn entity_state_delta_round_trips() {
        use super::{
            AnimalSpeciesWire, EntityKindWire, EntityStateDelta, EntityStateEntry,
        };
        let delta = EntityStateDelta {
            server_tick: 9001,
            chunk: (-3, 5),
            entries: vec![
                EntityStateEntry {
                    net_id: NetId(101),
                    kind: EntityKindWire::Person,
                    tile: (-90, 160),
                    z: 0,
                    facing: 3,
                    health_current: 87,
                    health_max: 100,
                    faction_id: 2,
                },
                EntityStateEntry {
                    net_id: NetId(202),
                    kind: EntityKindWire::Animal(AnimalSpeciesWire::Wolf),
                    tile: (-91, 161),
                    z: 0,
                    facing: 7,
                    health_current: 24,
                    health_max: 60,
                    faction_id: 0,
                },
                EntityStateEntry {
                    net_id: NetId(303),
                    kind: EntityKindWire::Vehicle,
                    tile: (-88, 162),
                    z: 1,
                    facing: 1,
                    health_current: 200,
                    health_max: 200,
                    faction_id: 2,
                },
            ],
        };
        let bytes = bincode::serialize(&delta).expect("serialize");
        let restored: EntityStateDelta = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.server_tick, 9001);
        assert_eq!(restored.chunk, (-3, 5));
        assert_eq!(restored.entries.len(), 3);
        assert!(matches!(restored.entries[0].kind, EntityKindWire::Person));
        assert!(matches!(
            restored.entries[1].kind,
            EntityKindWire::Animal(AnimalSpeciesWire::Wolf)
        ));
        assert!(matches!(restored.entries[2].kind, EntityKindWire::Vehicle));
        assert_eq!(restored.entries[2].tile, (-88, 162));
        assert_eq!(restored.entries[2].z, 1);
    }

    #[test]
    fn packed_autonomy_round_trips() {
        let original = PlayerCommand::SetPackedAutonomy {
            mode: PackedMigrationAutonomy::Forage,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::SetPackedAutonomy { mode } => {
                assert_eq!(mode, PackedMigrationAutonomy::Forage);
            }
            other => panic!("expected SetPackedAutonomy, got {:?}", other),
        }
    }

    #[test]
    fn diplomacy_proposal_round_trips() {
        use crate::simulation::diplomacy::DiplomacyProposal;
        let original = PlayerCommand::SendDiplomacyProposal {
            faction_id: 3,
            target_faction_id: 5,
            proposal: DiplomacyProposal::OfferAlliance,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::SendDiplomacyProposal {
                faction_id,
                target_faction_id,
                proposal,
            } => {
                assert_eq!(faction_id, 3);
                assert_eq!(target_faction_id, 5);
                assert_eq!(proposal, DiplomacyProposal::OfferAlliance);
            }
            other => panic!("expected SendDiplomacyProposal, got {:?}", other),
        }
    }

    #[test]
    fn diplomacy_response_round_trips() {
        use crate::simulation::diplomacy::{ProposalId, ProposalResponse};
        let original = PlayerCommand::RespondDiplomacyProposal {
            faction_id: 2,
            proposal_id: ProposalId(99),
            response: ProposalResponse::Accept,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::RespondDiplomacyProposal {
                faction_id,
                proposal_id,
                response,
            } => {
                assert_eq!(faction_id, 2);
                assert_eq!(proposal_id, ProposalId(99));
                assert_eq!(response, ProposalResponse::Accept);
            }
            other => panic!("expected RespondDiplomacyProposal, got {:?}", other),
        }
    }

    #[test]
    fn declare_war_round_trips() {
        let original = PlayerCommand::DeclareWar {
            faction_id: 1,
            target_faction_id: 4,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::DeclareWar {
                faction_id,
                target_faction_id,
            } => {
                assert_eq!(faction_id, 1);
                assert_eq!(target_faction_id, 4);
            }
            other => panic!("expected DeclareWar, got {:?}", other),
        }
    }

    #[test]
    fn break_treaty_round_trips() {
        use crate::simulation::diplomacy::TreatyKind;
        let original = PlayerCommand::BreakTreaty {
            faction_id: 1,
            target_faction_id: 4,
            treaty: TreatyKind::TradePact,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::BreakTreaty {
                faction_id,
                target_faction_id,
                treaty,
            } => {
                assert_eq!(faction_id, 1);
                assert_eq!(target_faction_id, 4);
                assert_eq!(treaty, TreatyKind::TradePact);
            }
            other => panic!("expected BreakTreaty, got {:?}", other),
        }
    }

    #[test]
    fn revoke_access_grant_round_trips() {
        use crate::simulation::access_grant::AccessKind;
        use crate::simulation::settlement::SettlementId;
        let original = PlayerCommand::RevokeAccessGrant {
            faction_id: 1,
            target_faction_id: 4,
            kind: AccessKind::MarketCorridor {
                settlement_id: SettlementId(7),
                radius: 6,
            },
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::RevokeAccessGrant {
                faction_id,
                target_faction_id,
                kind,
            } => {
                assert_eq!(faction_id, 1);
                assert_eq!(target_faction_id, 4);
                match kind {
                    AccessKind::MarketCorridor { settlement_id, radius } => {
                        assert_eq!(settlement_id, SettlementId(7));
                        assert_eq!(radius, 6);
                    }
                    other => panic!("expected MarketCorridor, got {:?}", other),
                }
            }
            other => panic!("expected RevokeAccessGrant, got {:?}", other),
        }
    }

    #[test]
    fn send_diplomacy_deal_package_round_trips() {
        use crate::simulation::diplomacy::{DealTerm, Direction, TreatyKind};
        let original = PlayerCommand::SendDiplomacyDealPackage {
            faction_id: 2,
            target_faction_id: 7,
            terms: vec![
                DealTerm::TreatyForm(TreatyKind::TradePact),
                DealTerm::ResourceTransfer {
                    resource_id: 5,
                    qty: 12,
                    direction: Direction::FromProposerToReceiver,
                },
                DealTerm::CurrencyTransfer {
                    amount: 30,
                    direction: Direction::FromReceiverToProposer,
                },
            ],
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::SendDiplomacyDealPackage {
                faction_id,
                target_faction_id,
                terms,
            } => {
                assert_eq!(faction_id, 2);
                assert_eq!(target_faction_id, 7);
                assert_eq!(terms.len(), 3);
            }
            other => panic!("expected SendDiplomacyDealPackage, got {:?}", other),
        }
    }

    #[test]
    fn respond_diplomacy_deal_package_round_trips() {
        use crate::simulation::diplomacy::{DealId, ProposalResponse};
        let original = PlayerCommand::RespondDiplomacyDealPackage {
            faction_id: 3,
            deal_id: DealId(42),
            response: ProposalResponse::Accept,
        };
        let restored = round_trip(&original);
        match restored {
            PlayerCommand::RespondDiplomacyDealPackage {
                faction_id,
                deal_id,
                response,
            } => {
                assert_eq!(faction_id, 3);
                assert_eq!(deal_id, DealId(42));
                assert_eq!(response, ProposalResponse::Accept);
            }
            other => panic!("expected RespondDiplomacyDealPackage, got {:?}", other),
        }
    }
}
