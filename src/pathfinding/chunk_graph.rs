use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use std::collections::VecDeque;
use std::time::Instant;

use crate::pathfinding::tile_cost::{tile_step_cost, IMPASSABLE};
use crate::simulation::perf::{micros_u32, BackgroundWorkDiagnostics, PerfWorkBudget};
use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MIN};
use crate::world::chunk_streaming::{ChunkLoadedEvent, ChunkUnloadedEvent, TileChangedEvent};

/// All 26 neighbour offsets in (dx, dy, dz). Used by intra-chunk
/// component flood-fill — same set the old `connectivity` module used.
pub const NEIGHBOR_DIRS_3D: [(i32, i32, i32); 26] = [
    // Same Z: 8 horizontal neighbours
    (-1, -1, 0),
    (-1, 0, 0),
    (-1, 1, 0),
    (0, -1, 0),
    (0, 1, 0),
    (1, -1, 0),
    (1, 0, 0),
    (1, 1, 0),
    // Z+1
    (-1, -1, 1),
    (-1, 0, 1),
    (-1, 1, 1),
    (0, -1, 1),
    (0, 0, 1),
    (0, 1, 1),
    (1, -1, 1),
    (1, 0, 1),
    (1, 1, 1),
    // Z-1
    (-1, -1, -1),
    (-1, 0, -1),
    (-1, 1, -1),
    (0, -1, -1),
    (0, 0, -1),
    (0, 1, -1),
    (1, -1, -1),
    (1, 0, -1),
    (1, 1, -1),
];

/// Chunk-local connected-component id. Components rarely exceed a
/// handful (surface + a couple of disconnected cave systems), so `u8`
/// is plenty.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash, Default)]
pub struct ComponentId(pub u8);

/// Per-chunk classification of every standable foot tile into a
/// connected component computed by 3D flood-fill at graph-build time.
/// Sparse — only standable cells appear.
#[derive(Default, Clone)]
pub struct ChunkComponents {
    pub at: AHashMap<(u8, u8, i8), ComponentId>,
    pub count: u8,
}

impl ChunkComponents {
    pub fn component_at(&self, lx: u8, ly: u8, z: i8) -> Option<ComponentId> {
        self.at.get(&(lx, ly, z)).copied()
    }
}

#[derive(Clone)]
pub struct ChunkEdge {
    pub neighbor: ChunkCoord,
    /// Tile in this chunk that borders the neighbor (local coords 0..CHUNK_SIZE-1).
    pub exit_local: (u8, u8),
    /// Z slice of the exit tile (foot Z of the agent crossing).
    pub exit_z: i8,
    /// Corresponding tile in the neighbor chunk.
    pub entry_local: (u8, u8),
    /// Z slice of the entry tile in the neighbor.
    pub entry_z: i8,
    /// Cost to traverse this edge (entry tile's `tile_step_cost` plus a
    /// small Z-change penalty matching A*/flow-field rules). Used by
    /// `ChunkRouter`'s weighted Dijkstra.
    pub traverse_cost: u16,
    /// Component on the source side of this edge.
    pub from_component: ComponentId,
    /// Component on the neighbour side.
    pub to_component: ComponentId,
}

#[derive(Resource, Default)]
pub struct ChunkGraph {
    pub edges: AHashMap<ChunkCoord, Vec<ChunkEdge>>,
    /// Per-chunk connected-component classification of standable foot
    /// tiles. Used by the router (component-typed graph nodes) and by
    /// `ChunkConnectivity` for reachability checks.
    pub components: AHashMap<ChunkCoord, ChunkComponents>,
    /// Bumped every time the graph rebuilds so dependent caches
    /// (ChunkRouter, ChunkConnectivity) can invalidate.
    pub generation: u32,
}

