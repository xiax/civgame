use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use ahash::AHashMap;
use bevy::prelude::*;

/// Direction bits: 0=N, 1=NE, 2=E, 3=SE, 4=S, 5=SW, 6=W, 7=NW, 0xFF=no path
pub struct FlowField {
    pub chunk: ChunkCoord,
    pub directions: Box<[u8; CHUNK_SIZE * CHUNK_SIZE]>,
    pub goal_tile: (u8, u8),
    pub goal_z: i8,
    pub generation: u32,
}

#[derive(Resource, Default)]
pub struct FlowFieldCache {
    pub fields: AHashMap<(ChunkCoord, u8, u8, i8), FlowField>,
    pub max_cached: usize,
}

impl FlowFieldCache {
    /// Build (or fetch) a flow field for `coord`/`goal` at Z slice `goal_z`.
    /// One field per Z slice — agents handle Z transitions step-by-step via
    /// `passable_step_3d` during movement, not via the flow field.
    pub fn get_or_build(
        &mut self,
        chunk_map: &ChunkMap,
        coord: ChunkCoord,
        goal: (u8, u8),
        goal_z: i8,
    ) -> &FlowField {
        let key = (coord, goal.0, goal.1, goal_z);
        if !self.fields.contains_key(&key) {
            // Simple over-cap eviction: drop one arbitrary entry. Real LRU
            // bookkeeping is overkill until profiling shows churn.
            if self.max_cached > 0 && self.fields.len() >= self.max_cached {
                if let Some(victim) = self.fields.keys().next().copied() {
                    self.fields.remove(&victim);
                }
            }
            let field = build_flow_field(chunk_map, coord, goal, goal_z);
            self.fields.insert(key, field);
        }
        &self.fields[&key]
    }
}

fn build_flow_field(
    chunk_map: &ChunkMap,
    coord: ChunkCoord,
    goal: (u8, u8),
    goal_z: i8,
) -> FlowField {
    use std::collections::VecDeque;

    let mut dist = [u16::MAX; CHUNK_SIZE * CHUNK_SIZE];
    let mut dir = [0xFFu8; CHUNK_SIZE * CHUNK_SIZE];

    let goal_idx = goal.1 as usize * CHUNK_SIZE + goal.0 as usize;
    dist[goal_idx] = 0;
    dir[goal_idx] = 0;

    let mut queue = VecDeque::new();
    queue.push_back(goal);

    // (dx, dy, direction_bit_pointing_back_toward_goal)
    const NEIGHBORS: [(i32, i32, u8); 8] = [
        (0, 1, 4),
        (-1, 1, 3),
        (-1, 0, 2),
        (-1, -1, 1),
        (0, -1, 0),
        (1, -1, 7),
        (1, 0, 6),
        (1, 1, 5),
    ];

    let chunk_origin_x = coord.0 * CHUNK_SIZE as i32;
    let chunk_origin_y = coord.1 * CHUNK_SIZE as i32;
    let goal_z_i32 = goal_z as i32;

    while let Some((x, y)) = queue.pop_front() {
        let cur_dist = dist[y as usize * CHUNK_SIZE + x as usize];

        for &(dx, dy, d) in &NEIGHBORS {
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx < 0 || ny < 0 || nx >= CHUNK_SIZE as i32 || ny >= CHUNK_SIZE as i32 {
                continue;
            }
            let nidx = ny as usize * CHUNK_SIZE + nx as usize;
            if dist[nidx] != u16::MAX {
                continue;
            }
            let global_tx = chunk_origin_x + nx;
            let global_ty = chunk_origin_y + ny;
            if chunk_map.passable_at(global_tx, global_ty, goal_z_i32) {
                dist[nidx] = cur_dist + 1;
                dir[nidx] = d;
                queue.push_back((nx as u8, ny as u8));
            }
        }
    }

    FlowField {
        chunk: coord,
        directions: Box::new(dir),
        goal_tile: goal,
        goal_z,
        generation: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_map_with_chunk(coord: ChunkCoord, surf_z: i8) -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(
            coord,
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    #[test]
    fn flow_field_reaches_corner_at_surface_z() {
        // Flat grass chunk at z=0 — every tile is passable at z=0, none at z=-1.
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 0), 0);
        // Tile diagonally opposite the goal must have a non-0xFF direction.
        let far = (CHUNK_SIZE - 1) * CHUNK_SIZE + (CHUNK_SIZE - 1);
        assert_ne!(field.directions[far], 0xFF);
    }

    #[test]
    fn flow_field_skips_non_floor_z_slice() {
        // At z=-5 (inside solid rock, no carved deltas) every tile is impassable.
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 0), -5);
        let far = (CHUNK_SIZE - 1) * CHUNK_SIZE + (CHUNK_SIZE - 1);
        assert_eq!(field.directions[far], 0xFF);
    }

    #[test]
    fn flow_field_separate_cache_per_z() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        cache.get_or_build(&map, coord, (0, 0), 0);
        cache.get_or_build(&map, coord, (0, 0), 1);
        assert_eq!(cache.fields.len(), 2);
    }

    #[test]
    fn flow_field_carved_tunnel_routes_underground() {
        // Hill column surface=5; carve a horizontal tunnel at z=0 along y=5.
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 5);
        for x in 0..10i32 {
            map.set_tile(x, 5, 1, TileData { kind: TileKind::Air, ..Default::default() });
            map.set_tile(x, 5, 0, TileData { kind: TileKind::Dirt, ..Default::default() });
        }
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 5), 0);
        // Tile (5, 5) at z=0 is in the carved tunnel — should have a path back to goal.
        let idx = 5 * CHUNK_SIZE + 5;
        assert_ne!(field.directions[idx], 0xFF);
        // Tile (5, 6) at z=0 is solid rock (no carve) — no path.
        let idx_rock = 6 * CHUNK_SIZE + 5;
        assert_eq!(field.directions[idx_rock], 0xFF);
    }
}
