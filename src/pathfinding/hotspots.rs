use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::pathfinding::flow_field::{build_flow_field, FlowField};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};

/// What the hotspot is for. Used by producers to tag their registrations
/// and by `unregister` to disambiguate when multiple kinds of hotspot share
/// a tile (e.g. a faction center that is also a storage tile).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum HotspotKind {
    FactionCenter,
    Storage,
    /// Player-issued military rally point — registered when the player
    /// right-clicks a tile with drafted units selected. Same flow-field
    /// machinery; lifecycle managed by `expire_rally_points_system`.
    RallyPoint,
}

const ALL_KINDS: [HotspotKind; 3] = [
    HotspotKind::FactionCenter,
    HotspotKind::Storage,
    HotspotKind::RallyPoint,
];

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct HotspotKey {
    pub tile: (i16, i16, i8),
    pub kind: HotspotKind,
}

pub struct HotspotEntry {
    pub field: FlowField,
}

/// Registry of "many agents converge here" tiles plus their precomputed
/// flow fields. Producers (faction.rs, construction.rs, terraform.rs,
/// settlement) call `register` on spawn and `unregister` on destruction.
/// The path worker calls `lookup_field` on a request whose goal is in the
/// start chunk to skip A* when the goal happens to be a registered hotspot.
#[derive(Resource, Default)]
pub struct HotspotFlowFields {
    pub entries: AHashMap<HotspotKey, HotspotEntry>,
    /// Keys whose flow field has not yet been built (new registration) or
    /// has been invalidated by a tile change. The build system drains this
    /// each PostUpdate.
    pub dirty: AHashSet<HotspotKey>,
    pub field_count: u32,
    pub lookup_count: u64,
    pub lookup_hits: u64,
}

impl HotspotFlowFields {
    pub fn register(&mut self, tile: (i16, i16, i8), kind: HotspotKind) {
        let key = HotspotKey { tile, kind };
        if !self.entries.contains_key(&key) {
            self.dirty.insert(key);
        }
    }

    pub fn unregister(&mut self, tile: (i16, i16, i8), kind: HotspotKind) {
        let key = HotspotKey { tile, kind };
        self.entries.remove(&key);
        self.dirty.remove(&key);
        self.field_count = self.entries.len() as u32;
    }

    pub fn is_registered(&self, tile: (i16, i16, i8), kind: HotspotKind) -> bool {
        let key = HotspotKey { tile, kind };
        self.entries.contains_key(&key) || self.dirty.contains(&key)
    }

    /// Drop every entry whose flow field lives in `coord`. They get pushed
    /// back onto `dirty` so the next build pass rebuilds them.
    pub fn invalidate_chunk(&mut self, coord: ChunkCoord) {
        let to_dirty: Vec<HotspotKey> = self
            .entries
            .iter()
            .filter(|(_, e)| e.field.chunk == coord)
            .map(|(k, _)| *k)
            .collect();
        for k in to_dirty {
            self.entries.remove(&k);
            self.dirty.insert(k);
        }
        self.field_count = self.entries.len() as u32;
    }

    /// Returns the encoded direction (0..=7) that an agent at `agent_pos`
    /// should step to walk toward the hotspot tile, or `None` when:
    /// - no hotspot of any kind is registered at `tile`,
    /// - the agent is in a different chunk than the hotspot (cross-chunk
    ///   routing is the router's job, not the flow field's),
    /// - the agent's tile has no path to the goal within the chunk.
    pub fn lookup_dir(
        &self,
        tile: (i16, i16, i8),
        agent_pos: (i32, i32, i8),
    ) -> Option<u8> {
        for k in ALL_KINDS {
            let key = HotspotKey { tile, kind: k };
            let Some(entry) = self.entries.get(&key) else { continue };
            let chunk = entry.field.chunk;
            let csz = CHUNK_SIZE as i32;
            let agent_chunk = ChunkCoord(
                agent_pos.0.div_euclid(csz),
                agent_pos.1.div_euclid(csz),
            );
            if agent_chunk != chunk {
                continue;
            }
            let lx = agent_pos.0.rem_euclid(csz) as usize;
            let ly = agent_pos.1.rem_euclid(csz) as usize;
            let cell_idx = ly * CHUNK_SIZE + lx;
            let dir = entry.field.directions[cell_idx];
            if dir == 0xFF {
                return None;
            }
            // Reject if the BFS reached this cell at a different Z than the
            // agent is currently on (e.g. the field rolled over a ramp at
            // Z+1 here, but the agent is on Z).
            if entry.field.cell_z[cell_idx] != agent_pos.2 {
                return None;
            }
            return Some(dir);
        }
        None
    }