impl ChunkGraph {
    /// Component id for the standable cell at (`world_x`, `world_y`, `z`),
    /// or `None` if the cell isn't classified (not standable, or chunk
    /// not yet built).
    pub fn component_for_tile(&self, world_x: i32, world_y: i32, z: i8) -> Option<ComponentId> {
        let csz = CHUNK_SIZE as i32;
        let coord = ChunkCoord(world_x.div_euclid(csz), world_y.div_euclid(csz));
        let lx = world_x.rem_euclid(csz) as u8;
        let ly = world_y.rem_euclid(csz) as u8;
        self.components.get(&coord)?.component_at(lx, ly, z)
    }

    /// All distinct component ids that appear at z-slice `z` anywhere in
    /// `chunk`. Used by `ChunkConnectivity::is_reachable` whose API only
    /// has (chunk, z) — no tile coords.
    pub fn components_at_z(&self, chunk: ChunkCoord, z: i8) -> Vec<ComponentId> {
        let mut out: Vec<ComponentId> = Vec::new();
        if let Some(cc) = self.components.get(&chunk) {
            for (&(_, _, cz), &cid) in &cc.at {
                if cz == z && !out.contains(&cid) {
                    out.push(cid);
                }
            }
        }
        out
    }
}

/// 3D flood-fill of every standable foot tile in `chunk` belonging to
/// `coord`. Crosses cells via `NEIGHBOR_DIRS_3D` but never leaves the
/// chunk — cross-chunk connectivity is provided by the border-edge
/// scan downstream. Returns the component classification.
fn classify_components(
    chunk_map: &ChunkMap,
    coord: ChunkCoord,
    chunk: &crate::world::chunk::Chunk,
) -> ChunkComponents {
    let csz = CHUNK_SIZE as i32;

    // Enumerate every standable cell in this chunk.
    let mut seeds: AHashSet<(u8, u8, i8)> = AHashSet::new();
    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let tx = coord.0 * csz + lx as i32;
            let ty = coord.1 * csz + ly as i32;
            let surf_z = chunk.surface_z[ly][lx];
            if chunk_map.passable_at(tx, ty, surf_z as i32) {
                seeds.insert((lx as u8, ly as u8, surf_z));
            }
        }
    }
    for &(lx, ly, z_local) in chunk.deltas.keys() {
        let z = z_local as i32 + Z_MIN;
        if !(z >= i8::MIN as i32 && z <= i8::MAX as i32) {
            continue;
        }
        let tx = coord.0 * csz + lx as i32;
        let ty = coord.1 * csz + ly as i32;
        if chunk_map.passable_at(tx, ty, z) {
            seeds.insert((lx, ly, z as i8));
        }
    }

    let mut at: AHashMap<(u8, u8, i8), ComponentId> = AHashMap::new();
    let mut next_id: u8 = 0;
    let mut queue: VecDeque<(u8, u8, i8)> = VecDeque::new();

    for seed in &seeds {
        if at.contains_key(seed) {
            continue;
        }
        let cid = ComponentId(next_id);
        next_id = next_id.saturating_add(1);
        at.insert(*seed, cid);
        queue.push_back(*seed);

        while let Some((lx, ly, z)) = queue.pop_front() {
            let wx = coord.0 * csz + lx as i32;
            let wy = coord.1 * csz + ly as i32;
            for &(dx, dy, dz) in &NEIGHBOR_DIRS_3D {
                let nx = wx + dx;
                let ny = wy + dy;
                let nz_i32 = z as i32 + dz;
                let n_chunk = ChunkCoord(nx.div_euclid(csz), ny.div_euclid(csz));
                if n_chunk != coord {
                    continue;
                }
                if !(nz_i32 >= i8::MIN as i32 && nz_i32 <= i8::MAX as i32) {
                    continue;
                }
                let nz = nz_i32 as i8;
                let nlx = nx.rem_euclid(csz) as u8;
                let nly = ny.rem_euclid(csz) as u8;
                let key = (nlx, nly, nz);
                if at.contains_key(&key) {
                    continue;
                }
                if !chunk_map.passable_at(nx, ny, nz_i32) {
                    continue;
                }
                // Thin housing walls sever connectivity across the edge they sit
                // on (the door edge stays passable, so interior↔exterior remain
                // one component through the doorway — correct). Cardinal: gate
                // the single shared edge; diagonal: require all four surrounding
                // edges clear, mirroring `passable_diagonal_step`.
                let from = (wx, wy);
                let to = (nx, ny);
                if dx != 0 && dy != 0 {
                    let ca = (nx, wy);
                    let cb = (wx, ny);
                    if chunk_map.edge_blocks_move(from, ca)
                        || chunk_map.edge_blocks_move(ca, to)
                        || chunk_map.edge_blocks_move(from, cb)
                        || chunk_map.edge_blocks_move(cb, to)
                    {
                        continue;
                    }
                } else if (dx != 0) != (dy != 0) && chunk_map.edge_blocks_move(from, to) {
                    continue;
                }
                at.insert(key, cid);
                queue.push_back(key);
            }
        }
    }

    ChunkComponents { at, count: next_id }
}

