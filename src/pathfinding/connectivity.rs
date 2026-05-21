//! Reachability facade over the component-typed `ChunkGraph`.
//!
//! Historical note: this module used to maintain its own union-find over
//! `(ChunkCoord, ZBand)` where `ZBand = z.div_euclid(4)`. The 4-z-level
//! buckets could merge a surface ramp at z=2 with a cave at z=-1 if any
//! single connecting tile happened to land in the same band, producing
//! false-positive reachability and indirectly causing A→B→A oscillations
//! in the chunk router.
//!
//! The classifier now lives in `chunk_graph.rs` (`ChunkComponents`) and
//! produces exact 3D-flood-fill components per chunk. This module is a
//! thin self-contained snapshot that answers `is_reachable((c, z), (c, z))`
//! in O(1) without needing a `&ChunkGraph` borrow at call time, so the
//! original API survives.

use ahash::AHashMap;
use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use std::time::Instant;

use crate::pathfinding::chunk_graph::{ChunkComponents, ChunkEdge, ChunkGraph, ComponentId};
use crate::simulation::perf::{micros_u32, BackgroundWorkDiagnostics};
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};

/// Coarse z-band used by debug visualisations to slice the world for
/// per-band gizmo overlays. NOT used by reachability — components are
/// exact, not banded.
pub type ZBand = i8;

pub fn z_band(z: i8) -> ZBand {
    (z as i32).div_euclid(4) as i8
}

/// Self-contained reachability snapshot. Built from `ChunkGraph` once
/// per rebuild; queried in O(set-intersection) at call sites that don't
/// hold a `&ChunkGraph`.
#[derive(Resource, Default)]
pub struct ChunkConnectivity {
    /// For each (chunk, z): the inter-chunk connected-component ids of
    /// every component that has a cell at exactly that z. Multiple ids
    /// can appear when a chunk has two disconnected components touching
    /// the same z slice.
    ///
    /// Retained for backwards-compat `is_reachable((chunk, z), …)` and
    /// the debug overlay. Gameplay paths should prefer `tile_reachable`,
    /// which routes via the component-precise `cc_by_component` map.
    cc_at_z: AHashMap<(ChunkCoord, i8), Vec<u32>>,
    /// Component-precise `(chunk, ComponentId) -> inter-chunk CC id`.
    /// Built directly from `build_connectivity_components` and used by
    /// `component_reachable` / `tile_reachable`.
    cc_by_component: AHashMap<(ChunkCoord, ComponentId), u32>,
    /// Total node count for the debug panel.
    cc_total_nodes: usize,
    /// Distinct CC ids — exposed as `component_count`.
    cc_distinct: usize,
    /// `(chunk, z_band, cc_id)` triples for the debug-overlay iterator.
    overlay_entries: Vec<(ChunkCoord, ZBand, u32)>,
    pub generation: u32,
}

struct ConnectivityGraphSnapshot {
    edges: AHashMap<ChunkCoord, Vec<ChunkEdge>>,
    components: AHashMap<ChunkCoord, ChunkComponents>,
    generation: u32,
}

struct ConnectivityRebuildResult {
    cc_at_z: AHashMap<(ChunkCoord, i8), Vec<u32>>,
    cc_by_component: AHashMap<(ChunkCoord, ComponentId), u32>,
    cc_total_nodes: usize,
    cc_distinct: usize,
    overlay_entries: Vec<(ChunkCoord, ZBand, u32)>,
    generation: u32,
    elapsed: std::time::Duration,
}

#[derive(Resource, Default)]
pub struct ConnectivityRebuildTask(Option<Task<ConnectivityRebuildResult>>);

impl ChunkConnectivity {
    pub fn is_reachable(&self, from: (ChunkCoord, i8), to: (ChunkCoord, i8)) -> bool {
        if from == to {
            return true;
        }
        let Some(a) = self.cc_at_z.get(&from) else {
            return false;
        };
        let Some(b) = self.cc_at_z.get(&to) else {
            return false;
        };
        for x in a {
            if b.contains(x) {
                return true;
            }
        }
        false
    }

