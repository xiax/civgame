//! River-aware detour-cost distance estimate.
//!
//! Every target-selection site in the simulation used to rank candidates
//! by straight-line (chebyshev / euclidean / manhattan) distance from the
//! agent. That ignores that a river forces a long walk-around: a target on
//! the far bank can be straight-line "near" while the real walk cost is
//! enormous. The chunk router already computes a river-aware cost (River /
//! Water are `IMPASSABLE` in `tile_cost.rs`), so this module exposes that
//! cost as a chebyshev-equivalent tile count that is a drop-in replacement
//! for the old distance term.
//!
//! Key property exploited: the chunk graph is weight-symmetric, so a
//! Dijkstra tree rooted at the *agent's* node yields the optimal chunk-path
//! cost to *every* candidate in one cached build (see
//! `ChunkRouter::with_tree_from`). The per-candidate cost is then an O(1)
//! hashmap lookup.

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::{ChunkRouter, RouterNode};
use crate::pathfinding::tile_cost::BASE_STEP_COST;
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};

/// Router edge cost is ~`BASE_STEP_COST` per chunk-border crossing, which
/// spatially spans ~`CHUNK_SIZE` tiles. Converting router units to
/// chebyshev-equivalent tiles is therefore `CHUNK_SIZE / BASE_STEP_COST`.
/// Derived from both constants so a future step-cost retune stays coherent.
pub const ROUTER_UNITS_TO_TILES: f32 = CHUNK_SIZE as f32 / BASE_STEP_COST as f32;

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

#[inline]
fn node_of(graph: &ChunkGraph, x: i32, y: i32, z: i8) -> Option<RouterNode> {
    let comp = graph.component_for_tile(x, y, z)?;
    let csz = CHUNK_SIZE as i32;
    Some((ChunkCoord(x.div_euclid(csz), y.div_euclid(csz)), comp))
}

/// Borrows the router + graph for the duration of a selection pass and
/// answers detour-aware distances in chebyshev-equivalent tiles.
pub struct DetourEstimator<'a> {
    pub router: &'a ChunkRouter,
    pub graph: &'a ChunkGraph,
}

impl<'a> DetourEstimator<'a> {
    pub fn new(router: &'a ChunkRouter, graph: &'a ChunkGraph) -> Self {
        Self { router, graph }
    }

    /// Detour-aware distance from `(o_tile, o_z)` to `(c_tile, c_z)` in
    /// chebyshev-equivalent tiles.
    ///
    /// `max(chebyshev, rescaled chunk-hop cost)`:
    /// - chebyshev alone underestimates across a river (the bug);
    /// - the chunk-hop term is coarse *within* the endpoint chunks.
    ///
    /// `max` keeps the chunk-hop term dominant when a river forces a
    /// detour and chebyshev dominant for sub-chunk ordering. It is a true
    /// lower bound on the real walk distance, so it never over-penalises a
    /// genuinely near target. Same chunk-component ⇒ chebyshev exactly
    /// (no detour possible, tree never consulted). Any resolution failure
    /// (chunk unloaded / not standable / unreachable) ⇒ chebyshev
    /// fallback — never 0, never panic, degrades to the old behaviour.
    pub fn tiles(&self, o_tile: (i32, i32), o_z: i8, c_tile: (i32, i32), c_z: i8) -> i32 {
        let cheb = chebyshev(o_tile, c_tile);
        let (Some(o_node), Some(c_node)) = (
            node_of(self.graph, o_tile.0, o_tile.1, o_z),
            node_of(self.graph, c_tile.0, c_tile.1, c_z),
        ) else {
            return cheb;
        };
        if o_node == c_node {
            // Same chunk *and* component: no cross-chunk routing, the
            // chunk graph is too coarse to refine — chebyshev is the
            // ordering signal.
            return cheb;
        }
        match self
            .router
            .with_tree_from(self.graph, o_node, |tree| tree.dist.get(&c_node).copied())
            .flatten()
        {
            Some(units) => {
                let detour = (units as f32 * ROUTER_UNITS_TO_TILES).round() as i32;
                cheb.max(detour)
            }
            // Candidate unreachable from origin (different connected
            // component) or origin chunk unloaded. Reachability is filtered
            // upstream; if one slips through, chebyshev keeps it ranked
            // rather than spuriously free.
            None => cheb,
        }
    }