/// Cardinal-only border directions for cross-chunk edge scanning.
/// `(dx, dy, scan_x_axis, at_max_edge)`.
const BORDER_DIRS: [(i32, i32, bool, bool); 4] = [
    (0, -1, true, false),  // North
    (0, 1, true, true),    // South
    (-1, 0, false, false), // West
    (1, 0, false, true),   // East
];

/// Pending rebuild work accumulated between tasks. `classify` holds chunks
/// whose tile state changed (TileChanged / ChunkLoaded) — they need fresh
/// component classification. `unloaded` holds chunks to drop. Cardinals of
/// these get edge-only rebuilds, computed at task-spawn time. Events that
/// arrive while a task is in flight accumulate here for the next task.
#[derive(Resource, Default)]
pub struct GraphDirty {
    pub classify: AHashSet<ChunkCoord>,
    pub unloaded: AHashSet<ChunkCoord>,
}

/// Snapshot passed to the off-thread rebuild.
///
/// - `chunks` covers two rings around `classify_dirty` so edge scans (which
///   read across one chunk boundary) have all the tile data they need.
/// - `live_components` carries the live `ChunkGraph` classification for every
///   chunk in `chunks` that's NOT in `classify_dirty`, so edge scans don't
///   re-classify (which would produce non-deterministic IDs that mismatch
///   the live graph and break router lookups).
struct RebuildSnapshot {
    chunks: ChunkMap,
    classify_dirty: AHashSet<ChunkCoord>,
    edge_dirty: AHashSet<ChunkCoord>,
    live_components: AHashMap<ChunkCoord, ChunkComponents>,
    unloaded: AHashSet<ChunkCoord>,
}

/// Result merged into `ChunkGraph` on the main thread.
pub struct RebuildResult {
    pub components_delta: AHashMap<ChunkCoord, ChunkComponents>,
    pub edges_delta: AHashMap<ChunkCoord, Vec<ChunkEdge>>,
    pub unloaded: AHashSet<ChunkCoord>,
    pub edge_count: usize,
    pub classify_count: usize,
    pub edge_chunks: usize,
    pub elapsed: std::time::Duration,
}

/// Holds the in-flight rebuild future, if any.
#[derive(Resource, Default)]
pub struct GraphRebuildTask(pub Option<Task<RebuildResult>>);

fn cardinal_neighbors(coord: ChunkCoord) -> [ChunkCoord; 4] {
    [
        ChunkCoord(coord.0, coord.1 - 1),
        ChunkCoord(coord.0, coord.1 + 1),
        ChunkCoord(coord.0 - 1, coord.1),
        ChunkCoord(coord.0 + 1, coord.1),
    ]
}

