use ahash::AHashMap;
use bevy::prelude::*;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Mutex;

use crate::pathfinding::chunk_graph::{ChunkGraph, ComponentId};
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};

/// (chunk, component) — graph nodes are component-typed so the router
/// cannot collapse a surface band and a disconnected cave into the
/// same Dijkstra node and produce A→B→A oscillations.
pub type RouterNode = (ChunkCoord, ComponentId);

/// Distance from every reachable router-node to a fixed destination
/// node, computed by Dijkstra outward from the destination over the
/// component-typed chunk graph.
pub struct ShortestPathTree {
    pub dist: AHashMap<RouterNode, u32>,
    /// For each reachable node, the next-hop edge toward the goal —
    /// stored as (next_chunk, next_component, entry_local, entry_z,
    /// edge_traverse_cost) so callers can reconstruct waypoints
    /// without re-querying the edge list.
    pub next_hop: AHashMap<RouterNode, NextHop>,
}

#[derive(Clone)]
pub struct NextHop {
    pub neighbor: ChunkCoord,
    pub neighbor_component: ComponentId,
    pub entry_local: (u8, u8),
    pub entry_z: i8,
}

#[derive(Clone)]
pub struct Waypoint {
    pub neighbor: ChunkCoord,
    pub neighbor_component: ComponentId,
    pub entry_x: i32,
    pub entry_y: i32,
    pub entry_z: i8,
}

const ROUTER_CAPACITY: usize = 64;

#[derive(Default)]
struct RouterState {
    trees: AHashMap<RouterNode, ShortestPathTree>,
    /// Insertion order, oldest first. Used for FIFO eviction at capacity.
    order: VecDeque<RouterNode>,
    /// Tracks which chunk-graph generation the cached trees are valid for.
    /// Mismatch ⇒ wholesale-drop on next access.
    last_seen_generation: u32,
}

/// Cached chunk-level Dijkstra. Wrapped in a `Mutex` so it can be accessed
/// from parallel queries. Cache is small and the critical section short,
/// so contention is negligible.
#[derive(Resource, Default)]
pub struct ChunkRouter {
    state: Mutex<RouterState>,
}

impl ChunkRouter {
    /// Component-precise route from `start` to `goal`, returned as the
    /// sequence of chunks the agent will visit (start at index 0, goal
    /// at the back). `None` when goal is unreachable.
    ///
    /// Used by the worker, which knows both endpoints' (lx, ly, z) and
    /// can derive the precise component on each side.
    pub fn compute_route(
        &self,
        graph: &ChunkGraph,
        start: RouterNode,
        goal: RouterNode,
    ) -> Option<Vec<ChunkCoord>> {
        if start == goal {
            return Some(vec![start.0]);
        }
        let mut state = lock_state(&self.state, start, goal);
        maybe_invalidate(&mut state, graph);
        ensure_tree(&mut state, graph, goal)?;
        let tree = state.trees.get(&goal)?;
        if !tree.dist.contains_key(&start) {
            return None;
        }
        // Walk next_hop from start to goal.
        let mut route: Vec<ChunkCoord> = vec![start.0];
        let mut cur = start;
        while cur != goal {
            let hop = tree.next_hop.get(&cur)?;
            route.push(hop.neighbor);
            cur = (hop.neighbor, hop.neighbor_component);
            // Defensive cap: tree.next_hop forms a strict descent toward
            // `goal`, so the loop must terminate. The bound is just to
            // prevent any pathological cycle from hanging the worker.
            if route.len() > 4096 {
                return None;
            }
        }
        Some(route)
    }

