//! Tile-overlay snapshot helpers (Phase 1e of `plans/multiplayer.md`).
//!
//! The server-auth pipeline needs to bootstrap a freshly-connecting client
//! with the current state of every tile-replacing structure — walls, doors,
//! bridges, dams, and the runtime-water cache. Entities are referenced by
//! `NetId` (stable across the wire), tiles by their `(i32, i32)` coordinate
//! that's already shape-serializable.
//!
//! These helpers are pure functions over the live resources; they don't
//! touch the World. Phase 2 will pack their outputs into the bootstrap
//! snapshot message; `apply_snapshot` rebuilds the maps on the client
//! after replication has materialised the entity stubs.
//!
//! Round-trip tests live alongside the helpers — they verify the
//! payload→map direction (the "rebuild" half of the contract). The
//! map→payload direction is iteration over `AHashMap` and is trivial.

use bevy::prelude::*;

use crate::net_id::{NetId, NetIdMap, Networked};
use crate::simulation::construction::{
    Bridge, BridgeMap, Dam, DamMap, DoorEntry, DoorMap, WallMap,
};
use crate::world::tile::TileKind;
use crate::world::water_runtime::{RuntimeWater, RuntimeWaterCell};

/// One wall entry in a tile-overlay snapshot. `tile` is the world tile
/// coord; `entity_net_id` is the wall entity's stable id.
#[derive(Debug, Clone, Copy)]
pub struct WallSnapshotEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
}

/// One door entry — carries the live `open` flag and faction id so the
/// client can drive LOS / passability without waiting for the door
/// component to replicate.
#[derive(Debug, Clone, Copy)]
pub struct DoorSnapshotEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
    pub open: bool,
    pub faction_id: u32,
}

/// One bridge entry — carries `restore_tile` so future deconstruct on
/// the client (under server auth) restores the right kind.
#[derive(Debug, Clone, Copy)]
pub struct BridgeSnapshotEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
}

/// One dam entry. Same shape as bridge; `crest_z` lives on the entity
/// component which arrives via per-entity replication.
#[derive(Debug, Clone, Copy)]
pub struct DamSnapshotEntry {
    pub tile: (i32, i32),
    pub entity_net_id: NetId,
}

/// One runtime-water cell. Tile-keyed; no entity ref.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeWaterSnapshotEntry {
    pub tile: (i32, i32),
    pub cell: RuntimeWaterCell,
}

/// A `Networked` component lookup that owns the entity→NetId mapping.
type NetworkedQuery<'w, 's> = Query<'w, 's, &'static Networked>;

/// Pure map → snapshot. Drops entries whose entity is missing a
/// `Networked` component — those aren't network-visible yet so they
/// shouldn't appear in a bootstrap message either.
pub fn snapshot_wall_map(map: &WallMap, q: &NetworkedQuery) -> Vec<WallSnapshotEntry> {
    let mut out = Vec::with_capacity(map.0.len());
    for (tile, &entity) in map.0.iter() {
        if let Ok(net) = q.get(entity) {
            out.push(WallSnapshotEntry {
                tile: *tile,
                entity_net_id: net.0,
            });
        }
    }
    out
}

pub fn snapshot_door_map(map: &DoorMap, q: &NetworkedQuery) -> Vec<DoorSnapshotEntry> {
    let mut out = Vec::with_capacity(map.0.len());
    for (tile, entry) in map.0.iter() {
        if let Ok(net) = q.get(entry.entity) {
            out.push(DoorSnapshotEntry {
                tile: *tile,
                entity_net_id: net.0,
                open: entry.open,
                faction_id: entry.faction_id,
            });
        }
    }
    out
}

pub fn snapshot_bridge_map(map: &BridgeMap, q: &NetworkedQuery) -> Vec<BridgeSnapshotEntry> {
    let mut out = Vec::with_capacity(map.0.len());
    for (tile, &entity) in map.0.iter() {
        if let Ok(net) = q.get(entity) {
            out.push(BridgeSnapshotEntry {
                tile: *tile,
                entity_net_id: net.0,
            });
        }
    }
    out
}

pub fn snapshot_dam_map(map: &DamMap, q: &NetworkedQuery) -> Vec<DamSnapshotEntry> {
    let mut out = Vec::with_capacity(map.0.len());
    for (tile, &entity) in map.0.iter() {
        if let Ok(net) = q.get(entity) {
            out.push(DamSnapshotEntry {
                tile: *tile,
                entity_net_id: net.0,
            });
        }
    }
    out
}

pub fn snapshot_runtime_water(water: &RuntimeWater) -> Vec<RuntimeWaterSnapshotEntry> {
    water
        .cells
        .iter()
        .map(|(tile, cell)| RuntimeWaterSnapshotEntry {
            tile: *tile,
            cell: *cell,
        })
        .collect()
}

/// Reverse direction: rebuild the tile→entity index on the client side
/// after the server has replicated the entity stubs. Skips entries whose
/// NetId hasn't been mapped to a live `Entity` yet (the client should
/// retry on the next replication tick).
pub fn apply_wall_snapshot(map: &mut WallMap, snap: &[WallSnapshotEntry], ids: &NetIdMap) {
    map.0.clear();
    for entry in snap {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            map.0.insert(entry.tile, entity);
        }
    }
}

pub fn apply_door_snapshot(
    map: &mut DoorMap,
    snap: &[DoorSnapshotEntry],
    ids: &NetIdMap,
) {
    map.0.clear();
    for entry in snap {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            map.0.insert(
                entry.tile,
                DoorEntry {
                    entity,
                    open: entry.open,
                    faction_id: entry.faction_id,
                },
            );
        }
    }
}

