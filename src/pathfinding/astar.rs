use crate::pathfinding::pool::AStarScratch;
use crate::pathfinding::tile_cost::{tile_step_cost, IMPASSABLE};
use crate::world::chunk::ChunkMap;
use std::cmp::Reverse;

/// Result of one bounded A* search.
pub enum AStarResult {
    /// Full path from `start` (exclusive) to `goal` (inclusive).
    Found(Vec<(i32, i32, i8)>),
    /// Search exhausted `max_nodes` expansions without reaching `goal`.
    /// `best_so_far` is the open-set node closest to `goal` by heuristic —
    /// callers can walk toward it as a partial step.
    BudgetExhausted { best_so_far: (i32, i32, i8) },
    /// Search closed without expanding any reachable neighbours; truly no
    /// path from `start` exists in the explored region.
    Unreachable,
}

fn h(a: (i32, i32, i8), b: (i32, i32, i8)) -> u32 {
    let dx = (a.0 - b.0).abs();
    let dy = (a.1 - b.1).abs();
    let dz = (a.2 as i32 - b.2 as i32).abs();
    ((dx.max(dy) + dz) as u32) * 100
}

const NEIGHBORS: [(i32, i32, bool); 8] = [
    (0, 1, false),
    (1, 1, true),
    (1, 0, false),
    (1, -1, true),
    (0, -1, false),
    (-1, -1, true),
    (-1, 0, false),
    (-1, 1, true),
];

/// Bounded 3D A* over tiles, using `passable_step_3d` for transitions
/// (so it walks via ramps automatically — including ramps in neighbouring
/// chunks). Reuses scratch buffers from the caller-provided `AStarScratch`
/// so steady-state searches don't allocate.
pub fn find_path_in(
    scratch: &mut AStarScratch,
    chunk_map: &ChunkMap,
    start: (i32, i32, i8),
    goal: (i32, i32, i8),
    max_nodes: usize,
) -> AStarResult {
    scratch.reset();
    if start == goal {
        return AStarResult::Found(Vec::new());
    }

    scratch.g_score.insert(start, 0);
    scratch.open.push(Reverse((h(start, goal), start)));

    let mut best_h = h(start, goal);
    let mut best_node = start;

    let mut expansions: usize = 0;
    while let Some(Reverse((_f, cur))) = scratch.open.pop() {
        if cur == goal {
            let mut path = Vec::new();
            let mut tail = cur;
            while tail != start {
                path.push(tail);
                tail = match scratch.came_from.get(&tail) {
                    Some(&p) => p,
                    None => return AStarResult::Unreachable,
                };
            }
            path.reverse();
            return AStarResult::Found(path);
        }

        expansions += 1;
        if expansions > max_nodes {
            if best_node == start {
                return AStarResult::Unreachable;
            }
            return AStarResult::BudgetExhausted { best_so_far: best_node };
        }

        let cur_g = *scratch.g_score.get(&cur).unwrap_or(&u32::MAX);
        for &(dx, dy, diag) in &NEIGHBORS {
            let nx = cur.0 + dx;
            let ny = cur.1 + dy;
            for &dz in &[0i32, 1, -1] {
                let nz = cur.2 as i32 + dz;
                let cur3 = (cur.0, cur.1, cur.2 as i32);
                if !chunk_map.passable_step_3d(cur3, (nx, ny, nz)) {
                    continue;
                }
                let kind = chunk_map.tile_at(nx, ny, nz).kind;
                let base = tile_step_cost(kind);
                if base == IMPASSABLE {
                    continue;
                }
                let mut step_cost = base as u32;
                if diag {
                    step_cost = step_cost * 141 / 100;
                }
                if dz != 0 {
                    step_cost = step_cost.saturating_add(8);
                }
                let next = (nx, ny, nz as i8);
                let tentative_g = cur_g.saturating_add(step_cost);
                let prev_g = *scratch.g_score.get(&next).unwrap_or(&u32::MAX);
                if tentative_g < prev_g {
                    scratch.g_score.insert(next, tentative_g);
                    scratch.came_from.insert(next, cur);
                    let nh = h(next, goal);
                    if nh < best_h {
                        best_h = nh;
                        best_node = next;
                    }
                    let f = tentative_g.saturating_add(nh);
                    scratch.open.push(Reverse((f, next)));
                }
                break;
            }
        }
    }

    if best_node == start {
        AStarResult::Unreachable
    } else {
        AStarResult::BudgetExhausted { best_so_far: best_node }
    }
}

/// Backwards-compatible wrapper: returns `Some(path)` only on `Found`.
/// `BudgetExhausted` and `Unreachable` both map to `None`. To be removed in
/// step (g) once movement.rs migrates to `find_path_in` directly.
pub fn find_path_toward(
    chunk_map: &ChunkMap,
    start: (i32, i32, i8),
    goal: (i32, i32, i8),
    max_nodes: usize,
) -> Option<Vec<(i32, i32, i8)>> {
    let mut scratch = AStarScratch::default();
    match find_path_in(&mut scratch, chunk_map, start, goal, max_nodes) {
        AStarResult::Found(p) if !p.is_empty() => Some(p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::TileKind;

    fn flat_chunk(surf_z: i8) -> Chunk {
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    #[test]
    fn flat_path_steps_toward_goal() {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (5, 5, 0), (8, 5, 0), 500) {
            AStarResult::Found(path) => {
                assert!(!path.is_empty());
                assert_eq!(path.last(), Some(&(8, 5, 0)));
            }
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn unreachable_returns_unreachable_or_budget() {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (5, 5, 0), (8, 5, 10), 200) {
            AStarResult::Found(_) => panic!("should not find"),
            AStarResult::Unreachable | AStarResult::BudgetExhausted { .. } => {}
        }
    }

    #[test]
    fn scratch_reused_across_calls() {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        let mut s = AStarScratch::default();
        for _ in 0..5 {
            let _ = find_path_in(&mut s, &map, (0, 0, 0), (10, 10, 0), 500);
        }
        assert!(s.g_score.capacity() > 0); // reused, not freed
    }
}
