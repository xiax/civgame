use crate::pathfinding::pool::AStarScratch;
use crate::pathfinding::step::passable_diagonal_step;
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
) -> (AStarResult, u32) {
    scratch.reset();
    if start == goal {
        return (AStarResult::Found(Vec::new()), 0);
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
                    None => return (AStarResult::Unreachable, expansions as u32),
                };
            }
            path.reverse();
            return (AStarResult::Found(path), expansions as u32);
        }

        expansions += 1;
        if expansions > max_nodes {
            if best_node == start {
                return (AStarResult::Unreachable, expansions as u32);
            }
            return (
                AStarResult::BudgetExhausted {
                    best_so_far: best_node,
                },
                expansions as u32,
            );
        }

        let cur_g = *scratch.g_score.get(&cur).unwrap_or(&u32::MAX);
        for &(dx, dy, diag) in &NEIGHBORS {
            let nx = cur.0 + dx;
            let ny = cur.1 + dy;
            for &dz in &[0i32, 1, -1] {
                let nz = cur.2 as i32 + dz;
                let cur3 = (cur.0, cur.1, cur.2 as i32);
                let to3 = (nx, ny, nz);
                let ok = if diag {
                    // Diagonal corner-cut rejection: movement walks pixel-
                    // by-pixel and rounds through one of the two axis-
                    // aligned corner cells before reaching the diagonal
                    // target. Both corners must be routable at some Z
                    // within ±1 of cur.z that is also a legal step into
                    // the chosen target Z, otherwise the boundary check
                    // in movement_system rejects the cross at runtime.
                    passable_diagonal_step(chunk_map, cur3, to3)
                } else {
                    chunk_map.passable_step_3d(cur3, to3)
                };
                if !ok {
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
                // No `break` — every legal dz must be pushed independently.
                // Stopping after the first hit (e.g. dz=0) used to silently
                // suppress descent options when a flat step was also legal,
                // stranding agents on top of plateaus and built walls.
            }
        }
    }

    let result = if best_node == start {
        AStarResult::Unreachable
    } else {
        AStarResult::BudgetExhausted {
            best_so_far: best_node,
        }
    };
    (result, expansions as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

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
            (AStarResult::Found(path), _iters) => {
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
            (AStarResult::Found(_), _) => panic!("should not find"),
            (AStarResult::Unreachable, _) | (AStarResult::BudgetExhausted { .. }, _) => {}
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

    #[test]
    fn diagonal_does_not_corner_cut_two_tall_wall() {
        // Two 2-tall walls flank the diagonal step (5,5,0) → (6,6,0): the
        // corner cells (6,5) and (5,6) are non-standable at any z within
        // ±1 of cur.z=0. Without the corner-cut guard, A* returns the
        // single-step diagonal and movement_system snap-backs at runtime.
        // With the guard, A* must route physically around the walls.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        for &(x, y) in &[(6i32, 5i32), (5, 6)] {
            for z in 0..=1i32 {
                map.set_tile(
                    x,
                    y,
                    z,
                    TileData {
                        kind: TileKind::Wall,
                        ..Default::default()
                    },
                );
            }
        }
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (5, 5, 0), (6, 6, 0), 500) {
            (AStarResult::Found(path), _) => {
                assert!(
                    path.len() >= 3,
                    "A* corner-cut leaked through: returned 1-step diagonal {:?}",
                    path
                );
            }
            (AStarResult::Unreachable, _) | (AStarResult::BudgetExhausted { .. }, _) => {}
        }
    }

    #[test]
    fn diagonal_rejected_when_only_one_corner_blocked() {
        // Wall stack at (6,5) only; (5,6) is open grass. The old guard
        // accepted the diagonal because it required *both* corners to fail.
        // Real movement can round through the blocked corner depending on
        // sub-pixel timing, so the diagonal must be rejected.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        for z in 0..=1i32 {
            map.set_tile(
                6,
                5,
                z,
                TileData {
                    kind: TileKind::Wall,
                    ..Default::default()
                },
            );
        }
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (5, 5, 0), (6, 6, 0), 500) {
            (AStarResult::Found(path), _) => {
                assert_ne!(
                    path.first(),
                    Some(&(6, 6, 0)),
                    "single-sided corner-cut leaked through: {:?}",
                    path
                );
                assert!(path.len() >= 2);
            }
            other => panic!("expected Found, got {:?}", other.0.kind()),
        }
    }

    #[test]
    fn diagonal_rejected_when_corner_z_mismatches_target_z() {
        // Target (6,6,1) sits one step up. Corner (6,5) is standable only
        // at z=-1 (floor pushed down). cur→corner is fine (|Δz|=1), but
        // corner→target is |Δz|=2 — impossible. The old guard accepted
        // because it only checked the cur→corner leg.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        // Raise (6,6) to z=1.
        map.set_tile(
            6,
            6,
            1,
            TileData {
                kind: TileKind::Grass,
                ..Default::default()
            },
        );
        // Make (6,5) standable only at z=-1.
        map.set_tile(
            6,
            5,
            0,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        map.set_tile(
            6,
            5,
            -1,
            TileData {
                kind: TileKind::Grass,
                ..Default::default()
            },
        );
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (5, 5, 0), (6, 6, 1), 500) {
            (AStarResult::Found(path), _) => {
                assert!(
                    path.first() != Some(&(6, 6, 1)),
                    "A* took the diagonal despite corner→target |Δz|=2: {:?}",
                    path
                );
                assert!(path.len() >= 2);
            }
            other => panic!("expected Found, got {:?}", other.0.kind()),
        }
    }

    #[test]
    fn neighbor_emits_all_legal_z_candidates() {
        // Stacked ramps at (5,5) make z = -1, 0, 1 all standable on the same
        // tile. With the old early-break in the dz loop, A* from (4,5,0)
        // would push only (5,5,0) (first dz that succeeds) and never expand
        // (5,5,1) — so a goal at (5,5,1) would be reported Unreachable. The
        // fix is to push every legal dz; this test guards against regression.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        for z in -1..=2i32 {
            map.set_tile(
                5,
                5,
                z,
                TileData {
                    kind: TileKind::Ramp,
                    ..Default::default()
                },
            );
        }
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (4, 5, 0), (5, 5, 1), 500) {
            (AStarResult::Found(path), _) => {
                assert_eq!(
                    path.last(),
                    Some(&(5, 5, 1)),
                    "expected to reach (5,5,1) directly: {:?}",
                    path
                );
            }
            other => panic!(
                "expected Found, got {:?} (early-break regression?)",
                other.0.kind()
            ),
        }
    }

    #[test]
    fn diagonal_allowed_on_uneven_but_routable_terrain() {
        // Gentle ramp: target (6,6) at z=1, both corners (6,5) and (5,6)
        // remain at z=0 grass. Each corner has cz=0 satisfying both legs
        // (cur→corner |Δz|=0, corner→target |Δz|=1). Diagonal must still
        // be emitted — the stricter rule must not over-restrict.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        map.set_tile(
            6,
            6,
            1,
            TileData {
                kind: TileKind::Grass,
                ..Default::default()
            },
        );
        let mut s = AStarScratch::default();
        match find_path_in(&mut s, &map, (5, 5, 0), (6, 6, 1), 500) {
            (AStarResult::Found(path), _) => {
                assert_eq!(
                    path.first(),
                    Some(&(6, 6, 1)),
                    "expected single-step diagonal, got {:?}",
                    path
                );
                assert_eq!(path.len(), 1);
            }
            other => panic!("expected Found, got {:?}", other.0.kind()),
        }
    }

    impl AStarResult {
        fn kind(&self) -> &'static str {
            match self {
                AStarResult::Found(_) => "Found",
                AStarResult::BudgetExhausted { .. } => "BudgetExhausted",
                AStarResult::Unreachable => "Unreachable",
            }
        }
    }
}
