//! Multi-tile vehicle pathfinder — Phase 3 of `plans/vehicle-system.md`.
//!
//! A heading-aware A* over `(anchor, z, heading)` nodes. Unlike person
//! pathing — a point agent on `(x, y, z)` — a vehicle is a rigid multi-tile
//! footprint that must fit, clear overhead obstacles, and turn within its
//! `turn_radius`. Each successor rotates the footprint to the candidate
//! heading and tests every footprint cell.
//!
//! The pathfinder is generic over a `cell_ok` closure so it has no
//! dependency on `simulation::` — the live caller (Phase 4) wraps
//! `ChunkMap::passable_at` + `ChunkMap::vertical_clearance_at` + the vehicle
//! occupancy index; tests pass a synthetic closure.
//!
//! The live caller (vehicle movement) is wired in Phase 4 alongside the
//! cargo-haul task — until then this module is exercised only by its tests.
#![allow(dead_code)]

use crate::collections::{AHashMap, AHashSet};
use bevy::math::IVec2;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Forward `(dx, dy)` for each heading. 0 = +Y (north); each step is 90° CCW.
const FORWARD: [(i32, i32); 4] = [(0, 1), (-1, 0), (0, -1), (1, 0)];

/// A node in the vehicle search space: the footprint anchor tile, foot Z, and
/// cardinal heading (`0..4`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VehicleNode {
    pub x: i32,
    pub y: i32,
    pub z: i8,
    pub heading: u8,
}

impl VehicleNode {
    pub fn new(x: i32, y: i32, z: i8, heading: u8) -> Self {
        VehicleNode { x, y, z, heading }
    }
}

/// Result of [`footprint_astar`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VehiclePathResult {
    /// A node sequence from start to a node on the goal tile.
    Found(Vec<VehicleNode>),
    /// The node budget was exhausted before reaching the goal.
    BudgetExhausted,
    /// No route exists.
    Unreachable,
}

/// Reusable scratch buffers for [`footprint_astar`] — mirrors `AStarScratch`.
#[derive(Default)]
pub struct VehiclePathScratch {
    open: BinaryHeap<Reverse<(u32, VehicleNode)>>,
    g_score: AHashMap<VehicleNode, u32>,
    came_from: AHashMap<VehicleNode, VehicleNode>,
    closed: AHashSet<VehicleNode>,
}

impl VehiclePathScratch {
    fn reset(&mut self) {
        self.open.clear();
        self.g_score.clear();
        self.came_from.clear();
        self.closed.clear();
    }
}

const MOVE_COST: u32 = 100;
const DIAG_COST: u32 = 141;
const Z_STEP_PENALTY: u32 = 8;
/// World Z range — the search never proposes a node outside it.
const Z_FLOOR: i8 = -16;
const Z_CEIL: i8 = 15;

fn chebyshev(a: (i32, i32), b: (i32, i32)) -> u32 {
    ((a.0 - b.0).abs().max((a.1 - b.1).abs())) as u32
}

