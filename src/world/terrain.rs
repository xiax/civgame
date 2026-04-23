use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};

use super::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
use super::globe::WorldCell;
use super::tile::{TileData, TileKind};

pub const WORLD_CHUNKS_X: i32 = 16;
pub const WORLD_CHUNKS_Y: i32 = 16;
pub const TILE_SIZE: f32 = 16.0;

pub fn generate_chunk_from_globe(coord: ChunkCoord, globe_cell: &WorldCell, perlin: &Perlin) -> Chunk {
    use super::globe::Biome;
    // Adjust tile thresholds per biome so local terrain matches world-map appearance
    let (water_t, grass_t, farm_t, forest_t) = match globe_cell.biome {
        Biome::Ocean     => (0.90, 0.95, 0.97, 0.99),
        Biome::Tundra    => (0.15, 0.80, 0.85, 0.90),
        Biome::Taiga     => (0.18, 0.35, 0.40, 0.70),
        Biome::Temperate => (0.22, 0.45, 0.60, 0.75),
        Biome::Grassland => (0.18, 0.60, 0.75, 0.82),
        Biome::Tropical  => (0.20, 0.30, 0.35, 0.78),
        Biome::Desert    => (0.10, 0.65, 0.68, 0.70),
        Biome::Mountain  => (0.12, 0.25, 0.28, 0.40),
    };
    generate_chunk_with_thresholds(coord, perlin, water_t, grass_t, farm_t, forest_t)
}

fn generate_chunk(coord: ChunkCoord, perlin: &Perlin) -> Chunk {
    generate_chunk_with_thresholds(coord, perlin, 0.22, 0.45, 0.60, 0.75)
}

fn generate_chunk_with_thresholds(
    coord: ChunkCoord,
    perlin: &Perlin,
    water_t: f32, grass_t: f32, farm_t: f32, forest_t: f32,
) -> Chunk {
    let mut tiles = Box::new([TileData::default(); CHUNK_SIZE * CHUNK_SIZE]);

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let world_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let world_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;

            let nx = world_tx as f64 * 0.04;
            let ny = world_ty as f64 * 0.04;

            let v = perlin.get([nx, ny]) * 0.6
                + perlin.get([nx * 2.0, ny * 2.0]) * 0.3
                + perlin.get([nx * 4.0, ny * 4.0]) * 0.1;
            let v = ((v + 1.0) * 0.5) as f32;

            let kind = if v < water_t  { TileKind::Water }
                  else if v < grass_t  { TileKind::Grass }
                  else if v < farm_t   { TileKind::Farmland }
                  else if v < forest_t { TileKind::Forest }
                  else                 { TileKind::Stone };

            let elevation = (v * 255.0) as u8;
            let fertility = if matches!(kind, TileKind::Farmland | TileKind::Grass) {
                ((1.0 - (v - 0.45).abs() * 5.0).max(0.0) * 255.0) as u8
            } else {
                0
            };

            tiles[ty * CHUNK_SIZE + tx] = TileData { kind, elevation, fertility, flags: 0 };
        }
    }

    Chunk::new(tiles)
}

pub fn spawn_world_system(mut chunk_map: ResMut<ChunkMap>) {
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};

    let perlin = Perlin::default().set_seed(42);

    // Generate the initial area: WORLD_CHUNKS_X×WORLD_CHUNKS_Y chunks
    // centered on the globe center so agents and population systems have tiles to work with.
    let start_cx = (GLOBE_WIDTH  / 2) * GLOBE_CELL_CHUNKS;
    let start_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;

    for dy in 0..WORLD_CHUNKS_Y {
        for dx in 0..WORLD_CHUNKS_X {
            let coord = ChunkCoord(start_cx + dx, start_cy + dy);
            let chunk = generate_chunk(coord, &perlin);
            chunk_map.0.insert(coord, chunk);
        }
    }

    info!(
        "Initial area generated: {}x{} chunks at globe center ({},{})",
        WORLD_CHUNKS_X, WORLD_CHUNKS_Y, start_cx, start_cy
    );
}

/// Convert tile coordinates to world-space pixel position (center of tile).
pub fn tile_to_world(tile_x: i32, tile_y: i32) -> Vec2 {
    Vec2::new(
        tile_x as f32 * TILE_SIZE + TILE_SIZE * 0.5,
        tile_y as f32 * TILE_SIZE + TILE_SIZE * 0.5,
    )
}

/// Convert world-space position to tile coordinates.
pub fn world_to_tile(pos: Vec2) -> (i32, i32) {
    (
        (pos.x / TILE_SIZE).floor() as i32,
        (pos.y / TILE_SIZE).floor() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_chunk_no_panic() {
        let perlin = Perlin::default().set_seed(1);
        let chunk = generate_chunk(ChunkCoord(0, 0), &perlin);
        assert_eq!(chunk.tiles.len(), CHUNK_SIZE * CHUNK_SIZE);
    }

    #[test]
    fn tile_world_roundtrip() {
        let pos = tile_to_world(5, 7);
        let (tx, ty) = world_to_tile(pos);
        assert_eq!((tx, ty), (5, 7));
    }
}
