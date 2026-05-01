use ahash::AHashMap;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MIN};

pub type ZBand = i8;

pub fn z_band(z: i8) -> ZBand {
    (z as i32).div_euclid(4) as i8
}

#[derive(Resource, Default)]
pub struct ChunkConnectivity {
    component: AHashMap<(ChunkCoord, ZBand), u32>,
    pub generation: u32,
}

impl ChunkConnectivity {
    pub fn is_reachable(&self, from: (ChunkCoord, i8), to: (ChunkCoord, i8)) -> bool {
        if from.0 == to.0 && z_band(from.1) == z_band(to.1) {
            return true;
        }
        let a = self.component.get(&(from.0, z_band(from.1)));
        let b = self.component.get(&(to.0, z_band(to.1)));
        matches!((a, b), (Some(x), Some(y)) if x == y)
    }

    pub fn component_count(&self) -> usize {
        let mut ids: ahash::AHashSet<u32> = ahash::AHashSet::new();
        for &id in self.component.values() {
            ids.insert(id);
        }
        ids.len()
    }

    pub fn node_count(&self) -> usize {
        self.component.len()
    }

    /// Returns the component id for `(chunk, z)`, or `None` if the chunk
    /// graph hasn't recorded that band yet.
    pub fn component_of(&self, chunk: ChunkCoord, z: i8) -> Option<u32> {
        self.component.get(&(chunk, z_band(z))).copied()
    }