fn coord_for_tile(tx: i32, ty: i32) -> ChunkCoord {
    let csz = CHUNK_SIZE as i32;
    ChunkCoord(tx.div_euclid(csz), ty.div_euclid(csz))
}

pub fn take_classify_batch(dirty: &mut AHashSet<ChunkCoord>, limit: usize) -> AHashSet<ChunkCoord> {
    let mut coords: Vec<ChunkCoord> = dirty.iter().copied().collect();
    coords.sort_by_key(|c| (c.0, c.1));
    let take = coords.len().min(limit.max(1));
    let mut batch = AHashSet::with_capacity(take);
    for coord in coords.into_iter().take(take) {
        dirty.remove(&coord);
        batch.insert(coord);
    }
    batch
}

/// Drains all three event readers into the `GraphDirty` accumulator. Only
/// chunks whose own tile state changed (or that just loaded) go into
/// `classify`; their cardinal neighbours are derived at task-spawn time.
pub fn enqueue_graph_dirty_system(
    mut dirty: ResMut<GraphDirty>,
    mut tile_changes: EventReader<TileChangedEvent>,
    mut loads: EventReader<ChunkLoadedEvent>,
    mut unloads: EventReader<ChunkUnloadedEvent>,
) {
    for ev in tile_changes.read() {
        dirty
            .classify
            .insert(coord_for_tile(ev.tx as i32, ev.ty as i32));
    }
    for ev in loads.read() {
        dirty.classify.insert(ev.coord);
    }
    for ev in unloads.read() {
        dirty.unloaded.insert(ev.coord);
    }
}

/// Spawns a background rebuild task when there's pending work and no task
/// already in flight. Builds a snapshot covering two rings around the
/// classify-dirty set so cardinals (one ring) can have their edges re-emitted
/// using live IDs, and outer cardinals (two rings) provide tile data for
/// those edge scans.
pub fn spawn_rebuild_task_system(
    chunk_map: Res<ChunkMap>,
    graph: Res<ChunkGraph>,
    mut dirty: ResMut<GraphDirty>,
    mut task: ResMut<GraphRebuildTask>,
    budget: Res<PerfWorkBudget>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    perf.graph_dirty_classify = dirty.classify.len().min(u32::MAX as usize) as u32;
    perf.graph_dirty_unloaded = dirty.unloaded.len().min(u32::MAX as usize) as u32;
    if task.0.is_some() {
        return;
    }
    if dirty.classify.is_empty() && dirty.unloaded.is_empty() {
        return;
    }

    // Drop classify entries for chunks that have since been unloaded.
    dirty.classify.retain(|c| chunk_map.0.contains_key(c));
    let classify_dirty =
        take_classify_batch(&mut dirty.classify, budget.graph_classify_chunks_per_task);
    let unloaded = std::mem::take(&mut dirty.unloaded);
    perf.graph_dirty_classify = dirty.classify.len().min(u32::MAX as usize) as u32;
    perf.graph_dirty_unloaded = 0;
    if classify_dirty.is_empty() && unloaded.is_empty() {
        return;
    }

    // Edge-dirty = classify ∪ cardinals_of(classify ∪ unloaded), restricted
    // to currently-loaded chunks. These need their edges re-emitted so they
    // reflect the latest IDs of classify-dirty neighbours and drop edges to
    // unloaded ones.
    let mut edge_dirty: AHashSet<ChunkCoord> = AHashSet::new();
    for &c in &classify_dirty {
        edge_dirty.insert(c);
        for nb in cardinal_neighbors(c) {
            if chunk_map.0.contains_key(&nb) {
                edge_dirty.insert(nb);
            }
        }
    }
    for &c in &unloaded {
        for nb in cardinal_neighbors(c) {
            if chunk_map.0.contains_key(&nb) {
                edge_dirty.insert(nb);
            }
        }
    }

    // Snapshot tile-data set: edge_dirty ∪ cardinals_of(edge_dirty). Edge
    // scans cross one chunk boundary so we need outer cardinals for tile
    // reads (passable_at on the far side of the border).
    let mut snapshot_coords: AHashSet<ChunkCoord> = AHashSet::new();
    for &c in &edge_dirty {
        snapshot_coords.insert(c);
        for nb in cardinal_neighbors(c) {
            if chunk_map.0.contains_key(&nb) {
                snapshot_coords.insert(nb);
            }
        }
    }

    let mut snap_map = ChunkMap::default();
    let mut live_components: AHashMap<ChunkCoord, ChunkComponents> = AHashMap::new();
    for coord in &snapshot_coords {
        if let Some(chunk) = chunk_map.0.get(coord) {
            snap_map.0.insert(*coord, chunk.clone());
        }
        // Live components for everything in the snapshot that we won't
        // re-classify. Edges from edge-dirty chunks reference these IDs.
        if !classify_dirty.contains(coord) {
            if let Some(cc) = graph.components.get(coord) {
                live_components.insert(*coord, cc.clone());
            }
        }
    }

    let snapshot = RebuildSnapshot {
        chunks: snap_map,
        classify_dirty,
        edge_dirty,
        live_components,
        unloaded,
    };

    let pool = AsyncComputeTaskPool::get();
    task.0 = Some(pool.spawn(async move { rebuild_offthread(snapshot) }));
}