    /// Like `compute_route` but returns the first cross-chunk waypoint
    /// (with full entry coords + neighbour component) instead of the
    /// chunk-id list. None when start == goal.
    pub fn first_waypoint_full(
        &self,
        graph: &ChunkGraph,
        start: RouterNode,
        goal: RouterNode,
    ) -> Option<Waypoint> {
        if start == goal {
            return None;
        }
        let mut state = lock_state(&self.state, start, goal);
        maybe_invalidate(&mut state, graph);
        ensure_tree(&mut state, graph, goal)?;
        let tree = state.trees.get(&goal)?;
        let hop = tree.next_hop.get(&start)?;
        Some(Waypoint {
            neighbor: hop.neighbor,
            neighbor_component: hop.neighbor_component,
            entry_x: hop.neighbor.0 * CHUNK_SIZE as i32 + hop.entry_local.0 as i32,
            entry_y: hop.neighbor.1 * CHUNK_SIZE as i32 + hop.entry_local.1 as i32,
            entry_z: hop.entry_z,
        })
    }

    /// Legacy API: returns the global tile coord of the entry tile of
    /// the next chunk to visit. Resolves component identity by trying
    /// every component present at `current_z` in `cur` and `dest`,
    /// picking the cheapest first hop. None when cur == dest, when
    /// `current_z` doesn't classify into any component, or when no
    /// route exists.
    ///
    /// Used by external callers (HTN, tasks) that don't track component
    /// ids. The worker uses `compute_route` / `first_waypoint_full`
    /// directly.
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
        let cur_comps = graph.components_at_z(cur, current_z);
        if cur_comps.is_empty() {
            return None;
        }
        let dest_comps = graph.components_at_z(dest, current_z);
        if dest_comps.is_empty() {
            return None;
        }

        let mut best: Option<(u32, Waypoint)> = None;
        for &dc in &dest_comps {
            let goal = (dest, dc);
            let mut state = lock_state(&self.state, (cur, ComponentId(0)), goal);
            maybe_invalidate(&mut state, graph);
            if ensure_tree(&mut state, graph, goal).is_none() {
                continue;
            }
            let tree = match state.trees.get(&goal) {
                Some(t) => t,
                None => continue,
            };
            for &cc in &cur_comps {
                let start = (cur, cc);
                let Some(&d) = tree.dist.get(&start) else {
                    continue;
                };
                let Some(hop) = tree.next_hop.get(&start) else {
                    continue;
                };
                let wp = Waypoint {
                    neighbor: hop.neighbor,
                    neighbor_component: hop.neighbor_component,
                    entry_x: hop.neighbor.0 * CHUNK_SIZE as i32 + hop.entry_local.0 as i32,
                    entry_y: hop.neighbor.1 * CHUNK_SIZE as i32 + hop.entry_local.1 as i32,
                    entry_z: hop.entry_z,
                };
                match best {
                    Some((bd, _)) if bd <= d => {}
                    _ => best = Some((d, wp)),
                }
            }
        }
        best.map(|(_, wp)| (wp.entry_x, wp.entry_y))
    }

    /// Reachability test in the component graph: are `start` and `goal`
    /// in the same connected component? O(degree) using a cached tree.
    pub fn is_reachable(
        &self,
        graph: &ChunkGraph,
        start: RouterNode,
        goal: RouterNode,
    ) -> bool {
        if start == goal {
            return true;
        }
        let mut state = lock_state(&self.state, start, goal);
        maybe_invalidate(&mut state, graph);
        if ensure_tree(&mut state, graph, goal).is_none() {
            return false;
        }
        match state.trees.get(&goal) {
            Some(tree) => tree.dist.contains_key(&start),
            None => false,
        }
    }

    pub fn cached_destination_count(&self) -> usize {
        self.state.lock().map(|s| s.trees.len()).unwrap_or(0)
    }
}

fn lock_state<'a>(
    mu: &'a Mutex<RouterState>,
    cur: RouterNode,
    dest: RouterNode,
) -> std::sync::MutexGuard<'a, RouterState> {
    match mu.lock() {
        Ok(s) => s,
        Err(poisoned) => {
            warn!(
                "[path] ChunkRouter mutex poisoned (cur={:?} dest={:?}); recovering",
                cur, dest
            );
            poisoned.into_inner()
        }
    }
}

fn maybe_invalidate(state: &mut RouterState, graph: &ChunkGraph) {
    if graph.generation != state.last_seen_generation {
        state.trees.clear();
        state.order.clear();
        state.last_seen_generation = graph.generation;
    }
}