    /// Iterate over every `(chunk, z_band, component_id)` tuple. Used by
    /// the connectivity-component debug overlay.
    pub fn iter(&self) -> impl Iterator<Item = (ChunkCoord, ZBand, u32)> + '_ {
        self.component.iter().map(|(&(c, b), &id)| (c, b, id))
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

/// All 26 neighbor offsets in (dx, dy, dz). Used to enumerate candidate
/// 3D-passable steps when scanning intra-chunk vertical connectivity.
const NEIGHBOR_DIRS_3D: [(i32, i32, i32); 26] = [
    // Same Z: 8 horizontal neighbors
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

/// Pure builder usable from tests. Produces the (chunk, z_band) → component-id
/// map. Cross-chunk edges come from `graph`; within-chunk vertical/diagonal
/// passability comes from scanning every chunk's `deltas` against `chunk_map`.
pub fn build_connectivity_components(
    graph: &ChunkGraph,
    chunk_map: &ChunkMap,
) -> AHashMap<(ChunkCoord, ZBand), u32> {
    let mut nodes: Vec<(ChunkCoord, ZBand)> = Vec::new();
    let mut idx: AHashMap<(ChunkCoord, ZBand), usize> = AHashMap::new();

    let mut intern = |key: (ChunkCoord, ZBand),
                      nodes: &mut Vec<(ChunkCoord, ZBand)>,
                      idx: &mut AHashMap<(ChunkCoord, ZBand), usize>|
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

    // Pass 1a — intern all nodes from cross-chunk edges (chunk-graph border scan).
    for (coord, edges) in &graph.edges {
        for edge in edges {
            intern((*coord, z_band(edge.exit_z)), &mut nodes, &mut idx);
            intern((edge.neighbor, z_band(edge.entry_z)), &mut nodes, &mut idx);
        }
    }

    // Pass 1b — intra-chunk passability. The chunk-graph only enumerates
    // cross-border edges, so a vertical shaft entirely inside one chunk
    // never registers and `is_reachable` would falsely reject it. Scan every
    // delta-touched cell, find passable foot tiles, and queue unions for
    // bands connected by a |Δ|≤1 step that stays in the same chunk.
    let mut intra_unions: Vec<(usize, usize)> = Vec::new();
    let csz = CHUNK_SIZE as i32;
    for (coord, chunk) in &chunk_map.0 {
        if chunk.deltas.is_empty() {
            continue;
        }
        for &(lx, ly, z_local) in chunk.deltas.keys() {
            let z = z_local as i32 + Z_MIN;
            let world_x = coord.0 * csz + lx as i32;
            let world_y = coord.1 * csz + ly as i32;
            if !chunk_map.passable_at(world_x, world_y, z) {
                continue;
            }
            let band_a = z_band(z as i8);
            for &(dx, dy, dz) in &NEIGHBOR_DIRS_3D {
                let nx = world_x + dx;
                let ny = world_y + dy;
                let nz = z + dz;
                let n_chunk = ChunkCoord(nx.div_euclid(csz), ny.div_euclid(csz));
                // Cross-chunk neighbors are already covered by the chunk-graph
                // edge scan; only union *within* this chunk.
                if n_chunk != *coord {
                    continue;
                }
                if !chunk_map.passable_at(nx, ny, nz) {
                    continue;
                }
                let band_b = z_band(nz as i8);
                if band_a == band_b {
                    continue;
                }
                let a = intern((*coord, band_a), &mut nodes, &mut idx);
                let b = intern((*coord, band_b), &mut nodes, &mut idx);
                intra_unions.push((a, b));
            }
        }
    }

    let n = nodes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut rank = vec![0u8; n];

    // Pass 2a — cross-chunk unions.
    for (coord, edges) in &graph.edges {
        for edge in edges {
            let from = idx[&(*coord, z_band(edge.exit_z))];
            let to = idx[&(edge.neighbor, z_band(edge.entry_z))];
            uf_union(&mut parent, &mut rank, from, to);
        }
    }
    // Pass 2b — intra-chunk unions.
    for (a, b) in intra_unions {
        uf_union(&mut parent, &mut rank, a, b);
    }

    let mut component = AHashMap::with_capacity(n);
    let mut root_to_id: AHashMap<usize, u32> = AHashMap::new();
    let mut next_id: u32 = 0;
    for i in 0..n {
        let root = uf_find(&mut parent, i);
        let id = *root_to_id.entry(root).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        component.insert(nodes[i], id);
    }

    component
}

pub fn rebuild_connectivity_system(
    graph: Res<ChunkGraph>,
    chunk_map: Res<ChunkMap>,
    mut conn: ResMut<ChunkConnectivity>,
) {
    conn.component = build_connectivity_components(&graph, &chunk_map);
    conn.generation = conn.generation.wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::{ChunkEdge, ChunkGraph};

    fn edge(neighbor: ChunkCoord, exit_z: i8, entry_z: i8) -> ChunkEdge {
        ChunkEdge {
            neighbor,
            exit_local: (0, 0),
            exit_z,
            entry_local: (0, 0),
            entry_z,
            traverse_cost: 100,
        }
    }

    fn run(graph: ChunkGraph) -> ChunkConnectivity {
        run_with_map(graph, ChunkMap::default())
    }

    fn run_with_map(graph: ChunkGraph, chunk_map: ChunkMap) -> ChunkConnectivity {
        ChunkConnectivity {
            component: build_connectivity_components(&graph, &chunk_map),
            generation: 0,
        }
    }

    #[test]
    fn two_connected_chunks_share_component() {
        let mut graph = ChunkGraph::default();
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0)]);
        let conn = run(graph);
        assert!(conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), 0)));
    }

    #[test]
    fn isolated_chunks_are_not_reachable() {
        let mut graph = ChunkGraph::default();
        graph.edges.insert(ChunkCoord(0, 0), vec![]);
        graph.edges.insert(ChunkCoord(5, 5), vec![]);
        let conn = run(graph);
        assert!(!conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(5, 5), 0)));
    }

    #[test]
    fn surface_and_underground_bands_separate_when_no_ramp() {
        let mut graph = ChunkGraph::default();
        // Only a surface edge exists; underground band should be its own component.
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, 0)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 0, 0)]);
        let conn = run(graph);
        assert!(conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), 0)));
        // Underground band -2 → z_band(-8) = -2; not present in graph at all.
        assert!(!conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), -8)));
    }

    #[test]
    fn ramp_edge_unifies_bands_across_chunks() {
        // exit at z=0 (band 0), entry at z=-1 (band -1) → bands unified.
        let mut graph = ChunkGraph::default();
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 0, -1)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), -1, 0)]);
        let conn = run(graph);
        assert!(conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(1, 0), -1)));
    }

    #[test]
    fn chunk_with_no_zero_band_edges_does_not_create_orphan_zero_node() {
        // Chunk has edges only at z_band 1 (z=4..7). The connectivity graph
        // must not invent an orphan (coord, 0) node — that would inflate the
        // component count and produce spurious Unreachable rejections for
        // agents not at z_band 0.
        let mut graph = ChunkGraph::default();
        graph
            .edges
            .insert(ChunkCoord(0, 0), vec![edge(ChunkCoord(1, 0), 4, 4)]);
        graph
            .edges
            .insert(ChunkCoord(1, 0), vec![edge(ChunkCoord(0, 0), 4, 4)]);
        let conn = run(graph);
        assert_eq!(conn.component_of(ChunkCoord(0, 0), 0), None);
        assert_eq!(conn.component_of(ChunkCoord(1, 0), 0), None);
        assert!(conn.is_reachable((ChunkCoord(0, 0), 4), (ChunkCoord(1, 0), 4)));
    }

    #[test]
    fn z_band_buckets() {
        assert_eq!(z_band(0), 0);
        assert_eq!(z_band(3), 0);
        assert_eq!(z_band(4), 1);
        assert_eq!(z_band(-1), -1);
        assert_eq!(z_band(-4), -1);
        assert_eq!(z_band(-5), -2);
    }

    use crate::world::chunk::{Chunk, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk(surf_z: i8) -> Chunk {
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    #[test]
    fn intra_chunk_diagonal_descent_unifies_bands() {
        // Stair-step inside a single chunk: surface=0 at (5,5), carve a
        // diagonal staircase down to z=-4 at (9,5) so passable foot tiles
        // exist at z=0,-1,-2,-3,-4 across consecutive XY columns. With the
        // intra-chunk pass the surface band (0) and the underground band
        // (-1) should be unified for chunk (0,0) even though no chunk-graph
        // edges exist (no neighbor chunks loaded).
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));

        // Carve five descending floors. Each step lowers the foot Z by 1
        // in an adjacent column. The surface_z update inside set_delta will
        // propagate.
        for (i, z) in (0..=4).enumerate() {
            let tx = 5 + i as i32;
            let ty = 5;
            let floor_z = -(z as i32);
            // Headspace at floor_z + 1
            map.set_tile(
                tx,
                ty,
                floor_z + 1,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            // Floor at floor_z
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
        // Sanity: foot tiles at z=0 (surface step) and z=-4 (deepest step).
        assert!(map.passable_at(5, 5, 0));
        assert!(map.passable_at(9, 5, -4));

        // Empty chunk graph (no neighbors loaded) — the only connectivity
        // signal must come from the intra-chunk pass.
        let conn = run_with_map(ChunkGraph::default(), map);
        assert!(
            conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(0, 0), -1)),
            "diagonal staircase within one chunk must unify surface and underground bands",
        );
    }

    #[test]
    fn intra_chunk_pass_does_not_leak_to_other_chunks() {
        // Carving a staircase in chunk (0,0) must not cause chunk (5,5)
        // to share its component (those chunks have no border edges).
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        map.0.insert(ChunkCoord(5, 5), flat_chunk(0));
        for (i, z) in (0..=4).enumerate() {
            let tx = 5 + i as i32;
            let ty = 5;
            let floor_z = -(z as i32);
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
        let conn = run_with_map(ChunkGraph::default(), map);
        // Within (0,0), bands unified.
        assert!(conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(0, 0), -1)));
        // Across chunks (no graph edges), still unreachable.
        assert!(!conn.is_reachable((ChunkCoord(0, 0), -1), (ChunkCoord(5, 5), 0)));
    }

    #[test]
    fn dead_end_vertical_shaft_does_not_unify_bands() {
        // The "agent dug straight down" scenario: a single XY column with
        // air-filled headspace and Dirt floor at -10. There's no horizontally
        // adjacent passable foot tile underground, so |Δz| = 1 vertical step
        // alone (Air→Dirt vertical) doesn't enable climbing — the column
        // really is a dead end and bands must NOT unify.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));

        // Surface at z=0 starts as Grass. Dig straight down at (5,5) until
        // surface_z = -10 (mirrors dig_system carving floor below itself).
        for floor_z in (-10..=0).rev() {
            // Carve headspace at floor_z + 1 (Air) and floor at floor_z (Dirt).
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
        // Agent foot at z=-10 standable; surface elsewhere on chunk still at z=0.
        assert!(map.passable_at(5, 5, -10));
        assert!(map.passable_at(4, 5, 0));

        let conn = run_with_map(ChunkGraph::default(), map);
        // The dead-end shaft should NOT unify bands — the agent really is
        // trapped at z=-10 (no horizontal neighbor underground is passable).
        // band(-10) = z_band(-10) = -3; band(0) = 0.
        assert!(!conn.is_reachable((ChunkCoord(0, 0), 0), (ChunkCoord(0, 0), -10)));
    }
}
