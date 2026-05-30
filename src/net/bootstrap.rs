//! Bootstrap snapshot construction + apply helpers.
//!
//! When a client connects to a server (`Client` mode against `ListenServer`
//! / `DedicatedServer`), the server packs the current world state into
//! `BootstrapSnapshot` so the client can boot the world without streaming
//! every chunk live. The construction half (`build_bootstrap_snapshot`)
//! runs server-side; the apply half (`apply_bootstrap_snapshot`) runs
//! client-side after Lightyear has delivered the message.
//!
//! Design notes:
//!
//! - Calendar / faction summaries are tiny and ship verbatim.
//! - Tile-overlay maps go through the `snapshot.rs` helpers — same shape
//!   the per-tick delta replication uses, so the client's apply path is
//!   one codepath.
//! - The world itself is *not* in the snapshot — only the deterministic
//!   `WorldSeed` is shipped (in `FactionAssignment`) so the client
//!   regenerates `Globe` + `ChunkMap` locally. This trades CPU for ~MBs
//!   of bandwidth on every connect.
//!
//! See `plans/multiplayer.md` Phase 2b for the full message catalog.

use bevy::prelude::*;

use crate::net::protocol::{
    BootstrapSnapshot, CalendarWire, FactionSummary, OverlayTileSnapshot, SettlementSummary,
    WireFederationEntry,
    WireBridgeEntry, WireDamEntry, WireDoorEntry, WireWallEntry, WireWaterEntry,
};
use crate::net_id::{NetIdMap, Networked};
use crate::simulation::construction::{BridgeMap, DamMap, DoorEntry, DoorMap, WallMap};
use crate::simulation::faction::{ControlledFactions, FactionRegistry};
use crate::simulation::settlement::{Settlement, SettlementMap};
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
use crate::world::seasons::{Calendar, Season};
use crate::world::water_runtime::RuntimeWater;

/// Cap on the bootstrap message to keep wire size bounded; the rest
/// streams via per-tick replication. Tuned so a typical mid-game world
/// fits comfortably under a single reliable message.
pub const MAX_FACTIONS_IN_BOOTSTRAP: usize = 64;
pub const MAX_SETTLEMENTS_IN_BOOTSTRAP: usize = 128;

/// Interest radius (in chunks) around a faction home; used to seed
/// `BootstrapSnapshot.interest_chunks`. Matches the runtime replication
/// system's `INTEREST_RADIUS_CHUNKS` so the client doesn't see stutter on
/// the first tick when the per-tick replicator takes over.
pub const INTEREST_RADIUS_CHUNKS: i32 = 4;

/// Pack the live world into a `BootstrapSnapshot` for the connecting
/// client. The caller passes the faction(s) this client controls so we
/// can scope `interest_chunks` and `controlled_factions` correctly.
///
/// Pure-ish: reads resources + a `Networked` query but produces no
/// side effects, so it's safe to call from any exclusive system.
#[allow(clippy::too_many_arguments)]
pub fn build_bootstrap_snapshot(
    server_tick: u64,
    controlled: &[u32],
    calendar: &Calendar,
    factions: &FactionRegistry,
    settlement_map: &SettlementMap,
    settlement_q: &Query<&Settlement>,
    wall_map: &WallMap,
    door_map: &DoorMap,
    bridge_map: &BridgeMap,
    dam_map: &DamMap,
    runtime_water: &RuntimeWater,
    edge_structures: &crate::simulation::construction::EdgeStructureMap,
    networked_q: &Query<&Networked>,
    wall_q: &Query<&crate::simulation::construction::Wall>,
    federation_map: &crate::simulation::federation::FederationMap,
) -> BootstrapSnapshot {
    let calendar_wire = CalendarWire {
        season: calendar.season as u8,
        day: calendar.day,
        ticks_this_day: calendar.ticks_this_day,
        ticks_per_day: calendar.ticks_per_day,
        days_per_season: calendar.days_per_season,
        year: calendar.year,
    };

    let mut faction_list: Vec<FactionSummary> = factions
        .factions
        .iter()
        .take(MAX_FACTIONS_IN_BOOTSTRAP)
        .map(|(&fid, data)| FactionSummary {
            faction_id: fid,
            home_tile: data.home_tile,
            member_count: data.member_count,
            treasury: data.treasury,
            materialized: data.materialized,
            parent_faction: data.parent_faction,
        })
        .collect();
    faction_list.sort_by_key(|f| f.faction_id);

    let mut settlement_list: Vec<SettlementSummary> = Vec::new();
    for (&id, &entity) in settlement_map.by_id.iter() {
        if settlement_list.len() >= MAX_SETTLEMENTS_IN_BOOTSTRAP {
            break;
        }
        if let Ok(s) = settlement_q.get(entity) {
            settlement_list.push(SettlementSummary {
                settlement_id: id.0,
                owner_faction: s.owner_faction,
                name: s.name.clone(),
                market_tile: s.market_tile,
                treasury: s.treasury,
                peak_population: s.peak_population,
            });
        }
    }
    settlement_list.sort_by_key(|s| s.settlement_id);

    let overlay_tiles = OverlayTileSnapshot {
        walls: collect_wall_entries(wall_map, networked_q, wall_q),
        doors: collect_door_entries(door_map, networked_q),
        bridges: collect_bridge_entries(bridge_map, networked_q),
        dams: collect_dam_entries(dam_map, networked_q),
        runtime_water: collect_water_entries(runtime_water),
        edge_walls: collect_edge_wall_entries(edge_structures, networked_q),
        edge_doors: collect_edge_door_entries(edge_structures, networked_q),
    };

    let interest_chunks =
        compute_interest_chunks(controlled, factions, INTEREST_RADIUS_CHUNKS);

    let mut federation_list: Vec<WireFederationEntry> = federation_map
        .by_id
        .values()
        .map(|f| WireFederationEntry {
            federation_id: f.id.0,
            name: f.name.clone(),
            members: f.members.clone(),
            founder: f.founder,
            founded_tick: f.founded_tick,
        })
        .collect();
    federation_list.sort_by_key(|f| f.federation_id);

    BootstrapSnapshot {
        server_tick,
        calendar: calendar_wire,
        factions: faction_list,
        settlements: settlement_list,
        controlled_factions: controlled.to_vec(),
        overlay_tiles,
        interest_chunks,
        federations: federation_list,
    }
}

