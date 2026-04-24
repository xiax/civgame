use ahash::AHashMap;
use bevy::prelude::*;
use std::collections::VecDeque;

use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};

#[derive(Clone)]
pub struct ChunkEdge {
    pub neighbor:    ChunkCoord,
    /// Tile in this chunk that borders the neighbor (local coords 0..CHUNK_SIZE-1).
    pub exit_local:  (u8, u8),
    /// Corresponding tile in the neighbor chunk.
    pub entry_local: (u8, u8),
}

#[derive(Resource, Default)]
pub struct ChunkGraph {
    pub edges: AHashMap<ChunkCoord, Vec<ChunkEdge>>,
}

impl ChunkGraph {
    /// BFS from `cur` to `dest`; returns the global tile coord of the first
    /// border crossing (the exit tile in `cur`'s chunk), or `None` if no route.
    pub fn next_waypoint(
        &self,
        cur: ChunkCoord,
        dest: ChunkCoord,
        _chunk_map: &ChunkMap,
    ) -> Option<(i16, i16)> {
        if cur == dest {
            return None;
        }

        // BFS over chunk graph; record first-step edge from `cur`.
        let mut visited = AHashMap::new();
        let mut queue: VecDeque<ChunkCoord> = VecDeque::new();
        visited.insert(cur, cur);
        queue.push_back(cur);

        while let Some(node) = queue.pop_front() {
            if let Some(edges) = self.edges.get(&node) {
                for edge in edges {
                    let nb = edge.neighbor;
                    if visited.contains_key(&nb) {
                        continue;
                    }
                    visited.insert(nb, node);
                    if nb == dest {
                        // Trace back to the edge that leaves `cur`
                        let first_step = trace_first_step(&visited, cur, dest);
                        if let Some(edges_from_cur) = self.edges.get(&cur) {
                            let candidates: Vec<_> = edges_from_cur.iter()
                                .filter(|e| e.neighbor == first_step)
                                .collect();
                            
                            if !candidates.is_empty() {
                                // Pick a random edge to avoid clustering all agents on the same tile
                                let e = candidates[fastrand::usize(..candidates.len())];
                                let gx = e.neighbor.0 * CHUNK_SIZE as i32 + e.entry_local.0 as i32;
                                let gy = e.neighbor.1 * CHUNK_SIZE as i32 + e.entry_local.1 as i32;
                                return Some((gx as i16, gy as i16));
                            }
                        }
                        return None;
                    }
                    queue.push_back(nb);
                }
            }
        }
        None
    }
}

fn trace_first_step(
    parent: &AHashMap<ChunkCoord, ChunkCoord>,
    start: ChunkCoord,
    dest: ChunkCoord,
) -> ChunkCoord {
    let mut cur = dest;
    loop {
        let p = parent[&cur];
        if p == start {
            return cur;
        }
        cur = p;
    }
}

pub fn build_chunk_graph_system(
    chunk_map: Res<ChunkMap>,
    mut graph: ResMut<ChunkGraph>,
) {
    // Cardinal direction offsets and which border row/col to scan
    let borders: [(i32, i32, bool, bool); 4] = [
        // (dx, dy, scan_x_axis, at_max_edge)
        (0, -1, true,  false), // North (top row, ty=0 in this chunk)
        (0,  1, true,  true),  // South (bottom row, ty=CHUNK_SIZE-1)
        (-1, 0, false, false), // West  (left col, tx=0)
        (1,  0, false, true),  // East  (right col, tx=CHUNK_SIZE-1)
    ];

    let mut edge_count = 0usize;

    for (coord, _) in &chunk_map.0 {
        let mut chunk_edges: Vec<ChunkEdge> = Vec::new();

        for (ddx, ddy, scan_x, at_max) in &borders {
            let nb = ChunkCoord(coord.0 + ddx, coord.1 + ddy);
            if !chunk_map.0.contains_key(&nb) {
                continue;
            }

            let size = CHUNK_SIZE as i32;
            let edge_idx = if *at_max { size - 1 } else { 0 };
            // Neighbor's border is the opposite edge
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

                if chunk_map.is_passable(tx, ty) && chunk_map.is_passable(nb_tx, nb_ty) {
                    chunk_edges.push(ChunkEdge {
                        neighbor:    nb,
                        exit_local:  (lx as u8, ly as u8),
                        entry_local: (nb_lx as u8, nb_ly as u8),
                    });
                    edge_count += 1;
                }
            }
        }

        graph.edges.insert(*coord, chunk_edges);
    }

    info!("ChunkGraph built: {} edges", edge_count);
}
