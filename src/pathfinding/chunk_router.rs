use ahash::AHashMap;
use bevy::prelude::*;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Mutex;

use crate::pathfinding::chunk_graph::{ChunkEdge, ChunkGraph};
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};

/// Distance from every reachable chunk to a fixed destination chunk,
/// computed by running Dijkstra outward from the destination over the
/// (undirected) chunk graph using `ChunkEdge::traverse_cost`. Lookup
/// during `first_waypoint` is then O(degree) per call.
pub struct ShortestPathTree {
    pub dist: AHashMap<ChunkCoord, u32>,
}

const ROUTER_CAPACITY: usize = 64;

#[derive(Default)]
struct RouterState {
    trees: AHashMap<ChunkCoord, ShortestPathTree>,
    /// Insertion order, oldest first. Used for FIFO eviction at capacity.
    order: VecDeque<ChunkCoord>,
    /// Tracks which chunk-graph generation the cached trees are valid for.
    /// Mismatch ⇒ wholesale-drop on next access.
    last_seen_generation: u32,
}

/// Cached chunk-level Dijkstra. Wrapped in a `Mutex` so it can be accessed
/// from parallel queries (`par_iter_mut` in `goal_dispatch_system`); the
/// cache is small and the critical section is short, so contention is
/// negligible.
#[derive(Resource, Default)]
pub struct ChunkRouter {
    state: Mutex<RouterState>,
}

impl ChunkRouter {
    /// Returns the global tile coord of the entry tile of the next chunk
    /// the agent should head toward. Z-aware: prefers an edge whose
    /// `exit_z` matches the agent's `current_z`. None when `cur == dest`
    /// or when `dest` is unreachable.
    pub fn first_waypoint(
        &self,
        graph: &ChunkGraph,
        cur: ChunkCoord,
        dest: ChunkCoord,
        current_z: i8,
    ) -> Option<(i32, i32)> {
        if cur == dest {
            return None;
        }
        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(poisoned) => {
                warn!(
                    "[path] ChunkRouter mutex poisoned (cur={:?} dest={:?}); recovering",
                    cur, dest
                );
                poisoned.into_inner()
            }
        };
        maybe_invalidate(&mut state, graph);

        // Build (or fetch) the tree for `dest`. We don't hold a ref into the
        // map across the edge scan because `tree_for` may mutate; instead
        // copy the relevant `dist[neighbor]` values out.
        if !state.trees.contains_key(&dest) {
            let tree = match build_tree(graph, dest) {
                Some(t) => t,
                None => return None,
            };
            while state.trees.len() >= ROUTER_CAPACITY {
                if let Some(victim) = state.order.pop_front() {
                    state.trees.remove(&victim);
                } else {
                    break;
                }
            }
            state.trees.insert(dest, tree);
            state.order.push_back(dest);
        }
        let tree = state.trees.get(&dest)?;
        if !tree.dist.contains_key(&cur) {
            debug!(
                "[path] router: no dist entry for cur={:?} dest={:?} (disconnected component)",
                cur, dest
            );
            return None;
        }

        let edges = graph.edges.get(&cur)?;
        let mut best: Option<&ChunkEdge> = None;
        let mut best_score: u64 = u64::MAX;
        for e in edges {
            let neighbor_dist = match tree.dist.get(&e.neighbor) {
                Some(&d) => d,
                None => continue,
            };
            let mut score: u64 = (e.traverse_cost as u64).saturating_add(neighbor_dist as u64);
            let z_pen = ((e.exit_z as i32 - current_z as i32).abs() as u64) * 32;
            score = score.saturating_add(z_pen);
            if score < best_score {
                best_score = score;
                best = Some(e);
            }
        }
        let chosen = best?;
        let gx = chosen.neighbor.0 * CHUNK_SIZE as i32 + chosen.entry_local.0 as i32;
        let gy = chosen.neighbor.1 * CHUNK_SIZE as i32 + chosen.entry_local.1 as i32;
        Some((gx as i32, gy as i32))
    }

    pub fn cached_destination_count(&self) -> usize {
        self.state.lock().map(|s| s.trees.len()).unwrap_or(0)
    }
}

fn maybe_invalidate(state: &mut RouterState, graph: &ChunkGraph) {
    if graph.generation != state.last_seen_generation {
        state.trees.clear();
        state.order.clear();
        state.last_seen_generation = graph.generation;
    }
}

