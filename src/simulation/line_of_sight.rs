use crate::world::chunk::ChunkMap;

/// 2D Bresenham ray from `from` to `to`.
///
/// Blocked when any intermediate tile's surface Z exceeds the linearly-interpolated
/// ray height between from_z and to_z by more than 0.5 tiles (i.e. a wall or hill
/// that sticks up above the sight line).
///
/// Entities at the same Z can always see each other horizontally;
/// entities at different Z still have LOS unless terrain between them is higher
/// than the straight-line path.
pub fn has_los(chunk_map: &ChunkMap, from: (i32, i32), to: (i32, i32)) -> bool {
    use crate::world::chunk::Z_MIN;

    let from_z = chunk_map.surface_z_at(from.0, from.1);
    let to_z = chunk_map.surface_z_at(to.0, to.1);
    if from_z < Z_MIN || to_z < Z_MIN {
        return false; // one end in an unloaded chunk
    }

    let (mut x0, mut y0) = (from.0, from.1);
    let (x1, y1) = (to.0, to.1);

    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;

    let total_dist = (dx.max(dy)).max(1) as f32;
    let mut steps = 0i32;

    loop {
        steps += 1;

        // Check the current intermediate tile (skip start and end tiles).
        if (x0, y0) != from && (x0, y0) != to {
            let tile_z = chunk_map.surface_z_at(x0, y0);
            if tile_z >= Z_MIN {
                let t = steps as f32 / total_dist;
                let ray_z = from_z as f32 + t * (to_z - from_z) as f32;
                if tile_z as f32 > ray_z + 0.5 {
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
    use ahash::AHashMap;

    fn flat_chunk_map() -> ChunkMap {
        use crate::world::tile::TileKind;
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
        assert!(has_los(&m, (0, 0), (0, 0)));
    }

    #[test]
    fn flat_terrain_always_has_los() {
        let m = flat_chunk_map();
        assert!(has_los(&m, (0, 0), (10, 5)));
    }
}