/// Polls the in-flight task; when ready, merges the result into `ChunkGraph`
/// and clears the task slot so the next tick can spawn a new one.
pub fn poll_rebuild_task_system(
    mut task: ResMut<GraphRebuildTask>,
    mut graph: ResMut<ChunkGraph>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    let Some(t) = task.0.as_mut() else {
        return;
    };
    let Some(result) = block_on(future::poll_once(t)) else {
        return; // still running
    };
    task.0 = None;

    let apply_started = Instant::now();
    for coord in &result.unloaded {
        graph.components.remove(coord);
        graph.edges.remove(coord);
    }
    for (coord, cc) in result.components_delta {
        graph.components.insert(coord, cc);
    }
    for (coord, edges) in result.edges_delta {
        graph.edges.insert(coord, edges);
    }
    graph.generation = graph.generation.wrapping_add(1);
    perf.graph_last_classify = result.classify_count.min(u32::MAX as usize) as u32;
    perf.graph_last_edge_chunks = result.edge_chunks.min(u32::MAX as usize) as u32;
    perf.graph_last_edges = result.edge_count.min(u32::MAX as usize) as u32;
    perf.graph_compute_us = micros_u32(result.elapsed);
    perf.graph_apply_us = micros_u32(apply_started.elapsed());

    info!(
        "ChunkGraph rebuilt async: classify={} edge_chunks={} edges={} elapsed={:?}",
        result.classify_count, result.edge_chunks, result.edge_count, result.elapsed,
    );
}

fn rebuild_offthread(snap: RebuildSnapshot) -> RebuildResult {
    let now = Instant::now();
    let chunk_map = &snap.chunks;

    // 1. Classify only chunks whose tile state actually changed. Their IDs
    // may shift; cardinal neighbours' edge re-emission below picks up the
    // new IDs.
    let mut components_delta: AHashMap<ChunkCoord, ChunkComponents> = AHashMap::new();
    for &coord in &snap.classify_dirty {
        if let Some(chunk) = chunk_map.0.get(&coord) {
            components_delta.insert(coord, classify_components(chunk_map, coord, chunk));
        }
    }

    // 2. Re-emit edges for the entire edge_dirty set (classify_dirty ∪
    // cardinals). Self-components come from `components_delta` for
    // classify_dirty chunks and from `live_components` for cardinals; that
    // way cardinals' IDs stay stable across the rebuild.
    let mut edges_delta: AHashMap<ChunkCoord, Vec<ChunkEdge>> = AHashMap::new();
    let mut edge_count = 0usize;

    for &coord in &snap.edge_dirty {
        let Some(chunk) = chunk_map.0.get(&coord) else {
            continue;
        };
        let chunk_edges = scan_edges_for_chunk(
            chunk_map,
            coord,
            chunk,
            &components_delta,
            &snap.live_components,
        );
        edge_count += chunk_edges.len();
        edges_delta.insert(coord, chunk_edges);
    }

    let classify_count = components_delta.len();
    let edge_chunks = edges_delta.len();

    RebuildResult {
        components_delta,
        edges_delta,
        unloaded: snap.unloaded,
        edge_count,
        classify_count,
        edge_chunks,
        elapsed: now.elapsed(),
    }
}