pub fn apply_bridge_snapshot(
    map: &mut BridgeMap,
    snap: &[BridgeSnapshotEntry],
    ids: &NetIdMap,
) {
    map.0.clear();
    for entry in snap {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            map.0.insert(entry.tile, entity);
        }
    }
}

pub fn apply_dam_snapshot(map: &mut DamMap, snap: &[DamSnapshotEntry], ids: &NetIdMap) {
    map.0.clear();
    for entry in snap {
        if let Some(entity) = ids.entity_of(entry.entity_net_id) {
            map.0.insert(entry.tile, entity);
        }
    }
}

pub fn apply_runtime_water_snapshot(water: &mut RuntimeWater, snap: &[RuntimeWaterSnapshotEntry]) {
    water.cells.clear();
    for entry in snap {
        water.cells.insert(entry.tile, entry.cell);
    }
}

// Silence "unused import" diagnostic when the file is read by `rustdoc`
// alone; `Bridge`/`Dam` show up in the doc comments above.
#[allow(dead_code)]
fn _doc_link_anchor() -> (Option<Bridge>, Option<Dam>, Option<TileKind>) {
    (None, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_world() -> World {
        let mut world = World::new();
        world.init_resource::<NetIdMap>();
        world
    }

    #[test]
    fn wall_snapshot_round_trips_through_netidmap() {
        let mut world = make_world();
        let mut wall_map = WallMap::default();
        // Spawn a wall entity and allocate a NetId for it.
        let entity = world.spawn(()).id();
        let net_id = world.resource_mut::<NetIdMap>().alloc(entity);
        world.entity_mut(entity).insert(Networked(net_id));
        wall_map.0.insert((3, 5), entity);

        // Take a snapshot using a temporary system that drives the query.
        let snap: Vec<WallSnapshotEntry> = {
            let mut q = world.query::<&Networked>();
            wall_map
                .0
                .iter()
                .filter_map(|(t, &e)| q.get(&world, e).ok().map(|n| WallSnapshotEntry {
                    tile: *t,
                    entity_net_id: n.0,
                }))
                .collect()
        };
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tile, (3, 5));
        assert_eq!(snap[0].entity_net_id, net_id);

        // Reverse direction: clear and rebuild.
        let mut rebuilt = WallMap::default();
        apply_wall_snapshot(&mut rebuilt, &snap, world.resource::<NetIdMap>());
        assert_eq!(rebuilt.0.len(), 1);
        assert_eq!(rebuilt.0.get(&(3, 5)).copied(), Some(entity));
    }

    #[test]
    fn door_snapshot_preserves_open_and_faction() {
        let mut world = make_world();
        let mut door_map = DoorMap::default();
        let entity = world.spawn(()).id();
        let net_id = world.resource_mut::<NetIdMap>().alloc(entity);
        world.entity_mut(entity).insert(Networked(net_id));
        door_map.0.insert(
            (7, 2),
            DoorEntry {
                entity,
                open: true,
                faction_id: 42,
            },
        );

        let snap: Vec<DoorSnapshotEntry> = {
            let mut q = world.query::<&Networked>();
            door_map
                .0
                .iter()
                .filter_map(|(t, e)| q.get(&world, e.entity).ok().map(|n| DoorSnapshotEntry {
                    tile: *t,
                    entity_net_id: n.0,
                    open: e.open,
                    faction_id: e.faction_id,
                }))
                .collect()
        };
        assert_eq!(snap.len(), 1);
        let entry = snap[0];
        assert!(entry.open);
        assert_eq!(entry.faction_id, 42);

        let mut rebuilt = DoorMap::default();
        apply_door_snapshot(&mut rebuilt, &snap, world.resource::<NetIdMap>());
        let restored = rebuilt.0.get(&(7, 2)).copied().unwrap();
        assert_eq!(restored.entity, entity);
        assert!(restored.open);
        assert_eq!(restored.faction_id, 42);
    }

    #[test]
    fn runtime_water_snapshot_is_tile_keyed_and_lossless() {
        let mut water = RuntimeWater::default();
        let cell = RuntimeWaterCell {
            ground_z: -3,
            depth: 1.5,
            reservoir_id: u32::MAX,
            salinity: 0.0,
            source_rate: 0.01,
        };
        water.cells.insert((10, -2), cell);

        let snap = snapshot_runtime_water(&water);
        assert_eq!(snap.len(), 1);
        let entry = snap[0];
        assert_eq!(entry.tile, (10, -2));
        assert_eq!(entry.cell.ground_z, -3);
        assert!((entry.cell.depth - 1.5).abs() < f32::EPSILON);

        let mut rebuilt = RuntimeWater::default();
        apply_runtime_water_snapshot(&mut rebuilt, &snap);
        assert_eq!(rebuilt.cells.len(), 1);
        let restored = rebuilt.cells.get(&(10, -2)).copied().unwrap();
        assert_eq!(restored.ground_z, -3);
        assert!((restored.depth - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn apply_snapshot_skips_unmapped_net_ids() {
        // Client-side rebuild while the entity stub hasn't been
        // materialised yet — the entry should be dropped, not crash.
        let snap = vec![WallSnapshotEntry {
            tile: (1, 1),
            entity_net_id: NetId(999),
        }];
        let empty_ids = NetIdMap::default();
        let mut rebuilt = WallMap::default();
        apply_wall_snapshot(&mut rebuilt, &snap, &empty_ids);
        assert!(rebuilt.0.is_empty());
    }
}
