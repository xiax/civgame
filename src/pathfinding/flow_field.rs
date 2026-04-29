use crate::pathfinding::tile_cost::{tile_step_cost, IMPASSABLE};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
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

pub fn build_flow_field<F>(
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

/// Direction-byte → (dx, dy) of the BFS expansion that wrote that byte.
/// Mirrors the `NEIGHBORS` table above: `build_flow_field` stores
/// `dir[neighbor] = d` where (dx, dy) at index `d` is the offset *from the
/// parent cell to the neighbor*. To walk back toward the goal from the
/// neighbor, step in the *opposite* of this offset.
const DIR_OFFSET: [(i32, i32); 8] = [
    (0, -1),
    (-1, -1),
    (-1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
    (1, 0),
    (1, -1),
];

/// Walks the flow field from `start_local` to the field's goal, producing
/// the same shape of segment path A* would: a list of (global_x, global_y, z)
/// tiles in step order, goal inclusive. Returns `None` if the start is
/// unreachable, the walk leaves the chunk, or it exceeds the safety bound.
///
/// Z is held at `field.goal_z` for every step — the worker only invokes
/// this when start.z == goal.z, and the field itself was built for that Z.
pub fn walk_to_goal(field: &FlowField, start_local: (u8, u8)) -> Option<Vec<(i16, i16, i8)>> {
    let csz = CHUNK_SIZE;
    let origin_x = field.chunk.0 * csz as i32;
    let origin_y = field.chunk.1 * csz as i32;
    let z = field.goal_z;

    let mut path: Vec<(i16, i16, i8)> = Vec::new();
    let mut x = start_local.0 as i32;
    let mut y = start_local.1 as i32;
    let goal_x = field.goal_tile.0 as i32;
    let goal_y = field.goal_tile.1 as i32;

    if x == goal_x && y == goal_y {
        return Some(path);
    }

    let max_steps = csz * csz;
    for _ in 0..max_steps {
        let idx = y as usize * csz + x as usize;
        let dir = field.directions[idx];
        if dir == 0xFF || (dir as usize) >= DIR_OFFSET.len() {
            return None;
        }
        // Step against the recorded expansion offset to walk goalward.
        let (dx, dy) = DIR_OFFSET[dir as usize];
        x -= dx;
        y -= dy;
        if x < 0 || y < 0 || x >= csz as i32 || y >= csz as i32 {
            return None;
        }
        let gx = (origin_x + x) as i16;
        let gy = (origin_y + y) as i16;
        path.push((gx, gy, z));
        if x == goal_x && y == goal_y {
            return Some(path);
        }
    }
    None
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

    fn build(map: &ChunkMap, coord: ChunkCoord, goal: (u8, u8), goal_z: i8) -> FlowField {
        build_flow_field(map, coord, goal, goal_z, &no_extra)
    }

    #[test]
    fn flow_field_reaches_corner_at_surface_z() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let field = build(&map, coord, (0, 0), 0);
        let far = (CHUNK_SIZE - 1) * CHUNK_SIZE + (CHUNK_SIZE - 1);
        assert_ne!(field.directions[far], 0xFF);
    }

    #[test]
    fn flow_field_skips_non_floor_z_slice() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let field = build(&map, coord, (0, 0), -5);
        let far = (CHUNK_SIZE - 1) * CHUNK_SIZE + (CHUNK_SIZE - 1);
        assert_eq!(field.directions[far], 0xFF);
    }

    #[test]
    fn flow_field_carved_tunnel_routes_underground() {
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 5);
        for x in 0..10i32 {
            map.set_tile(x, 5, 1, TileData { kind: TileKind::Air, ..Default::default() });
            map.set_tile(x, 5, 0, TileData { kind: TileKind::Dirt, ..Default::default() });
        }
        let field = build(&map, coord, (0, 5), 0);
        let idx = 5 * CHUNK_SIZE + 5;
        assert_ne!(field.directions[idx], 0xFF);
        let idx_rock = 6 * CHUNK_SIZE + 5;
        assert_eq!(field.directions[idx_rock], 0xFF);
    }

    #[test]
    fn flow_field_routes_around_wall() {
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 0);
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
        let field = build(&map, coord, (0, 0), 0);
        let far = 5 * CHUNK_SIZE + 10;
        assert_ne!(field.directions[far], 0xFF);
        let wall_idx = 5 * CHUNK_SIZE + 5;
        assert_eq!(field.directions[wall_idx], 0xFF);
    }

    #[test]
    fn flow_field_routes_over_one_step_ramp() {
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

        let field = build(&map, coord, (10, 5), 0);
        let idx_far = 5 * CHUNK_SIZE + 0;
        assert_ne!(field.directions[idx_far], 0xFF);
    }

    #[test]
    fn walk_to_goal_reaches_goal_with_unit_steps() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let goal = (4u8, 7u8);
        let field = build(&map, coord, goal, 0);

        let start = (12u8, 1u8);
        // Sanity: the BFS should have reached the start cell on a flat map.
        let start_idx = start.1 as usize * CHUNK_SIZE + start.0 as usize;
        assert_ne!(field.directions[start_idx], 0xFF);

        let path = walk_to_goal(&field, start).expect("flat map must produce a path");
        assert!(!path.is_empty());

        let origin_x = coord.0 * CHUNK_SIZE as i32;
        let origin_y = coord.1 * CHUNK_SIZE as i32;
        let last = *path.last().unwrap();
        assert_eq!(
            last,
            (
                (origin_x + goal.0 as i32) as i16,
                (origin_y + goal.1 as i32) as i16,
                0,
            )
        );

        let mut prev = (
            (origin_x + start.0 as i32) as i16,
            (origin_y + start.1 as i32) as i16,
        );
        for &(x, y, _) in &path {
            let dx = (x as i32 - prev.0 as i32).abs();
            let dy = (y as i32 - prev.1 as i32).abs();
            assert!(dx <= 1 && dy <= 1, "non-unit step from {prev:?} to ({x},{y})");
            prev = (x, y);
        }
    }

    #[test]
    fn walk_to_goal_returns_empty_when_already_at_goal() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let goal = (3u8, 3u8);
        let field = build(&map, coord, goal, 0);
        let path = walk_to_goal(&field, goal).expect("at-goal walk should succeed");
        assert!(path.is_empty());
    }

    #[test]
    fn walk_to_goal_returns_none_when_unreachable() {
        let coord = ChunkCoord(0, 0);
        let map = flat_map_with_chunk(coord, 0);
        let field = build(&map, coord, (0, 0), -5);
        let path = walk_to_goal(&field, (CHUNK_SIZE as u8 - 1, CHUNK_SIZE as u8 - 1));
        assert!(path.is_none());
    }
}