/// Border edge scan for one chunk. `fresh_components` carries the just-
/// classified entries for `classify_dirty` chunks; `live_components`
/// carries the live `ChunkGraph` classification for any chunk in the
/// snapshot that we did NOT re-classify. The combined lookup keeps IDs
/// consistent: edges into a re-classified chunk use its new IDs, and
/// edges into an untouched chunk use the live IDs the rest of the graph
/// already references.
fn scan_edges_for_chunk(
    chunk_map: &ChunkMap,
    coord: ChunkCoord,
    chunk: &Chunk,
    fresh_components: &AHashMap<ChunkCoord, ChunkComponents>,
    live_components: &AHashMap<ChunkCoord, ChunkComponents>,
) -> Vec<ChunkEdge> {
    let mut chunk_edges: Vec<ChunkEdge> = Vec::new();

    // Self-components: prefer fresh (we just re-classified this chunk),
    // fall back to live (this is an edge-only chunk that wasn't re-classified).
    // Last-resort classify is a defensive fallback for the test/Startup full
    // rebuild path; in the runtime async path one of the two maps always hits.
    let self_components = match fresh_components
        .get(&coord)
        .or_else(|| live_components.get(&coord))
    {
        Some(cc) => cc.clone(),
        None => classify_components(chunk_map, coord, chunk),
    };

    // Build a map of (lx, ly) → Vec<z> from this chunk's deltas so we know
    // which underground Z slices to consider for each border tile.
    let mut deltas_by_xy: AHashMap<(u8, u8), Vec<i8>> = AHashMap::new();
    for &(lx, ly, z_local) in chunk.deltas.keys() {
        let z = (z_local as i32 + Z_MIN) as i8;
        deltas_by_xy.entry((lx, ly)).or_default().push(z);
    }

    for (ddx, ddy, scan_x, at_max) in &BORDER_DIRS {
        let nb = ChunkCoord(coord.0 + ddx, coord.1 + ddy);
        let Some(nb_chunk) = chunk_map.0.get(&nb) else {
            continue;
        };
        let nb_components = match fresh_components
            .get(&nb)
            .or_else(|| live_components.get(&nb))
        {
            Some(cc) => cc.clone(),
            None => classify_components(chunk_map, nb, nb_chunk),
        };

        let mut nb_deltas_by_xy: AHashMap<(u8, u8), Vec<i8>> = AHashMap::new();
        for &(lx, ly, z_local) in nb_chunk.deltas.keys() {
            let z = (z_local as i32 + Z_MIN) as i8;
            nb_deltas_by_xy.entry((lx, ly)).or_default().push(z);
        }

        let size = CHUNK_SIZE as i32;
        let edge_idx = if *at_max { size - 1 } else { 0 };
        let nb_edge_idx = if *at_max { 0 } else { size - 1 };

        for i in 0..size {
            let (lx, ly) = if *scan_x {
                (i, edge_idx)
            } else {
                (edge_idx, i)
            };
            let (nb_lx, nb_ly) = if *scan_x {
                (i, nb_edge_idx)
            } else {
                (nb_edge_idx, i)
            };

            let tx = coord.0 * size + lx;
            let ty = coord.1 * size + ly;
            let nb_tx = nb.0 * size + nb_lx;
            let nb_ty = nb.1 * size + nb_ly;

            let surf_z = chunk.surface_z[ly as usize][lx as usize];
            let mut zs: Vec<i8> = vec![surf_z];
            if let Some(extra) = deltas_by_xy.get(&(lx as u8, ly as u8)) {
                for &z in extra {
                    if !zs.contains(&z) {
                        zs.push(z);
                    }
                }
            }
            let nb_surf_z = nb_chunk.surface_z[nb_ly as usize][nb_lx as usize];
            let mut nb_zs: Vec<i8> = vec![nb_surf_z];
            if let Some(extra) = nb_deltas_by_xy.get(&(nb_lx as u8, nb_ly as u8)) {
                for &z in extra {
                    if !nb_zs.contains(&z) {
                        nb_zs.push(z);
                    }
                }
            }

            for &z in &zs {
                if !chunk_map.passable_at(tx, ty, z as i32) {
                    continue;
                }
                let from_component = match self_components.component_at(lx as u8, ly as u8, z) {
                    Some(cid) => cid,
                    None => continue,
                };
                for &nz in &nb_zs {
                    if (nz as i32 - z as i32).abs() > 1 {
                        continue;
                    }
                    if !chunk_map.passable_at(nb_tx, nb_ty, nz as i32) {
                        continue;
                    }
                    // A housing wall sitting on the chunk-border edge severs the
                    // crossing (cardinal border step).
                    if chunk_map.edge_blocks_move((tx, ty), (nb_tx, nb_ty)) {
                        continue;
                    }
                    let to_component =
                        match nb_components.component_at(nb_lx as u8, nb_ly as u8, nz) {
                            Some(cid) => cid,
                            None => continue,
                        };
                    let entry_kind = chunk_map.tile_at(nb_tx, nb_ty, nz as i32).kind;
                    let base = tile_step_cost(entry_kind);
                    let traverse_cost = if base == IMPASSABLE {
                        IMPASSABLE
                    } else {
                        let mut c = base as u32;
                        if (nz as i32 - z as i32).abs() == 1 {
                            c = c.saturating_add(8);
                        }
                        c.min(IMPASSABLE as u32) as u16
                    };
                    chunk_edges.push(ChunkEdge {
                        neighbor: nb,
                        exit_local: (lx as u8, ly as u8),
                        exit_z: z,
                        entry_local: (nb_lx as u8, nb_ly as u8),
                        entry_z: nz,
                        traverse_cost,
                        from_component,
                        to_component,
                    });
                }
            }
        }
    }

    chunk_edges
}

