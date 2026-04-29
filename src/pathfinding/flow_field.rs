use crate::pathfinding::tile_cost::{tile_step_cost, IMPASSABLE};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use ahash::AHashMap;
use bevy::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

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
    /// `extra_cost` lets callers add per-tile penalties (e.g. furniture)
    /// on top of the base `tile_step_cost`. It receives global tile coords.
    pub fn get_or_build<F>(
        &mut self,
        chunk_map: &ChunkMap,
        coord: ChunkCoord,
        goal: (u8, u8),
        goal_z: i8,
        extra_cost: F,
    ) -> &FlowField
    where
        F: Fn((i32, i32)) -> u16,
    {
        let key = (coord, goal.0, goal.1, goal_z);
        if !self.fields.contains_key(&key) {
            if self.max_cached > 0 && self.fields.len() >= self.max_cached {
                if let Some(victim) = self.fields.keys().next().copied() {
                    self.fields.remove(&victim);
                }
            }
            let field = build_flow_field(chunk_map, coord, goal, goal_z, &extra_cost);
            self.fields.insert(key, field);
        }
        &self.fields[&key]
    }

    /// Drop every cached field whose chunk matches `coord`. Call this when a
    /// tile in `coord` (or one near it) changes so future lookups rebuild.
    pub fn invalidate_chunk(&mut self, coord: ChunkCoord) {
        self.fields.retain(|(c, _, _, _), _| *c != coord);
    }
}