    /// Curried form for the closure-shaped call sites (vision pickers,
    /// `pick_least_pressured_rep`, `nearest_with_cluster_filter`). Captures
    /// the origin once; `z_of` resolves a candidate tile's standable z
    /// (call sites pass the same `nearest_standable_z`-based closure they
    /// already build for `reach_from_agent`).
    pub fn from<Z>(
        &'a self,
        o_tile: (i32, i32),
        o_z: i8,
        z_of: Z,
    ) -> impl Fn((i32, i32)) -> i32 + 'a
    where
        Z: Fn((i32, i32)) -> i8 + 'a,
    {
        move |c_tile| self.tiles(o_tile, o_z, c_tile, z_of(c_tile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::{ChunkComponents, ChunkEdge, ComponentId};
    use crate::collections::AHashMap;

    fn comp(coord_tiles: &[(u8, u8)]) -> ChunkComponents {
        let mut at = AHashMap::default();
        for &(lx, ly) in coord_tiles {
            at.insert((lx, ly, 0i8), ComponentId(0));
        }
        ChunkComponents { at, count: 1 }
    }

    fn edge(neighbor: ChunkCoord) -> ChunkEdge {
        ChunkEdge {
            neighbor,
            exit_local: (0, 0),
            exit_z: 0,
            entry_local: (0, 0),
            entry_z: 0,
            traverse_cost: 100,
            from_component: ComponentId(0),
            to_component: ComponentId(0),
        }
    }

    /// Graph: chunk(0,0) and chunk(0,1) are spatially adjacent (share the
    /// y=32 border) but have NO direct edge — a "river" runs along that
    /// border. They connect only via the detour chunk(0,0)→chunk(1,0)→
    /// chunk(1,1)→chunk(0,1) (3 hops).
    fn river_split_graph() -> ChunkGraph {
        let mut g = ChunkGraph::default();
        // chunk(0,0): origin tile (10,31) [lx10,ly31] + same-side tile (20,20).
        g.components
            .insert(ChunkCoord(0, 0), comp(&[(10, 31), (20, 20)]));
        // chunk(0,1): far-bank tile (10,33) → lx10, ly1.
        g.components.insert(ChunkCoord(0, 1), comp(&[(10, 1)]));
        g.components.insert(ChunkCoord(1, 0), comp(&[(0, 0)]));
        g.components.insert(ChunkCoord(1, 1), comp(&[(0, 0)]));
        // Detour edges only (no (0,0)<->(0,1) direct edge).
        g.edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0))]);
        g.edges.insert(
            ChunkCoord(1, 0),
            vec![edge(ChunkCoord(0, 0)), edge(ChunkCoord(1, 1))],
        );
        g.edges.insert(
            ChunkCoord(1, 1),
            vec![edge(ChunkCoord(1, 0)), edge(ChunkCoord(0, 1))],
        );
        g.edges
            .insert(ChunkCoord(0, 1), vec![edge(ChunkCoord(1, 1))]);
        g.generation = 1;
        g
    }

    #[test]
    fn same_component_returns_plain_chebyshev() {
        let g = river_split_graph();
        let r = ChunkRouter::default();
        let est = DetourEstimator::new(&r, &g);
        // (10,31) → (20,20), both chunk(0,0) component 0.
        assert_eq!(
            est.tiles((10, 31), 0, (20, 20), 0),
            chebyshev((10, 31), (20, 20))
        );
    }

    #[test]
    fn across_river_costs_the_detour_not_the_straight_line() {
        let g = river_split_graph();
        let r = ChunkRouter::default();
        let est = DetourEstimator::new(&r, &g);
        // Spatially only 2 tiles apart ((10,31)→(10,33)) but 3 chunk hops.
        let straight = chebyshev((10, 31), (10, 33));
        assert_eq!(straight, 2);
        let d = est.tiles((10, 31), 0, (10, 33), 0);
        // 3 hops × 100 × (32/100) = 96, far above the straight-line 2.
        let expected = (3.0 * 100.0 * ROUTER_UNITS_TO_TILES).round() as i32;
        assert_eq!(d, expected);
        assert!(d > straight, "detour must dominate the straight line");
        // The bug fixed: a near same-side target (chebyshev 11) now beats
        // the across-river target (96) instead of losing to its tiny
        // straight-line distance.
        let same_side = est.tiles((10, 31), 0, (20, 20), 0);
        assert!(same_side < d);
    }

    /// End-to-end against the *production* graph builder
    /// (`rebuild_chunk_graph_sync`) over a real `ChunkMap` with a
    /// `TileKind::River` band — proves the estimator prices the
    /// walk-around through the real River-impassable edge scan, not just
    /// synthetic edges. This is the reported bug scenario in miniature.
    #[test]
    fn real_river_graph_far_bank_loses_to_near_bank() {
        use crate::pathfinding::chunk_graph::rebuild_chunk_graph_sync;
        use crate::world::chunk::{Chunk, ChunkMap};
        use crate::world::tile::TileKind;

        // River at local x=20 for every row, EXCEPT a 4-row ford
        // (ly 0..=3) in the ford chunk that links the two banks.
        fn bank_chunk(is_ford: bool) -> Chunk {
            let surface_z = Box::new([[0i8; 32]; 32]);
            let mut kind = Box::new([[TileKind::Grass; 32]; 32]);
            for ly in 0..32 {
                if is_ford && ly <= 3 {
                    continue;
                }
                kind[ly][20] = TileKind::River;
            }
            let fert = Box::new([[8u8; 32]; 32]);
            Chunk::new(surface_z, kind, fert)
        }

        let mut chunk_map = ChunkMap::default();
        // chunk(0,0) carries the ford; chunk(0,1) is a full split.
        chunk_map.0.insert(ChunkCoord(0, 0), bank_chunk(true));
        chunk_map.0.insert(ChunkCoord(0, 1), bank_chunk(false));

        let mut graph = crate::pathfinding::chunk_graph::ChunkGraph::default();
        rebuild_chunk_graph_sync(&chunk_map, &mut graph);

        let router = ChunkRouter::default();
        let est = DetourEstimator::new(&router, &graph);

        let origin = (5, 40); // chunk(0,1), west bank
        let near_far_bank = (25, 40); // chunk(0,1), east bank — cheb 20
        let far_same_bank = (5, 4); // chunk(0,0), west bank — cheb 36

        let a = est.tiles(origin, 0, near_far_bank, 0);
        let b = est.tiles(origin, 0, far_same_bank, 0);

        assert!(
            a > chebyshev(origin, near_far_bank),
            "far-bank target must cost more than its straight line ({a} vs 20)"
        );
        assert!(
            a > b,
            "chebyshev-nearer far-bank target ({a}) must lose to the \
             chebyshev-farther same-bank target ({b}) — the bug"
        );
    }

    #[test]
    fn unloaded_or_unreachable_falls_back_to_chebyshev() {
        let g = river_split_graph();
        let r = ChunkRouter::default();
        let est = DetourEstimator::new(&r, &g);
        // Candidate chunk not in graph ⇒ node_of None ⇒ chebyshev.
        let far = (900, 900);
        assert_eq!(est.tiles((10, 31), 0, far, 0), chebyshev((10, 31), far));
        // Origin chunk not in graph ⇒ chebyshev.
        assert_eq!(est.tiles(far, 0, (10, 31), 0), chebyshev(far, (10, 31)));
    }
}