fn ensure_tree(state: &mut RouterState, graph: &ChunkGraph, goal: RouterNode) -> Option<()> {
    if state.trees.contains_key(&goal) {
        return Some(());
    }
    let tree = build_tree(graph, goal)?;
    while state.trees.len() >= ROUTER_CAPACITY {
        if let Some(victim) = state.order.pop_front() {
            state.trees.remove(&victim);
        } else {
            break;
        }
    }
    state.trees.insert(goal, tree);
    state.order.push_back(goal);
    Some(())
}

fn build_tree(graph: &ChunkGraph, goal: RouterNode) -> Option<ShortestPathTree> {
    if !graph.components.contains_key(&goal.0) {
        return None;
    }
    let mut dist: AHashMap<RouterNode, u32> = AHashMap::new();
    let mut next_hop: AHashMap<RouterNode, NextHop> = AHashMap::new();
    // ChunkCoord doesn't impl Ord, so we put the coord/component inline
    // in the heap key.
    let mut heap: BinaryHeap<Reverse<(u32, i32, i32, u8)>> = BinaryHeap::new();
    dist.insert(goal, 0);
    heap.push(Reverse((0, goal.0 .0, goal.0 .1, goal.1 .0)));

    while let Some(Reverse((cur_d, cx, cy, cc))) = heap.pop() {
        let cur: RouterNode = (ChunkCoord(cx, cy), ComponentId(cc));
        let known = *dist.get(&cur).unwrap_or(&u32::MAX);
        if cur_d > known {
            continue;
        }
        // Walk edges leaving this chunk; we relax edges that *arrive* at
        // (cur). Since the graph is symmetric (every A→B has a paired
        // B→A produced by the build's per-chunk border scan), an edge
        // `(neighbor → cur)` lives in `graph.edges[neighbor]` with
        // `to_component = cur.1`. For Dijkstra we want the predecessor
        // distance, so iterate neighbours' edges that target `cur` —
        // that's expensive. Easier: the symmetric paired edge in
        // `graph.edges[cur.0]` has `from_component = cur.1` and
        // points to (neighbor, neighbor_component) — the same back-edge.
        let edges = match graph.edges.get(&cur.0) {
            Some(e) => e,
            None => continue,
        };
        for e in edges {
            if e.from_component != cur.1 {
                continue;
            }
            let neighbor: RouterNode = (e.neighbor, e.to_component);
            let new_d = cur_d.saturating_add(e.traverse_cost as u32);
            let prev = *dist.get(&neighbor).unwrap_or(&u32::MAX);
            if new_d < prev {
                dist.insert(neighbor, new_d);
                // The next-hop FROM `neighbor` toward `goal` is the
                // paired reverse edge: it crosses from `neighbor` chunk
                // back into `cur.0`, entering cur's chunk at the tile
                // recorded as `e.exit_local`/`e.exit_z` (that's where the
                // forward edge departed, so the reverse edge arrives there).
                next_hop.insert(
                    neighbor,
                    NextHop {
                        neighbor: cur.0,
                        neighbor_component: cur.1,
                        entry_local: e.exit_local,
                        entry_z: e.exit_z,
                    },
                );
                heap.push(Reverse((new_d, e.neighbor.0, e.neighbor.1, e.to_component.0)));
            }
        }
    }

    Some(ShortestPathTree { dist, next_hop })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::{ChunkComponents, ChunkEdge, ChunkGraph};

    fn comp_one(coord: ChunkCoord, count: u8) -> ChunkComponents {
        let mut at = AHashMap::new();
        // Just put a single sentinel cell at (0,0,0) — tests use compute_route
        // which doesn't read individual cells, only the edge list.
        for c in 0..count {
            at.insert((c, 0, 0), ComponentId(c));
        }
        ChunkComponents { at, count }
    }

    fn edge(
        neighbor: ChunkCoord,
        exit_z: i8,
        entry_z: i8,
        exit_local: (u8, u8),
        from_c: u8,
        to_c: u8,
    ) -> ChunkEdge {
        ChunkEdge {
            neighbor,
            exit_local,
            exit_z,
            entry_local: (0, exit_local.1),
            entry_z,
            traverse_cost: 100,
            from_component: ComponentId(from_c),
            to_component: ComponentId(to_c),
        }
    }

    #[test]
    fn compute_route_through_one_intermediate() {
        let mut graph = ChunkGraph::default();
        graph.components.insert(ChunkCoord(0, 0), comp_one(ChunkCoord(0, 0), 1));
        graph.components.insert(ChunkCoord(1, 0), comp_one(ChunkCoord(1, 0), 1));
        graph.components.insert(ChunkCoord(2, 0), comp_one(ChunkCoord(2, 0), 1));
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0, (31, 5), 0, 0)]);
        graph.edges.insert(
            ChunkCoord(1, 0),
            vec![
                edge(ChunkCoord(0, 0), 0, 0, (0, 5), 0, 0),
                edge(ChunkCoord(2, 0), 0, 0, (31, 5), 0, 0),
            ],
        );
        graph
            .edges
            .insert(ChunkCoord(2, 0), vec![edge(ChunkCoord(1, 0), 0, 0, (0, 5), 0, 0)]);
        graph.generation = 1;

        let router = ChunkRouter::default();
        let route = router
            .compute_route(
                &graph,
                (ChunkCoord(0, 0), ComponentId(0)),
                (ChunkCoord(2, 0), ComponentId(0)),
            )
            .expect("route exists");
        assert_eq!(
            route,
            vec![ChunkCoord(0, 0), ChunkCoord(1, 0), ChunkCoord(2, 0)]
        );
    }

    #[test]
    fn unreachable_components_within_chunk_dont_oscillate() {
        // Chunk B has two components: surface (0) and cave (1). Edge from
        // A surface (0) goes to B surface (0). Edge from A cave (1) goes
        // to B cave (1). Trying to route from A surface to B cave must
        // return None (not bounce A→B→A).
        let mut graph = ChunkGraph::default();
        graph.components.insert(ChunkCoord(0, 0), comp_one(ChunkCoord(0, 0), 2));
        graph.components.insert(ChunkCoord(1, 0), comp_one(ChunkCoord(1, 0), 2));
        graph.edges.insert(
            ChunkCoord(0, 0),
            vec![
                edge(ChunkCoord(1, 0), 0, 0, (31, 5), 0, 0),
                edge(ChunkCoord(1, 0), -5, -5, (31, 6), 1, 1),
            ],
        );
        graph.edges.insert(
            ChunkCoord(1, 0),
            vec![
                edge(ChunkCoord(0, 0), 0, 0, (0, 5), 0, 0),
                edge(ChunkCoord(0, 0), -5, -5, (0, 6), 1, 1),
            ],
        );
        graph.generation = 1;

        let router = ChunkRouter::default();
        let route = router.compute_route(
            &graph,
            (ChunkCoord(0, 0), ComponentId(0)), // A surface
            (ChunkCoord(1, 0), ComponentId(1)), // B cave
        );
        assert!(
            route.is_none(),
            "surface and cave components must not connect through a 2D chunk node"
        );
    }

    #[test]
    fn router_caches_then_invalidates_on_generation_bump() {
        let mut graph = ChunkGraph::default();
        graph.components.insert(ChunkCoord(0, 0), comp_one(ChunkCoord(0, 0), 1));
        graph.components.insert(ChunkCoord(1, 0), comp_one(ChunkCoord(1, 0), 1));
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0, (31, 5), 0, 0)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0, (0, 5), 0, 0)]);
        graph.generation = 1;

        let router = ChunkRouter::default();
        router
            .compute_route(
                &graph,
                (ChunkCoord(0, 0), ComponentId(0)),
                (ChunkCoord(1, 0), ComponentId(0)),
            )
            .unwrap();
        assert_eq!(router.cached_destination_count(), 1);

        graph.generation = 2;
        router
            .compute_route(
                &graph,
                (ChunkCoord(0, 0), ComponentId(0)),
                (ChunkCoord(1, 0), ComponentId(0)),
            )
            .unwrap();
        assert_eq!(router.cached_destination_count(), 1);
    }
}
