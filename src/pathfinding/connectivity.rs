use ahash::AHashMap;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::world::chunk::ChunkCoord;

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
    let (lo, hi) = if rank[ra] < rank[rb] { (ra, rb) } else { (rb, ra) };
    parent[lo] = hi;
    if rank[lo] == rank[hi] {
        rank[hi] = rank[hi].saturating_add(1);
    }
}

pub fn rebuild_connectivity_system(graph: Res<ChunkGraph>, mut conn: ResMut<ChunkConnectivity>) {
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

    for (coord, edges) in &graph.edges {
        intern((*coord, 0), &mut nodes, &mut idx);
        for edge in edges {
            intern((*coord, z_band(edge.exit_z)), &mut nodes, &mut idx);
            intern((edge.neighbor, z_band(edge.entry_z)), &mut nodes, &mut idx);
        }
    }

    let n = nodes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut rank = vec![0u8; n];

    for (coord, edges) in &graph.edges {
        for edge in edges {
            let from = idx[&(*coord, z_band(edge.exit_z))];
            let to = idx[&(edge.neighbor, z_band(edge.entry_z))];
            uf_union(&mut parent, &mut rank, from, to);
        }
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

    conn.component = component;
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
        let mut conn = ChunkConnectivity::default();
        let mut nodes: Vec<(ChunkCoord, ZBand)> = Vec::new();
        let mut idx: AHashMap<(ChunkCoord, ZBand), usize> = AHashMap::new();
        for (coord, edges) in &graph.edges {
            if !idx.contains_key(&(*coord, 0)) {
                idx.insert((*coord, 0), nodes.len());
                nodes.push((*coord, 0));
            }
            for edge in edges {
                let k = (*coord, z_band(edge.exit_z));
                if !idx.contains_key(&k) {
                    idx.insert(k, nodes.len());
                    nodes.push(k);
                }
                let k2 = (edge.neighbor, z_band(edge.entry_z));
                if !idx.contains_key(&k2) {
                    idx.insert(k2, nodes.len());
                    nodes.push(k2);
                }
            }
        }
        let n = nodes.len();
        let mut parent: Vec<usize> = (0..n).collect();
        let mut rank = vec![0u8; n];
        for (coord, edges) in &graph.edges {
            for edge in edges {
                let from = idx[&(*coord, z_band(edge.exit_z))];
                let to = idx[&(edge.neighbor, z_band(edge.entry_z))];
                uf_union(&mut parent, &mut rank, from, to);
            }
        }
        let mut component = AHashMap::new();
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
        conn.component = component;
        conn
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
    fn z_band_buckets() {
        assert_eq!(z_band(0), 0);
        assert_eq!(z_band(3), 0);
        assert_eq!(z_band(4), 1);
        assert_eq!(z_band(-1), -1);
        assert_eq!(z_band(-4), -1);
        assert_eq!(z_band(-5), -2);
    }
}