fn collect_wall_entries(
    map: &WallMap,
    q: &Query<&Networked>,
    walls: &Query<&crate::simulation::construction::Wall>,
) -> Vec<WireWallEntry> {
    map.0
        .iter()
        .filter_map(|(tile, &entity)| {
            q.get(entity).ok().map(|n| WireWallEntry {
                tile: *tile,
                entity_net_id: n.0,
                owner_faction: walls.get(entity).ok().and_then(|w| w.owner_faction),
            })
        })
        .collect()
}

fn collect_door_entries(map: &DoorMap, q: &Query<&Networked>) -> Vec<WireDoorEntry> {
    map.0
        .iter()
        .filter_map(|(tile, entry)| {
            q.get(entry.entity).ok().map(|n| WireDoorEntry {
                tile: *tile,
                entity_net_id: n.0,
                open: entry.open,
                faction_id: entry.faction_id,
            })
        })
        .collect()
}

fn collect_bridge_entries(map: &BridgeMap, q: &Query<&Networked>) -> Vec<WireBridgeEntry> {
    map.0
        .iter()
        .filter_map(|(tile, &entity)| {
            q.get(entity).ok().map(|n| WireBridgeEntry {
                tile: *tile,
                entity_net_id: n.0,
            })
        })
        .collect()
}

fn collect_dam_entries(map: &DamMap, q: &Query<&Networked>) -> Vec<WireDamEntry> {
    map.0
        .iter()
        .filter_map(|(tile, &entity)| {
            q.get(entity).ok().map(|n| WireDamEntry {
                tile: *tile,
                entity_net_id: n.0,
            })
        })
        .collect()
}

fn collect_edge_wall_entries(
    edges: &crate::simulation::construction::EdgeStructureMap,
    q: &Query<&Networked>,
) -> Vec<crate::net::protocol::WireEdgeWallEntry> {
    edges
        .0
        .iter()
        .filter_map(|(&edge, entry)| {
            let w = entry.wall.as_ref()?;
            q.get(w.entity).ok().map(|n| crate::net::protocol::WireEdgeWallEntry {
                edge,
                entity_net_id: n.0,
                material: w.material,
                owner_faction: w.owner_faction,
            })
        })
        .collect()
}

fn collect_edge_door_entries(
    edges: &crate::simulation::construction::EdgeStructureMap,
    q: &Query<&Networked>,
) -> Vec<crate::net::protocol::WireEdgeDoorEntry> {
    edges
        .0
        .iter()
        .filter_map(|(&edge, entry)| {
            let d = entry.door.as_ref()?;
            q.get(d.entity).ok().map(|n| crate::net::protocol::WireEdgeDoorEntry {
                edge,
                entity_net_id: n.0,
                open: d.open,
                faction_id: d.faction_id,
                dir: d.dir,
            })
        })
        .collect()
}

fn collect_water_entries(water: &RuntimeWater) -> Vec<WireWaterEntry> {
    water
        .cells
        .iter()
        .map(|(tile, cell)| WireWaterEntry {
            tile: *tile,
            cell: *cell,
        })
        .collect()
}

