use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;
use std::collections::VecDeque;
use std::time::Instant;

use crate::pathfinding::tile_cost::{tile_step_cost, IMPASSABLE};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MIN};

/// All 26 neighbour offsets in (dx, dy, dz). Used by intra-chunk
/// component flood-fill — same set the old `connectivity` module used.
pub const NEIGHBOR_DIRS_3D: [(i32, i32, i32); 26] = [
    // Same Z: 8 horizontal neighbours
    (-1, -1, 0), (-1, 0, 0), (-1, 1, 0),
    (0, -1, 0),              (0, 1, 0),
    (1, -1, 0),  (1, 0, 0),  (1, 1, 0),
    // Z+1
    (-1, -1, 1), (-1, 0, 1), (-1, 1, 1),
    (0, -1, 1),  (0, 0, 1),  (0, 1, 1),
    (1, -1, 1),  (1, 0, 1),  (1, 1, 1),
    // Z-1
    (-1, -1, -1), (-1, 0, -1), (-1, 1, -1),
    (0, -1, -1),  (0, 0, -1),  (0, 1, -1),
    (1, -1, -1),  (1, 0, -1),  (1, 1, -1),
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
                at.insert(key, cid);
                queue.push_back(key);
            }
        }
    }

    ChunkComponents {
        at,
        count: next_id,
    }
}

pub fn build_chunk_graph_system(chunk_map: Res<ChunkMap>, mut graph: ResMut<ChunkGraph>) {
    let now = Instant::now();
    // Cardinal direction offsets and which border row/col to scan
    let borders: [(i32, i32, bool, bool); 4] = [
        // (dx, dy, scan_x_axis, at_max_edge)
        (0, -1, true, false),  // North (top row, ty=0 in this chunk)
        (0, 1, true, true),    // South (bottom row, ty=CHUNK_SIZE-1)
        (-1, 0, false, false), // West  (left col, tx=0)
        (1, 0, false, true),   // East  (right col, tx=CHUNK_SIZE-1)
    ];

    // Drop stale entries for chunks that have been unloaded since the last
    // rebuild. Without this, edges to unloaded neighbors would linger and
    // produce false-positive routes through chunks that no longer exist.
    graph
        .edges
        .retain(|coord, _| chunk_map.0.contains_key(coord));
    graph
        .components
        .retain(|coord, _| chunk_map.0.contains_key(coord));

    // Pass 1 — classify components per chunk via 3D flood-fill. Must run
    // before edge building since edges carry their endpoint components.
    for (coord, chunk) in &chunk_map.0 {
        let cc = classify_components(&chunk_map, *coord, chunk);
        graph.components.insert(*coord, cc);
    }

    let mut edge_count = 0usize;

    // Pass 2 — border scan, now component-aware.
    for (coord, chunk) in &chunk_map.0 {
        let mut chunk_edges: Vec<ChunkEdge> = Vec::new();

        // Build a map of (lx, ly) → Vec<z> from this chunk's deltas so we
        // know which underground Z slices to consider for each border tile.
        let mut deltas_by_xy: AHashMap<(u8, u8), Vec<i8>> = AHashMap::new();
        for &(lx, ly, z_local) in chunk.deltas.keys() {
            let z = (z_local as i32 + Z_MIN) as i8;
            deltas_by_xy.entry((lx, ly)).or_default().push(z);
        }

        for (ddx, ddy, scan_x, at_max) in &borders {
            let nb = ChunkCoord(coord.0 + ddx, coord.1 + ddy);
            let Some(nb_chunk) = chunk_map.0.get(&nb) else {
                continue;
            };

            // Gather neighbor's deltas-by-xy on its border too.
            let mut nb_deltas_by_xy: AHashMap<(u8, u8), Vec<i8>> = AHashMap::new();
            for &(lx, ly, z_local) in nb_chunk.deltas.keys() {
                let z = (z_local as i32 + Z_MIN) as i8;
                nb_deltas_by_xy.entry((lx, ly)).or_default().push(z);
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

                // Candidate Z slices: this side's surface + carved deltas at
                // this border tile (and same for neighbor).
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
                    let from_component =
                        match graph.components.get(coord).and_then(|cc| {
                            cc.component_at(lx as u8, ly as u8, z)
                        }) {
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
                        let to_component = match graph.components.get(&nb).and_then(|cc| {
                            cc.component_at(nb_lx as u8, nb_ly as u8, nz)
                        }) {
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
                        edge_count += 1;
                    }
                }
            }
        }

        graph.edges.insert(*coord, chunk_edges);
    }

    graph.generation = graph.generation.wrapping_add(1);

    info!(
        "ChunkGraph built: {} edges, {} chunks classified in {:?}",
        edge_count,
        graph.components.len(),
        now.elapsed()
    );
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
        let bottom = cc.component_at(9, 5, -4).expect("staircase bottom classified");
        assert_eq!(
            top, bottom,
            "diagonal staircase must unify surface and underground into one component"
        );
    }
}
