use crate::simulation::construction::DoorMap;
use crate::world::chunk::ChunkMap;

/// 3D voxel LOS from `from` to `to` (each is `(tx, ty, foot_z)`).
///
/// Walks an integer voxel grid using 2D Bresenham on (x, y) with linearly
/// interpolated z. At each non-endpoint voxel, queries `chunk_map.tile_at`
/// and rejects if the tile is opaque (`Wall`, or a closed `Door`). Open doors
/// are transparent. Underground rock reads as `Wall` via the deltas-only
/// `tile_at`, so an unbroken stretch of solid earth correctly blocks sight.
pub fn has_los(
    chunk_map: &ChunkMap,
    door_map: &DoorMap,
    from: (i32, i32, i8),
    to: (i32, i32, i8),
) -> bool {
    let (mut x0, mut y0) = (from.0, from.1);
    let (x1, y1) = (to.0, to.1);
    let from_z = from.2 as f32;
    let to_z = to.2 as f32;

    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;

    let total_dist = (dx.max(dy)).max(1) as f32;
    let mut steps = 0i32;

    loop {
        steps += 1;

        // Skip endpoint voxels (caller's tiles); only test voxels in between.
        if (x0, y0) != (from.0, from.1) && (x0, y0) != (to.0, to.1) {
            let t = steps as f32 / total_dist;
            let ray_z = (from_z + t * (to_z - from_z)).round() as i32;
            if chunk_map.tile_at(x0, y0, ray_z).kind.is_opaque() {
                return false;
            }
            // Closed door blocks LOS even though the underlying tile is passable.
            if let Some(door) = door_map.0.get(&(x0 as i32, y0 as i32)) {
                if !door.open {
                    return false;
                }
            }
        }

        if x0 == x1 && y0 == y1 {
            break;
        }

        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x0 += sx;
        }
        if e2 < dx {
            err += dx;
            y0 += sy;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk_map() -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    #[test]
    fn same_tile_has_los() {
        let m = flat_chunk_map();
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 0, 0), (0, 0, 0)));
    }

    #[test]
    fn flat_terrain_always_has_los() {
        let m = flat_chunk_map();
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 0, 0), (10, 5, 0)));
    }

    #[test]
    fn underground_tunnel_has_los_along_its_length() {
        // Hill column surface=5 across the chunk (set by flat_chunk_map at 0,
        // so reuse and lift surface). Build a fresh chunk with surface=5.
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[5i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Stone; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        // Carve a tunnel at z=-4 along y=10, x=0..10.
        for x in 0..10i32 {
            m.set_tile(
                x,
                10,
                -3,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            m.set_tile(
                x,
                10,
                -4,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
        }
        // Two agents inside the tunnel can see each other.
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 10, -4), (9, 10, -4)));
    }

    #[test]
    fn solid_rock_between_underground_agents_blocks_los() {
        // Two agents at z=-4 in a chunk that has NO carving — every voxel
        // between them is Wall, so LOS is blocked.
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[5i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Stone; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        let d = DoorMap::default();
        assert!(!has_los(&m, &d, (0, 0, -4), (9, 0, -4)));
    }

    #[test]
    fn surface_to_underground_blocked_by_overhead_rock() {
        // Surface agent at z=0 trying to see underground agent at z=-4
        // through unbroken rock — blocked.
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        let d = DoorMap::default();
        assert!(!has_los(&m, &d, (0, 0, 0), (10, 0, -4)));
    }
}