/// Enumerate chunks within `radius` of every controlled faction's home.
/// Result deduplicated and sorted for stable on-wire bytes.
pub fn compute_interest_chunks(
    controlled: &[u32],
    factions: &FactionRegistry,
    radius: i32,
) -> Vec<(i32, i32)> {
    let mut out: crate::collections::AHashSet<(i32, i32)> = crate::collections::AHashSet::default();
    for &fid in controlled {
        let Some(faction) = factions.factions.get(&fid) else {
            continue;
        };
        let (hx, hy) = faction.home_tile;
        // Tile→chunk math is pure integer; no need to round-trip through
        // world-space `TILE_SIZE`.
        let home_chunk = ChunkCoord(
            hx.div_euclid(CHUNK_SIZE as i32),
            hy.div_euclid(CHUNK_SIZE as i32),
        );
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                out.insert((home_chunk.0 + dx, home_chunk.1 + dy));
            }
        }
    }
    let mut v: Vec<(i32, i32)> = out.into_iter().collect();
    v.sort();
    v
}

/// Tile-to-chunk derivation that doesn't depend on `TILE_SIZE` — pure
/// integer math so tests don't need rendering glue. Useful for the
/// overlay-delta server when emitting `ChunkOverlayDelta.chunk`.
pub fn tile_to_chunk_coord(tile: (i32, i32)) -> (i32, i32) {
    (
        tile.0.div_euclid(CHUNK_SIZE as i32),
        tile.1.div_euclid(CHUNK_SIZE as i32),
    )
}

/// Client-side apply. Rewrites the calendar, faction registry, controlled
/// factions, and overlay maps from the snapshot. Settlements and entity
/// stubs require entity spawning and are handled by the client systems
/// in `net::client` — this fn covers everything pure-data.
///
/// Caller must have already populated `NetIdMap` with the entity stubs
/// the wall/door/bridge/dam entries refer to (the client systems spawn
/// a placeholder `Entity` per incoming `entity_net_id` and register it
/// before calling this).
pub fn apply_bootstrap_snapshot(
    snap: &BootstrapSnapshot,
    calendar: &mut Calendar,
    controlled: &mut ControlledFactions,
    wall_map: &mut WallMap,
    door_map: &mut DoorMap,
    bridge_map: &mut BridgeMap,
    dam_map: &mut DamMap,
    runtime_water: &mut RuntimeWater,
    edge_structures: &mut crate::simulation::construction::EdgeStructureMap,
    chunk_map: &mut crate::world::chunk::ChunkMap,
    ids: &NetIdMap,
    federation_map: &mut crate::simulation::federation::FederationMap,
) {
    calendar.season = Season::from_index(snap.calendar.season);
    calendar.day = snap.calendar.day;
    calendar.ticks_this_day = snap.calendar.ticks_this_day;
    calendar.ticks_per_day = snap.calendar.ticks_per_day;
    calendar.days_per_season = snap.calendar.days_per_season;
    calendar.year = snap.calendar.year;

    controlled.ids = snap.controlled_factions.clone();

    wall_map.0.clear();
    for entry in &snap.overlay_tiles.walls {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            wall_map.0.insert(entry.tile, entity);
        }
    }

    door_map.0.clear();
    for entry in &snap.overlay_tiles.doors {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            door_map.0.insert(
                entry.tile,
                DoorEntry {
                    entity,
                    open: entry.open,
                    faction_id: entry.faction_id,
                },
            );
        }
    }

    bridge_map.0.clear();
    for entry in &snap.overlay_tiles.bridges {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            bridge_map.0.insert(entry.tile, entity);
        }
    }

    dam_map.0.clear();
    for entry in &snap.overlay_tiles.dams {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            dam_map.0.insert(entry.tile, entity);
        }
    }

    runtime_water.cells.clear();
    for entry in &snap.overlay_tiles.runtime_water {
        runtime_water.cells.insert(entry.tile, entry.cell);
    }

    // Thin housing edge walls/doors. Rebuild the durable `EdgeStructureMap` +
    // stamp the per-chunk edge cache so client fog LOS + edge sprites match the
    // server. Stubs (with `EdgeWallVisual`/`EdgeDoorVisual`) are spawned by the
    // client system before this call, like wall/door entries.
    edge_structures.0.clear();
    for entry in &snap.overlay_tiles.edge_walls {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            let ent = edge_structures.0.entry(entry.edge).or_default();
            ent.wall = Some(crate::simulation::construction::EdgeWall {
                material: entry.material,
                owner_faction: entry.owner_faction,
                entity,
            });
            let st = ent.projected_state();
            chunk_map.set_edge_state(entry.edge, st);
        }
    }
    for entry in &snap.overlay_tiles.edge_doors {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            let ent = edge_structures.0.entry(entry.edge).or_default();
            ent.door = Some(crate::simulation::construction::EdgeDoorRef {
                entity,
                open: entry.open,
                faction_id: entry.faction_id,
                dir: entry.dir,
            });
            let st = ent.projected_state();
            chunk_map.set_edge_state(entry.edge, st);
        }
    }

    federation_map.by_id.clear();
    federation_map.by_root_faction.clear();
    federation_map.invites.clear();
    federation_map.next_id = 0;
    for entry in &snap.federations {
        let fid = crate::simulation::federation::FederationId(entry.federation_id);
        let fed = crate::simulation::federation::Federation {
            id: fid,
            name: entry.name.clone(),
            members: entry.members.clone(),
            founder: entry.founder,
            founded_tick: entry.founded_tick,
            charter: crate::simulation::federation::FederationCharter::default(),
        };
        for &m in &fed.members {
            federation_map.by_root_faction.insert(m, fid);
        }
        federation_map.by_id.insert(fid, fed);
        federation_map.next_id = federation_map.next_id.max(entry.federation_id);
    }
}