    /// Returns the precomputed flow field whose goal is `tile`, regardless
    /// of which hotspot kind it was registered under. Used by the path
    /// worker to short-circuit single-chunk A* when the goal is a known
    /// hotspot.
    pub fn lookup_field(&self, tile: (i16, i16, i8)) -> Option<&FlowField> {
        for k in ALL_KINDS {
            if let Some(entry) = self.entries.get(&HotspotKey { tile, kind: k }) {
                return Some(&entry.field);
            }
        }
        None
    }
}

/// Builds flow fields for every dirty hotspot. Skips entries whose chunk
/// isn't loaded yet — they stay dirty and retry next tick.
pub fn rebuild_dirty_hotspots_system(
    chunk_map: Res<ChunkMap>,
    mut fields: ResMut<HotspotFlowFields>,
) {
    if fields.dirty.is_empty() {
        return;
    }
    let dirty: Vec<HotspotKey> = fields.dirty.drain().collect();
    let csz = CHUNK_SIZE as i32;
    let mut requeue: Vec<HotspotKey> = Vec::new();
    let mut built: Vec<(HotspotKey, FlowField)> = Vec::new();
    for key in dirty {
        let (gx, gy, gz) = key.tile;
        let chunk = ChunkCoord((gx as i32).div_euclid(csz), (gy as i32).div_euclid(csz));
        if !chunk_map.0.contains_key(&chunk) {
            requeue.push(key);
            continue;
        }
        let goal_local = (
            (gx as i32 - chunk.0 * csz) as u8,
            (gy as i32 - chunk.1 * csz) as u8,
        );
        let field = build_flow_field(&chunk_map, chunk, goal_local, gz, &|_| 0u16);
        built.push((key, field));
    }
    for (key, field) in built {
        fields.entries.insert(key, HotspotEntry { field });
    }
    for key in requeue {
        fields.dirty.insert(key);
    }
    fields.field_count = fields.entries.len() as u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkMap};
    use crate::world::tile::TileKind;

    fn flat_chunk_map(coord: ChunkCoord, surf_z: i8) -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0
            .insert(coord, Chunk::new(surface_z, surface_kind, surface_fertility));
        map
    }

    #[test]
    fn register_then_rebuild_populates_lookup_field() {
        let mut fields = HotspotFlowFields::default();
        let goal_tile = (5i16, 6i16, 0i8);
        fields.register(goal_tile, HotspotKind::FactionCenter);
        assert!(fields.is_registered(goal_tile, HotspotKind::FactionCenter));
        assert!(fields.lookup_field(goal_tile).is_none(), "not yet built");

        // Drive the rebuild logic the same way the system does, without
        // bringing up a full Bevy app.
        let coord = ChunkCoord(0, 0);
        let chunk_map = flat_chunk_map(coord, 0);
        let dirty: Vec<HotspotKey> = fields.dirty.drain().collect();
        for key in dirty {
            let (gx, gy, gz) = key.tile;
            let csz = CHUNK_SIZE as i32;
            let chunk = ChunkCoord((gx as i32).div_euclid(csz), (gy as i32).div_euclid(csz));
            let goal_local = (
                (gx as i32 - chunk.0 * csz) as u8,
                (gy as i32 - chunk.1 * csz) as u8,
            );
            let field = build_flow_field(&chunk_map, chunk, goal_local, gz, &|_| 0u16);
            fields.entries.insert(key, HotspotEntry { field });
        }

        let field = fields.lookup_field(goal_tile).expect("field built");
        assert_eq!(field.goal_tile, (5, 6));
        assert_eq!(field.goal_z, 0);
    }

    #[test]
    fn lookup_field_misses_when_unregistered() {
        let fields = HotspotFlowFields::default();
        assert!(fields.lookup_field((1, 2, 0)).is_none());
    }

    #[test]
    fn lookup_dir_rejects_z_mismatch() {
        let mut fields = HotspotFlowFields::default();
        let goal_tile = (5i16, 6i16, 0i8);
        fields.register(goal_tile, HotspotKind::FactionCenter);

        let coord = ChunkCoord(0, 0);
        let chunk_map = flat_chunk_map(coord, 0);
        let dirty: Vec<HotspotKey> = fields.dirty.drain().collect();
        for key in dirty {
            let (gx, gy, gz) = key.tile;
            let csz = CHUNK_SIZE as i32;
            let chunk = ChunkCoord((gx as i32).div_euclid(csz), (gy as i32).div_euclid(csz));
            let goal_local = (
                (gx as i32 - chunk.0 * csz) as u8,
                (gy as i32 - chunk.1 * csz) as u8,
            );
            let field = build_flow_field(&chunk_map, chunk, goal_local, gz, &|_| 0u16);
            fields.entries.insert(key, HotspotEntry { field });
        }

        // Same chunk, reachable cell at z=0 should hit.
        assert!(fields.lookup_dir(goal_tile, (10, 10, 0)).is_some());
        // Same cell but agent claims z=7 — BFS reached it at z=0, so reject.
        assert!(fields.lookup_dir(goal_tile, (10, 10, 7)).is_none());
    }
}