fn build_tree(graph: &ChunkGraph, dest: ChunkCoord) -> Option<ShortestPathTree> {
    if !graph.edges.contains_key(&dest) {
        return None;
    }
    let mut dist: AHashMap<ChunkCoord, u32> = AHashMap::new();
    // ChunkCoord doesn't impl Ord, so we put the coord components inline
    // in the heap key — Dijkstra only needs the distance for ordering.
    let mut heap: BinaryHeap<Reverse<(u32, i32, i32)>> = BinaryHeap::new();
    dist.insert(dest, 0);
    heap.push(Reverse((0, dest.0, dest.1)));

    while let Some(Reverse((cur_d, cx, cy))) = heap.pop() {
        let cur = ChunkCoord(cx, cy);
        let known = *dist.get(&cur).unwrap_or(&u32::MAX);
        if cur_d > known {
            continue;
        }
        let edges = match graph.edges.get(&cur) {
            Some(e) => e,
            None => continue,
        };
        for e in edges {
            // Graphs are symmetric (every A→B edge has a paired B→A); we
            // approximate the reverse traversal cost using the forward
            // edge's traverse_cost.
            let new_d = cur_d.saturating_add(e.traverse_cost as u32);
            let prev = *dist.get(&e.neighbor).unwrap_or(&u32::MAX);
            if new_d < prev {
                dist.insert(e.neighbor, new_d);
                heap.push(Reverse((new_d, e.neighbor.0, e.neighbor.1)));
            }
        }
    }

    Some(ShortestPathTree { dist })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::ChunkEdge;

    fn edge(neighbor: ChunkCoord, exit_z: i8, entry_z: i8, exit_local: (u8, u8)) -> ChunkEdge {
        ChunkEdge {
            neighbor,
            exit_local,
            exit_z,
            entry_local: (0, exit_local.1),
            entry_z,
            traverse_cost: 100,
        }
    }

    #[test]
    fn first_waypoint_prefers_same_z_edge() {
        let mut graph = ChunkGraph::default();
        graph.edges.insert(
            ChunkCoord(0, 0),
            vec![
                edge(ChunkCoord(1, 0), 0, 0, (31, 5)),
                edge(ChunkCoord(1, 0), -3, -3, (31, 6)),
            ],
        );
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0, (0, 5))]);
        graph.generation = 1;

        let router = ChunkRouter::default();
        let wp = router
            .first_waypoint(&graph, ChunkCoord(0, 0), ChunkCoord(1, 0), -3)
            .unwrap();
        assert_eq!(wp.1, 6);
    }

    #[test]
    fn first_waypoint_returns_none_for_unreachable() {
        let mut graph = ChunkGraph::default();
        graph.edges.insert(ChunkCoord(0, 0), vec![]);
        graph.edges.insert(ChunkCoord(5, 5), vec![]);
        graph.generation = 1;

        let router = ChunkRouter::default();
        assert!(router
            .first_waypoint(&graph, ChunkCoord(0, 0), ChunkCoord(5, 5), 0)
            .is_none());
    }

    #[test]
    fn router_caches_then_invalidates_on_generation_bump() {
        let mut graph = ChunkGraph::default();
        graph.edges.insert(
            ChunkCoord(0, 0),
            vec![edge(ChunkCoord(1, 0), 0, 0, (31, 5))],
        );
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0, (0, 5))]);
        graph.generation = 1;

        let router = ChunkRouter::default();
        router
            .first_waypoint(&graph, ChunkCoord(0, 0), ChunkCoord(1, 0), 0)
            .unwrap();
        assert_eq!(router.cached_destination_count(), 1);

        graph.generation = 2;
        router
            .first_waypoint(&graph, ChunkCoord(0, 0), ChunkCoord(1, 0), 0)
            .unwrap();
        assert_eq!(router.cached_destination_count(), 1);
    }

    #[test]
    fn router_picks_shortest_route_through_intermediate_chunk() {
        let mut graph = ChunkGraph::default();
        graph.edges.insert(
            ChunkCoord(0, 0),
            vec![edge(ChunkCoord(1, 0), 0, 0, (31, 5))],
        );
        graph.edges.insert(
            ChunkCoord(1, 0),
            vec![
                edge(ChunkCoord(0, 0), 0, 0, (0, 5)),
                edge(ChunkCoord(2, 0), 0, 0, (31, 5)),
            ],
        );
        graph
            .edges
            .insert(ChunkCoord(2, 0), vec![edge(ChunkCoord(1, 0), 0, 0, (0, 5))]);
        graph.generation = 1;

        let router = ChunkRouter::default();
        let wp = router
            .first_waypoint(&graph, ChunkCoord(0, 0), ChunkCoord(2, 0), 0)
            .unwrap();
        assert_eq!(wp.0, CHUNK_SIZE as i32);
    }
}