    pub fn component_count(&self) -> usize {
        self.cc_distinct
    }

    pub fn node_count(&self) -> usize {
        self.cc_total_nodes
    }

    /// First CC id at `(chunk, z)`, or None when no component classifies
    /// at that z.
    pub fn component_of(&self, chunk: ChunkCoord, z: i8) -> Option<u32> {
        self.cc_at_z.get(&(chunk, z))?.first().copied()
    }

    /// Exact inter-chunk CC id for a specific `(chunk, ComponentId)` node.
    /// Returns `None` when the node isn't classified (chunk not built,
    /// component id stale across a rebuild).
    pub fn cc_of_component(&self, chunk: ChunkCoord, component: ComponentId) -> Option<u32> {
        self.cc_by_component.get(&(chunk, component)).copied()
    }

    /// True iff the two `(chunk, component)` nodes share an inter-chunk
    /// connected component. Component-precise — no false positives from
    /// surface/cave coexisting at the same `(chunk, z)`.
    pub fn component_reachable(
        &self,
        from: (ChunkCoord, ComponentId),
        to: (ChunkCoord, ComponentId),
    ) -> bool {
        if from == to {
            return true;
        }
        match (
            self.cc_by_component.get(&from),
            self.cc_by_component.get(&to),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Component-precise reachability between two exact 3D tiles. Looks
    /// up each tile's chunk-graph component via
    /// `ChunkGraph::component_for_tile` and tests CC equality.
    ///
    /// Use this for gameplay routing decisions instead of
    /// `is_reachable((chunk, z), …)`: the `(chunk, z)` overload OR-merges
    /// every component touching `z` in `chunk` and can return `true`
    /// when the agent's actual cell is sealed in a disconnected
    /// component that happens to share a `z` slice.
    pub fn tile_reachable(
        &self,
        graph: &ChunkGraph,
        from: (i32, i32, i8),
        to: (i32, i32, i8),
    ) -> bool {
        if from == to {
            return true;
        }
        if self.generation != graph.generation {
            return true;
        }
        let csz = CHUNK_SIZE as i32;
        let from_chunk = ChunkCoord(from.0.div_euclid(csz), from.1.div_euclid(csz));
        let to_chunk = ChunkCoord(to.0.div_euclid(csz), to.1.div_euclid(csz));
        let Some(from_c) = graph.component_for_tile(from.0, from.1, from.2) else {
            return false;
        };
        let Some(to_c) = graph.component_for_tile(to.0, to.1, to.2) else {
            return false;
        };
        self.component_reachable((from_chunk, from_c), (to_chunk, to_c))
    }

    /// `(chunk, z_band, cc_id)` triples for the per-band debug overlay.
    /// Each unique (band, cc) pair appears once per chunk.
    pub fn iter(&self) -> impl Iterator<Item = (ChunkCoord, ZBand, u32)> + '_ {
        self.overlay_entries.iter().copied()
    }
}

fn uf_find(parent: &mut [usize], x: usize) -> usize {
    let mut root = x;
    while parent[root] != root {
        root = parent[root];
    }
    let mut cur = x;
    while parent[cur] != root {
        let next = parent[cur];
        parent[cur] = root;
        cur = next;
    }
    root
}

fn uf_union(parent: &mut [usize], rank: &mut [u8], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra == rb {
        return;
    }
    let (lo, hi) = if rank[ra] < rank[rb] {
        (ra, rb)
    } else {
        (rb, ra)
    };
    parent[lo] = hi;
    if rank[lo] == rank[hi] {
        rank[hi] = rank[hi].saturating_add(1);
    }
}

/// Compute the per-(chunk, ComponentId) connected-component map from
/// `graph`. Each `ChunkEdge` unifies its two endpoint nodes; intra-chunk
/// component identity is exact courtesy of the chunk-graph flood-fill,
/// so no separate intra-chunk pass is needed.
pub fn build_connectivity_components(
    graph: &ChunkGraph,
) -> AHashMap<(ChunkCoord, ComponentId), u32> {
    let mut nodes: Vec<(ChunkCoord, ComponentId)> = Vec::new();
    let mut idx: AHashMap<(ChunkCoord, ComponentId), usize> = AHashMap::new();

    let intern = |key: (ChunkCoord, ComponentId),
                  nodes: &mut Vec<(ChunkCoord, ComponentId)>,
                  idx: &mut AHashMap<(ChunkCoord, ComponentId), usize>|
     -> usize {
        if let Some(&i) = idx.get(&key) {
            i
        } else {
            let i = nodes.len();
            nodes.push(key);
            idx.insert(key, i);
            i
        }
    };

    // Intern every (chunk, component) — even ones with no inter-chunk
    // edges (an isolated cave system in a single chunk is a valid CC).
    for (coord, cc) in &graph.components {
        for c in 0..cc.count {
            intern((*coord, ComponentId(c)), &mut nodes, &mut idx);
        }
    }
    // Defensive: edges may reference component ids beyond what the
    // components map recorded if state is mid-rebuild.
    for (coord, edges) in &graph.edges {
        for e in edges {
            intern((*coord, e.from_component), &mut nodes, &mut idx);
            intern((e.neighbor, e.to_component), &mut nodes, &mut idx);
        }
    }

    let n = nodes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut rank = vec![0u8; n];

    for (coord, edges) in &graph.edges {
        for e in edges {
            let from = idx[&(*coord, e.from_component)];
            let to = idx[&(e.neighbor, e.to_component)];
            uf_union(&mut parent, &mut rank, from, to);
        }
    }

    let mut out: AHashMap<(ChunkCoord, ComponentId), u32> = AHashMap::with_capacity(n);
    let mut root_to_id: AHashMap<usize, u32> = AHashMap::new();
    let mut next_id: u32 = 0;
    for i in 0..n {
        let root = uf_find(&mut parent, i);
        let id = *root_to_id.entry(root).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        out.insert(nodes[i], id);
    }
    out
}

/// Repopulates `conn` from `graph`. Shared between the live
/// `rebuild_connectivity_system` (which has Bevy resource accessors) and
/// out-of-module tests that build their own `ChunkGraph` directly.
pub fn populate_connectivity_from_graph(graph: &ChunkGraph, conn: &mut ChunkConnectivity) {
    let cc_map = build_connectivity_components(graph);
    let mut cc_at_z: AHashMap<(ChunkCoord, i8), Vec<u32>> = AHashMap::new();
    let mut overlay_seen: ahash::AHashSet<(ChunkCoord, ZBand, u32)> = ahash::AHashSet::new();
    let mut overlay: Vec<(ChunkCoord, ZBand, u32)> = Vec::new();
    let mut distinct: ahash::AHashSet<u32> = ahash::AHashSet::new();
    for (coord, components) in &graph.components {
        for (&(_, _, z), &cid) in &components.at {
            let Some(&cc_id) = cc_map.get(&(*coord, cid)) else {
                continue;
            };
            distinct.insert(cc_id);
            let entry = cc_at_z.entry((*coord, z)).or_default();
            if !entry.contains(&cc_id) {
                entry.push(cc_id);
            }
            let band = z_band(z);
            let key = (*coord, band, cc_id);
            if overlay_seen.insert(key) {
                overlay.push(key);
            }
        }
    }
    conn.cc_at_z = cc_at_z;
    conn.cc_by_component = cc_map;
    conn.overlay_entries = overlay;
    conn.cc_total_nodes = conn.cc_by_component.len();
    conn.cc_distinct = distinct.len();
    conn.generation = conn.generation.wrapping_add(1);
}

pub fn rebuild_connectivity_system(graph: Res<ChunkGraph>, mut conn: ResMut<ChunkConnectivity>) {
    populate_connectivity_from_graph(&graph, &mut conn);
}

pub fn spawn_connectivity_rebuild_task_system(
    graph: Res<ChunkGraph>,
    conn: Res<ChunkConnectivity>,
    mut task: ResMut<ConnectivityRebuildTask>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    perf.connectivity_in_flight = task.0.is_some();
    perf.connectivity_generation = conn.generation;
    if task.0.is_some() || graph.generation == conn.generation {
        return;
    }

    let snapshot = ConnectivityGraphSnapshot {
        edges: graph.edges.clone(),
        components: graph.components.clone(),
        generation: graph.generation,
    };
    let pool = AsyncComputeTaskPool::get();
    task.0 = Some(pool.spawn(async move { rebuild_connectivity_offthread(snapshot) }));
    perf.connectivity_in_flight = true;
}

pub fn poll_connectivity_rebuild_task_system(
    graph: Res<ChunkGraph>,
    mut conn: ResMut<ChunkConnectivity>,
    mut task: ResMut<ConnectivityRebuildTask>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    let Some(t) = task.0.as_mut() else {
        perf.connectivity_in_flight = false;
        perf.connectivity_generation = conn.generation;
        return;
    };
    let Some(result) = block_on(future::poll_once(t)) else {
        perf.connectivity_in_flight = true;
        perf.connectivity_generation = conn.generation;
        return;
    };
    task.0 = None;
    perf.connectivity_in_flight = false;

    if result.generation != graph.generation {
        perf.connectivity_dropped_stale = perf.connectivity_dropped_stale.saturating_add(1);
        perf.connectivity_compute_us = micros_u32(result.elapsed);
        perf.connectivity_generation = conn.generation;
        return;
    }

    let apply_started = Instant::now();
    conn.cc_at_z = result.cc_at_z;
    conn.cc_by_component = result.cc_by_component;
    conn.cc_total_nodes = result.cc_total_nodes;
    conn.cc_distinct = result.cc_distinct;
    conn.overlay_entries = result.overlay_entries;
    conn.generation = result.generation;
    perf.connectivity_compute_us = micros_u32(result.elapsed);
    perf.connectivity_apply_us = micros_u32(apply_started.elapsed());
    perf.connectivity_generation = conn.generation;
}

fn rebuild_connectivity_offthread(
    snapshot: ConnectivityGraphSnapshot,
) -> ConnectivityRebuildResult {
    let started = Instant::now();
    let graph = ChunkGraph {
        edges: snapshot.edges,
        components: snapshot.components,
        generation: snapshot.generation,
    };
    let mut conn = ChunkConnectivity::default();
    populate_connectivity_from_graph(&graph, &mut conn);
    ConnectivityRebuildResult {
        cc_at_z: conn.cc_at_z,
        cc_by_component: conn.cc_by_component,
        cc_total_nodes: conn.cc_total_nodes,
        cc_distinct: conn.cc_distinct,
        overlay_entries: conn.overlay_entries,
        generation: snapshot.generation,
        elapsed: started.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::{ChunkComponents, ChunkEdge, ChunkGraph};

    fn comp_at_z(coord: ChunkCoord, z: i8, count: u8) -> ChunkComponents {
        let mut at = AHashMap::new();
        for c in 0..count {
            at.insert((c, 0, z), ComponentId(c));
        }
        ChunkComponents { at, count }
    }

    fn edge(neighbor: ChunkCoord, from_c: u8, to_c: u8) -> ChunkEdge {
        ChunkEdge {
            neighbor,
            exit_local: (0, 0),
            exit_z: 0,
            entry_local: (0, 0),
            entry_z: 0,
            traverse_cost: 100,
            from_component: ComponentId(from_c),
            to_component: ComponentId(to_c),
        }
    }

    fn rebuild(graph: &ChunkGraph) -> ChunkConnectivity {
        let cc_map = build_connectivity_components(graph);
        let mut cc_at_z: AHashMap<(ChunkCoord, i8), Vec<u32>> = AHashMap::new();
        let mut distinct: ahash::AHashSet<u32> = ahash::AHashSet::new();
        for (coord, components) in &graph.components {
            for (&(_, _, z), &cid) in &components.at {
                if let Some(&cc_id) = cc_map.get(&(*coord, cid)) {
                    distinct.insert(cc_id);
                    let entry = cc_at_z.entry((*coord, z)).or_default();
                    if !entry.contains(&cc_id) {
                        entry.push(cc_id);
                    }
                }
            }
        }
        let total_nodes = cc_map.len();
        ChunkConnectivity {
            cc_at_z,
            cc_by_component: cc_map,
            cc_total_nodes: total_nodes,
            cc_distinct: distinct.len(),
            overlay_entries: Vec::new(),
            generation: 0,
        }
    }

    #[test]
    fn two_connected_chunks_reach_at_their_shared_z() {
        let mut graph = ChunkGraph::default();
        graph
            .components
            .insert(ChunkCoord(0, 0), comp_at_z(ChunkCoord(0, 0), 0, 1));
        graph
            .components
            .insert(ChunkCoord(1, 0), comp_at_z(ChunkCoord(1, 0), 0, 1));
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0)]);
        let conn = rebuild(&graph);
        assert!(conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), 0)));
    }

    #[test]
    fn isolated_chunks_unreachable() {
        let mut graph = ChunkGraph::default();
        graph
            .components
            .insert(ChunkCoord(0, 0), comp_at_z(ChunkCoord(0, 0), 0, 1));
        graph
            .components
            .insert(ChunkCoord(5, 5), comp_at_z(ChunkCoord(5, 5), 0, 1));
        let conn = rebuild(&graph);
        assert!(!conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(5, 5), 0)));
    }

    #[test]
    fn tile_reachable_succeeds_across_z_offset() {
        // Chunk A's only standable cell is at z=0, chunk B's at z=1, joined
        // by a stair-step edge. The legacy `(chunk, z)` check between
        // B@z=0 fails (no component there), but exact tile reachability
        // between (A tile, z=0) and (B tile, z=1) succeeds.
        let mut a = AHashMap::new();
        a.insert((0u8, 0u8, 0i8), ComponentId(0));
        let mut b = AHashMap::new();
        b.insert((0u8, 0u8, 1i8), ComponentId(0));
        let mut graph = ChunkGraph::default();
        graph
            .components
            .insert(ChunkCoord(0, 0), ChunkComponents { at: a, count: 1 });
        graph
            .components
            .insert(ChunkCoord(1, 0), ChunkComponents { at: b, count: 1 });
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0)]);
        let conn = rebuild(&graph);

        // Legacy chunk/z check rejects because chunk B has no component at z=0.
        assert!(
            !conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), 0)),
            "legacy same-z chunk check must fail for B@z=0 — no component there"
        );

        // Exact tile reachability between (A's tile at z=0) and (B's tile
        // at z=1) succeeds — the stair-step edge unifies their components.
        let csz = CHUNK_SIZE as i32;
        let a_tile = (0, 0, 0i8);
        let b_tile = (csz, 0, 1i8);
        assert!(
            conn.tile_reachable(&graph, a_tile, b_tile),
            "exact tile reachability must follow component identity, not (chunk, z)"
        );
    }

    #[test]
    fn surface_and_cave_in_same_chunk_dont_share_cc() {
        // Component 0 at z=0, component 1 at z=-5 (disconnected). Surface
        // travels across to chunk (1,0), cave doesn't. Asking
        // is_reachable((0,0)@0, (1,0)@-5) must be false.
        let mut a = AHashMap::new();
        a.insert((0u8, 0u8, 0i8), ComponentId(0));
        a.insert((1u8, 0u8, -5i8), ComponentId(1));
        let mut b = AHashMap::new();
        b.insert((0u8, 0u8, 0i8), ComponentId(0));
        b.insert((1u8, 0u8, -5i8), ComponentId(1));
        let mut graph = ChunkGraph::default();
        graph
            .components
            .insert(ChunkCoord(0, 0), ChunkComponents { at: a, count: 2 });
        graph
            .components
            .insert(ChunkCoord(1, 0), ChunkComponents { at: b, count: 2 });
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0)]);
        let conn = rebuild(&graph);
        assert!(conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), 0)));
        assert!(!conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), -5)));
    }
}