/// Heading-aware multi-tile A*.
///
/// * `offsets_by_heading` — the footprint's XY cell offsets pre-rotated for
///   each heading (`VehicleFootprint::offsets_by_heading`); each set is
///   anchored so its minimum corner is `(0, 0)`.
/// * `turn_cost` — cost of a 90° turn-in-place; derive it from the vehicle's
///   `turn_radius` (a wider radius → higher cost → the planner prefers arcs).
/// * `cell_ok(x, y, z)` — true iff one footprint cell at `(x, y, z)` is
///   standable, has the vehicle's vertical clearance, and is unoccupied.
/// * `max_nodes` — node-expansion budget.
///
/// The goal is heading-agnostic: any node whose anchor equals `goal` wins.
pub fn footprint_astar(
    scratch: &mut VehiclePathScratch,
    offsets_by_heading: &[Vec<IVec2>; 4],
    start: VehicleNode,
    goal: (i32, i32),
    turn_cost: u32,
    cell_ok: impl Fn(i32, i32, i32) -> bool,
    max_nodes: usize,
) -> VehiclePathResult {
    scratch.reset();

    // The footprint must be valid at the start, or there is no route.
    let footprint_ok = |anchor: (i32, i32), z: i8, heading: u8| -> bool {
        offsets_by_heading[heading as usize]
            .iter()
            .all(|o| cell_ok(anchor.0 + o.x, anchor.1 + o.y, z as i32))
    };
    if !footprint_ok((start.x, start.y), start.z, start.heading) {
        return VehiclePathResult::Unreachable;
    }
    if (start.x, start.y) == goal {
        return VehiclePathResult::Found(vec![start]);
    }

    scratch.g_score.insert(start, 0);
    scratch
        .open
        .push(Reverse((chebyshev((start.x, start.y), goal) * MOVE_COST, start)));

    let mut expansions = 0usize;

    while let Some(Reverse((_, cur))) = scratch.open.pop() {
        if (cur.x, cur.y) == goal {
            return VehiclePathResult::Found(reconstruct(&scratch.came_from, cur));
        }
        if scratch.closed.contains(&cur) {
            continue;
        }
        scratch.closed.insert(cur);

        expansions += 1;
        if expansions > max_nodes {
            return VehiclePathResult::BudgetExhausted;
        }

        let cur_g = *scratch.g_score.get(&cur).unwrap_or(&u32::MAX);

        // ── successors ────────────────────────────────────────────────
        let mut successors: Vec<(VehicleNode, u32)> = Vec::with_capacity(8);

        // Turn in place — heading ±1, anchor + z fixed.
        for &nh in &[(cur.heading + 1) % 4, (cur.heading + 3) % 4] {
            successors.push((VehicleNode::new(cur.x, cur.y, cur.z, nh), turn_cost));
        }

        // Translate — forward, and the two forward diagonals; heading fixed.
        let fwd = FORWARD[cur.heading as usize];
        let left = FORWARD[((cur.heading + 1) % 4) as usize];
        let right = FORWARD[((cur.heading + 3) % 4) as usize];
        let moves: [((i32, i32), u32); 3] = [
            (fwd, MOVE_COST),
            ((fwd.0 + left.0, fwd.1 + left.1), DIAG_COST),
            ((fwd.0 + right.0, fwd.1 + right.1), DIAG_COST),
        ];
        for &((dx, dy), base) in &moves {
            for dz in [0i8, 1, -1] {
                let nz = cur.z + dz;
                if nz < Z_FLOOR || nz > Z_CEIL {
                    continue;
                }
                let cost = base + if dz != 0 { Z_STEP_PENALTY } else { 0 };
                successors.push((
                    VehicleNode::new(cur.x + dx, cur.y + dy, nz, cur.heading),
                    cost,
                ));
            }
        }

        for (next, step_cost) in successors {
            if scratch.closed.contains(&next) {
                continue;
            }
            if !footprint_ok((next.x, next.y), next.z, next.heading) {
                continue;
            }
            let tentative = cur_g.saturating_add(step_cost);
            let prior = *scratch.g_score.get(&next).unwrap_or(&u32::MAX);
            if tentative < prior {
                scratch.came_from.insert(next, cur);
                scratch.g_score.insert(next, tentative);
                let h = chebyshev((next.x, next.y), goal);
                scratch
                    .open
                    .push(Reverse((tentative + h * MOVE_COST, next)));
            }
        }
    }

    // Open set drained without reaching the goal.
    VehiclePathResult::Unreachable
}