/// Synchronous full-rebuild on the main thread. Used at Startup (after
/// `terrain::spawn_world_system` populates `ChunkMap`) and by tests that
/// populate `ChunkMap` directly without going through chunk streaming.
/// Treats every chunk in `chunk_map` as dirty.
pub fn rebuild_chunk_graph_sync(chunk_map: &ChunkMap, graph: &mut ChunkGraph) {
    graph.components.clear();
    graph.edges.clear();

    let mut components: AHashMap<ChunkCoord, ChunkComponents> = AHashMap::new();
    for (coord, chunk) in &chunk_map.0 {
        components.insert(*coord, classify_components(chunk_map, *coord, chunk));
    }
    let empty: AHashMap<ChunkCoord, ChunkComponents> = AHashMap::new();
    for (coord, chunk) in &chunk_map.0 {
        let edges = scan_edges_for_chunk(chunk_map, *coord, chunk, &components, &empty);
        graph.edges.insert(*coord, edges);
    }
    graph.components = components;
    graph.generation = graph.generation.wrapping_add(1);
}

/// Bevy Startup system wrapper for `rebuild_chunk_graph_sync`. Runs after
/// `terrain::spawn_world_system` so the initial 32×32 spawn area is
/// classified before any agent queries the graph.
pub fn startup_initial_build_system(chunk_map: Res<ChunkMap>, mut graph: ResMut<ChunkGraph>) {
    rebuild_chunk_graph_sync(&chunk_map, &mut graph);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk(surf_z: i8) -> Chunk {
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    #[test]
    fn graph_dirty_batch_splits_and_leaves_backlog() {
        let mut dirty = AHashSet::default();
        for x in (0..20).rev() {
            dirty.insert(ChunkCoord(x, 0));
        }

        let first = take_classify_batch(&mut dirty, 16);
        assert_eq!(first.len(), 16);
        assert_eq!(dirty.len(), 4);
        for x in 0..16 {
            assert!(first.contains(&ChunkCoord(x, 0)));
        }

        let second = take_classify_batch(&mut dirty, 16);
        assert_eq!(second.len(), 4);
        assert!(dirty.is_empty());
        for x in 16..20 {
            assert!(second.contains(&ChunkCoord(x, 0)));
        }
    }

    #[test]
    fn flat_chunk_has_single_component() {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        let chunk = map.0.get(&ChunkCoord(0, 0)).unwrap().clone();
        let cc = classify_components(&map, ChunkCoord(0, 0), &chunk);
        assert_eq!(cc.count, 1);
        // Every (lx, ly) at surface_z=0 should map to component 0.
        for ly in 0..CHUNK_SIZE as u8 {
            for lx in 0..CHUNK_SIZE as u8 {
                assert_eq!(cc.component_at(lx, ly, 0), Some(ComponentId(0)));
            }
        }
    }

    #[test]
    fn dead_end_vertical_shaft_has_separate_component_from_surface() {
        // Same scenario as the old `dead_end_vertical_shaft_does_not_unify_bands`
        // test: dig straight down at (5,5) from z=0 to z=-10. Surface at
        // z=0 elsewhere on the chunk should be one component; the trapped
        // shaft floor at z=-10 should be a different component.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        for floor_z in (-10..=0).rev() {
            map.set_tile(
                5,
                5,
                floor_z + 1,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            map.set_tile(
                5,
                5,
                floor_z,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
        }
        let chunk = map.0.get(&ChunkCoord(0, 0)).unwrap().clone();
        let cc = classify_components(&map, ChunkCoord(0, 0), &chunk);
        // The shaft column re-bottomed surface_z[5][5] to -10; the rest of
        // the chunk still has surface_z=0. Pick a definitely-surface cell.
        let surface_comp = cc.component_at(0, 0, 0).expect("surface classified");
        let shaft_comp = cc.component_at(5, 5, -10).expect("shaft floor classified");
        assert_ne!(
            surface_comp, shaft_comp,
            "trapped vertical shaft must not share the surface component"
        );
    }

    #[test]
    fn diagonal_staircase_unifies_into_single_component() {
        // Stair-step inside a single chunk: surface=0, carve a diagonal
        // staircase from (5,5,0) down to (9,5,-4) so each step is reachable
        // from the previous via a single |Δz|=1, |Δxy|=1 move. All cells
        // should land in the same component.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        for (i, depth) in (0..=4).enumerate() {
            let tx = 5 + i as i32;
            let ty = 5;
            let floor_z = -(depth as i32);
            map.set_tile(
                tx,
                ty,
                floor_z + 1,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            map.set_tile(
                tx,
                ty,
                floor_z,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
        }
        let chunk = map.0.get(&ChunkCoord(0, 0)).unwrap().clone();
        let cc = classify_components(&map, ChunkCoord(0, 0), &chunk);
        let top = cc.component_at(0, 0, 0).expect("surface classified");
        let bottom = cc
            .component_at(9, 5, -4)
            .expect("staircase bottom classified");
        assert_eq!(
            top, bottom,
            "diagonal staircase must unify surface and underground into one component"
        );
    }
}