fn build_flow_field<F>(
    chunk_map: &ChunkMap,
    coord: ChunkCoord,
    goal: (u8, u8),
    goal_z: i8,
    extra_cost: &F,
) -> FlowField
where
    F: Fn((i32, i32)) -> u16,
{
    let mut dist = [u32::MAX; CHUNK_SIZE * CHUNK_SIZE];
    let mut dir = [0xFFu8; CHUNK_SIZE * CHUNK_SIZE];
    // Per-cell standable Z reached by the BFS so neighbour expansion uses
    // the right foot-Z (so ramps within a chunk stay routable). Sentinel
    // i8::MIN means "not yet reached".
    let mut best_z = [i8::MIN; CHUNK_SIZE * CHUNK_SIZE];

    let goal_idx = goal.1 as usize * CHUNK_SIZE + goal.0 as usize;
    dist[goal_idx] = 0;
    dir[goal_idx] = 0;
    best_z[goal_idx] = goal_z;

    // (dx, dy, direction_bit_pointing_back_toward_goal, is_diagonal)
    const NEIGHBORS: [(i32, i32, u8, bool); 8] = [
        (0, 1, 4, false),
        (-1, 1, 3, true),
        (-1, 0, 2, false),
        (-1, -1, 1, true),
        (0, -1, 0, false),
        (1, -1, 7, true),
        (1, 0, 6, false),
        (1, 1, 5, true),
    ];

    let chunk_origin_x = coord.0 * CHUNK_SIZE as i32;
    let chunk_origin_y = coord.1 * CHUNK_SIZE as i32;

    // Min-heap on (cost, x, y).
    let mut heap: BinaryHeap<Reverse<(u32, u8, u8)>> = BinaryHeap::new();
    heap.push(Reverse((0, goal.0, goal.1)));

    while let Some(Reverse((cur_dist, x, y))) = heap.pop() {
        let idx = y as usize * CHUNK_SIZE + x as usize;
        if cur_dist > dist[idx] {
            continue;
        }
        let cur_z = best_z[idx];
        let cur_gx = chunk_origin_x + x as i32;
        let cur_gy = chunk_origin_y + y as i32;

        for &(dx, dy, d, diag) in &NEIGHBORS {
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx < 0 || ny < 0 || nx >= CHUNK_SIZE as i32 || ny >= CHUNK_SIZE as i32 {
                continue;
            }
            let nidx = ny as usize * CHUNK_SIZE + nx as usize;
            let global_tx = chunk_origin_x + nx;
            let global_ty = chunk_origin_y + ny;

            // Find a standable Z at (nx, ny) reachable from (x, y, cur_z)
            // via a single passable_step_3d (|Δz| ≤ 1). Prefer same Z, then
            // step up, then step down.
            let mut chosen_nz: Option<i8> = None;
            for &dz in &[0i32, 1, -1] {
                let nz = cur_z as i32 + dz;
                if chunk_map.passable_step_3d(
                    (cur_gx, cur_gy, cur_z as i32),
                    (global_tx, global_ty, nz),
                ) {
                    chosen_nz = Some(nz as i8);
                    break;
                }
            }
            let nz = match chosen_nz {
                Some(z) => z,
                None => continue,
            };

            // Cost of the chosen Z slice — passable_step_3d guarantees
            // standability, but we still want speed weighting (Road faster,
            // Forest slower) and IMPASSABLE rejection for kinds that
            // shouldn't ever be routed through (Water, Wall, Air).
            let kind = chunk_map.tile_at(global_tx, global_ty, nz as i32).kind;
            let base = tile_step_cost(kind);
            if base == IMPASSABLE {
                continue;
            }
            let extra = extra_cost((global_tx, global_ty));
            let mut step_cost: u32 = (base as u32).saturating_add(extra as u32);
            if diag {
                // 1.41 ≈ sqrt(2); use 141/100 to keep things integer.
                step_cost = step_cost * 141 / 100;
            }
            // Mild penalty for changing Z so the field prefers level paths
            // when both are available.
            if nz != cur_z {
                step_cost = step_cost.saturating_add(8);
            }
            let new_dist = cur_dist.saturating_add(step_cost);
            if new_dist < dist[nidx] {
                dist[nidx] = new_dist;
                dir[nidx] = d;
                best_z[nidx] = nz;
                heap.push(Reverse((new_dist, nx as u8, ny as u8)));
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

    fn no_extra(_pos: (i32, i32)) -> u16 {
        0
    }

    #[test]
    fn flow_field_reaches_corner_at_surface_z() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 0), 0, no_extra);
        let far = (CHUNK_SIZE - 1) * CHUNK_SIZE + (CHUNK_SIZE - 1);
        assert_ne!(field.directions[far], 0xFF);
    }

    #[test]
    fn flow_field_skips_non_floor_z_slice() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 0), -5, no_extra);
        let far = (CHUNK_SIZE - 1) * CHUNK_SIZE + (CHUNK_SIZE - 1);
        assert_eq!(field.directions[far], 0xFF);
    }

    #[test]
    fn flow_field_separate_cache_per_z() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        cache.get_or_build(&map, coord, (0, 0), 0, no_extra);
        cache.get_or_build(&map, coord, (0, 0), 1, no_extra);
        assert_eq!(cache.fields.len(), 2);
    }

    #[test]
    fn flow_field_carved_tunnel_routes_underground() {
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 5);
        for x in 0..10i32 {
            map.set_tile(x, 5, 1, TileData { kind: TileKind::Air, ..Default::default() });
            map.set_tile(x, 5, 0, TileData { kind: TileKind::Dirt, ..Default::default() });
        }
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 5), 0, no_extra);
        let idx = 5 * CHUNK_SIZE + 5;
        assert_ne!(field.directions[idx], 0xFF);
        let idx_rock = 6 * CHUNK_SIZE + 5;
        assert_eq!(field.directions[idx_rock], 0xFF);
    }

    #[test]
    fn flow_field_routes_around_wall() {
        // Carve a vertical wall column blocking the direct path from corner to corner.
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 0);
        // Block column x=5 from y=0..CHUNK_SIZE-1 (leave the bottom row open).
        for y in 0..(CHUNK_SIZE as i32 - 1) {
            map.set_tile(
                5,
                y,
                1,
                TileData {
                    kind: TileKind::Wall,
                    ..Default::default()
                },
            );
        }
        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (0, 0), 0, no_extra);
        // A tile on the far side of the wall should still have a path (around it).
        let far = 5 * CHUNK_SIZE + 10;
        assert_ne!(field.directions[far], 0xFF);
        // But the wall tile itself has no path.
        let wall_idx = 5 * CHUNK_SIZE + 5;
        assert_eq!(field.directions[wall_idx], 0xFF);
    }

    #[test]
    fn flow_field_routes_over_one_step_ramp() {
        // Flat z=0 chunk. Block (5, 5) at z=0 with a Wall and place a Ramp
        // at (5, 5, 1) so the only east-west path through column 5 is up
        // and over the ramp at z=1.
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 0);
        map.set_tile(
            5,
            5,
            0,
            TileData {
                kind: TileKind::Wall,
                ..Default::default()
            },
        );
        map.set_tile(
            5,
            5,
            1,
            TileData {
                kind: TileKind::Ramp,
                ..Default::default()
            },
        );

        let mut cache = FlowFieldCache::default();
        let field = cache.get_or_build(&map, coord, (10, 5), 0, no_extra);
        // (0, 5) at z=0 must reach the goal — the BFS must climb the ramp.
        let idx_far = 5 * CHUNK_SIZE + 0;
        assert_ne!(field.directions[idx_far], 0xFF);
    }

    #[test]
    fn invalidate_chunk_drops_cached_fields() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let mut cache = FlowFieldCache::default();
        cache.get_or_build(&map, coord, (0, 0), 0, no_extra);
        assert_eq!(cache.fields.len(), 1);
        cache.invalidate_chunk(coord);
        assert!(cache.fields.is_empty());
    }
}