fn reconstruct(came_from: &AHashMap<VehicleNode, VehicleNode>, end: VehicleNode) -> Vec<VehicleNode> {
    let mut path = vec![end];
    let mut cur = end;
    while let Some(&prev) = came_from.get(&cur) {
        path.push(prev);
        cur = prev;
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 footprint (point vehicle) — identical for every heading.
    fn unit_footprint() -> [Vec<IVec2>; 4] {
        let one = vec![IVec2::new(0, 0)];
        [one.clone(), one.clone(), one.clone(), one]
    }

    /// A 2-wide × 1-deep footprint at heading 0; rotations swap the axes.
    fn cart_footprint() -> [Vec<IVec2>; 4] {
        let wide = vec![IVec2::new(0, 0), IVec2::new(1, 0)];
        let tall = vec![IVec2::new(0, 0), IVec2::new(0, 1)];
        [wide.clone(), tall.clone(), wide, tall]
    }

    #[test]
    fn straight_route_on_open_ground() {
        let mut scratch = VehiclePathScratch::default();
        let fp = unit_footprint();
        let r = footprint_astar(
            &mut scratch,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 6),
            50,
            |_, _, _| true,
            10_000,
        );
        match r {
            VehiclePathResult::Found(path) => {
                let end = path.last().unwrap();
                assert_eq!((end.x, end.y), (0, 6));
            }
            other => panic!("expected a route, got {:?}", other),
        }
    }

    #[test]
    fn routes_around_a_wall() {
        let mut scratch = VehiclePathScratch::default();
        let fp = unit_footprint();
        // A wall blocks the column x=0 for y in 1..=4, leaving a gap at y=5.
        let r = footprint_astar(
            &mut scratch,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 6),
            50,
            |x, y, _| !(x == 0 && (1..=4).contains(&y)),
            50_000,
        );
        assert!(matches!(r, VehiclePathResult::Found(_)));
    }

    #[test]
    fn unreachable_when_fully_walled() {
        let mut scratch = VehiclePathScratch::default();
        let fp = unit_footprint();
        // The start sits in a finite open box (x 0..4, y 0..1); a solid wall
        // at y >= 2 seals it off from the goal at (0, 3).
        let r = footprint_astar(
            &mut scratch,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 3),
            50,
            |x, y, _| (0..=1).contains(&y) && (0..=4).contains(&x),
            50_000,
        );
        assert_eq!(r, VehiclePathResult::Unreachable);
    }

    #[test]
    fn wide_vehicle_rejected_in_narrow_gap() {
        let mut scratch = VehiclePathScratch::default();
        let fp = cart_footprint();
        // A 1-wide corridor at x=0 (x>=1 blocked) — a 2-wide cart can't fit
        // while pointing along it at heading 0 (footprint spans x 0..1).
        let r = footprint_astar(
            &mut scratch,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 5),
            50,
            |x, _, _| x == 0,
            50_000,
        );
        assert_eq!(r, VehiclePathResult::Unreachable);
    }

    #[test]
    fn low_clearance_blocks_tall_vehicle() {
        let mut scratch = VehiclePathScratch::default();
        let fp = unit_footprint();
        // `cell_ok` folds in clearance: tiles at y in 2..=3 have a low
        // overhang the (tall) vehicle can't pass. The reachable region is a
        // finite box so the search drains rather than wandering off.
        let r = footprint_astar(
            &mut scratch,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 5),
            50,
            |x, y, _| !(2..=3).contains(&y) && (-2..=2).contains(&x) && (0..=8).contains(&y),
            50_000,
        );
        assert_eq!(r, VehiclePathResult::Unreachable);
        // The same route succeeds once the overhang is cleared.
        let mut s2 = VehiclePathScratch::default();
        let r2 = footprint_astar(
            &mut s2,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 5),
            50,
            |_, _, _| true,
            50_000,
        );
        assert!(matches!(r2, VehiclePathResult::Found(_)));
    }

    #[test]
    fn budget_exhaustion_is_reported() {
        let mut scratch = VehiclePathScratch::default();
        let fp = unit_footprint();
        let r = footprint_astar(
            &mut scratch,
            &fp,
            VehicleNode::new(0, 0, 0, 0),
            (0, 500),
            50,
            |_, _, _| true,
            5, // tiny budget
        );
        assert_eq!(r, VehiclePathResult::BudgetExhausted);
    }
}