impl Season {
    /// Inverse of `Season as u8` cast. Out-of-range values clamp to
    /// `Spring` (defensive — wire mismatches should be caught upstream
    /// by `PROTOCOL_VERSION` rejection).
    pub fn from_index(idx: u8) -> Self {
        match idx {
            0 => Season::Spring,
            1 => Season::Summer,
            2 => Season::Autumn,
            3 => Season::Winter,
            _ => Season::Spring,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn season_round_trips_through_u8() {
        for s in [Season::Spring, Season::Summer, Season::Autumn, Season::Winter] {
            assert_eq!(Season::from_index(s as u8) as u8, s as u8);
        }
    }

    #[test]
    fn season_out_of_range_clamps_to_spring() {
        assert!(matches!(Season::from_index(42), Season::Spring));
    }

    #[test]
    fn tile_to_chunk_coord_handles_negatives() {
        // 32-tile chunks; tile (-1, -1) lives in chunk (-1, -1).
        assert_eq!(tile_to_chunk_coord((-1, -1)), (-1, -1));
        assert_eq!(tile_to_chunk_coord((0, 0)), (0, 0));
        assert_eq!(tile_to_chunk_coord((31, 31)), (0, 0));
        assert_eq!(tile_to_chunk_coord((32, 32)), (1, 1));
        assert_eq!(tile_to_chunk_coord((-32, -32)), (-1, -1));
        assert_eq!(tile_to_chunk_coord((-33, -33)), (-2, -2));
    }

    #[test]
    fn interest_chunks_dedup_and_sort() {
        let mut registry = FactionRegistry::default();
        let id = registry.create_faction((0, 0));
        let chunks = compute_interest_chunks(&[id], &registry, 1);
        // 3×3 around (0,0)
        assert_eq!(chunks.len(), 9);
        assert_eq!(chunks[0], (-1, -1));
        // Sorted ascending
        for w in chunks.windows(2) {
            assert!(w[0] <= w[1]);
        }
    }

    #[test]
    fn interest_chunks_skips_missing_faction() {
        let registry = FactionRegistry::default();
        let chunks = compute_interest_chunks(&[99], &registry, 2);
        assert!(chunks.is_empty());
    }

    #[test]
    fn apply_bootstrap_restores_calendar_and_controlled() {
        let snap = BootstrapSnapshot {
            server_tick: 0,
            calendar: CalendarWire {
                season: 2, // Autumn
                day: 17,
                ticks_this_day: 1200,
                ticks_per_day: crate::world::seasons::TICKS_PER_DAY,
                days_per_season: 30,
                year: 5,
            },
            factions: Vec::new(),
            settlements: Vec::new(),
            controlled_factions: vec![3, 4],
            overlay_tiles: OverlayTileSnapshot::default(),
            interest_chunks: Vec::new(),
            federations: Vec::new(),
        };

        let mut calendar = Calendar::default();
        let mut controlled = ControlledFactions::default();
        let mut wall = WallMap::default();
        let mut door = DoorMap::default();
        let mut bridge = BridgeMap::default();
        let mut dam = DamMap::default();
        let mut water = RuntimeWater::default();
        let mut edges = crate::simulation::construction::EdgeStructureMap::default();
        let mut chunk_map = crate::world::chunk::ChunkMap::default();
        let ids = NetIdMap::default();
        let mut fed_map = crate::simulation::federation::FederationMap::default();

        apply_bootstrap_snapshot(
            &snap,
            &mut calendar,
            &mut controlled,
            &mut wall,
            &mut door,
            &mut bridge,
            &mut dam,
            &mut water,
            &mut edges,
            &mut chunk_map,
            &ids,
            &mut fed_map,
        );

        assert!(matches!(calendar.season, Season::Autumn));
        assert_eq!(calendar.year, 5);
        assert_eq!(calendar.day, 17);
        assert_eq!(controlled.ids, vec![3, 4]);
    }
}
