use bevy::prelude::*;
use ahash::AHashMap;
use super::tile::TileData;

pub const CHUNK_SIZE: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ChunkCoord(pub i32, pub i32);

impl ChunkCoord {
    pub fn from_world(world_x: f32, world_y: f32, tile_size: f32) -> Self {
        let tile_x = (world_x / tile_size).floor() as i32;
        let tile_y = (world_y / tile_size).floor() as i32;
        ChunkCoord(
            tile_x.div_euclid(CHUNK_SIZE as i32),
            tile_y.div_euclid(CHUNK_SIZE as i32),
        )
    }

    pub fn chebyshev_dist(self, other: ChunkCoord) -> i32 {
        (self.0 - other.0).abs().max((self.1 - other.1).abs())
    }
}

/// Aggregate stats for chunks simulated at LOD::Aggregate level.
#[derive(Default, Clone, Copy)]
pub struct ChunkAggregate {
    pub pop_count:      u32,
    pub avg_hunger:     u8,
    pub avg_mood:       i8,
    pub food_produced:  f32,
    pub food_consumed:  f32,
    pub employed_count: u32,
}

pub struct Chunk {
    pub tiles:     Box<[TileData; CHUNK_SIZE * CHUNK_SIZE]>,
    pub entities:  Vec<Entity>,
    pub aggregate: ChunkAggregate,
    pub is_active: bool,
}

impl Chunk {
    pub fn new(tiles: Box<[TileData; CHUNK_SIZE * CHUNK_SIZE]>) -> Self {
        Self {
            tiles,
            entities: Vec::new(),
            aggregate: ChunkAggregate::default(),
            is_active: true,
        }
    }

    pub fn tile(&self, local_x: usize, local_y: usize) -> TileData {
        self.tiles[local_y * CHUNK_SIZE + local_x]
    }

    pub fn tile_mut(&mut self, local_x: usize, local_y: usize) -> &mut TileData {
        &mut self.tiles[local_y * CHUNK_SIZE + local_x]
    }
}

#[derive(Resource, Default)]
pub struct ChunkMap(pub AHashMap<ChunkCoord, Chunk>);

impl ChunkMap {
    pub fn tile_at(&self, tile_x: i32, tile_y: i32) -> Option<TileData> {
        let coord = ChunkCoord(
            tile_x.div_euclid(CHUNK_SIZE as i32),
            tile_y.div_euclid(CHUNK_SIZE as i32),
        );
        let lx = tile_x.rem_euclid(CHUNK_SIZE as i32) as usize;
        let ly = tile_y.rem_euclid(CHUNK_SIZE as i32) as usize;
        self.0.get(&coord).map(|c| c.tile(lx, ly))
    }

    pub fn is_passable(&self, tile_x: i32, tile_y: i32) -> bool {
        self.tile_at(tile_x, tile_y)
            .map(|t| t.is_passable())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::tile::TileKind;

    fn make_chunk(kind: TileKind) -> Chunk {
        let tiles = Box::new([TileData { kind, ..Default::default() }; CHUNK_SIZE * CHUNK_SIZE]);
        Chunk::new(tiles)
    }

    #[test]
    fn chunk_coord_from_world() {
        let coord = ChunkCoord::from_world(0.0, 0.0, 16.0);
        assert_eq!(coord, ChunkCoord(0, 0));

        let coord = ChunkCoord::from_world(32.0 * 16.0, 0.0, 16.0);
        assert_eq!(coord, ChunkCoord(1, 0));
    }

    #[test]
    fn chunk_tile_access() {
        let mut chunk = make_chunk(TileKind::Grass);
        chunk.tile_mut(5, 3).kind = TileKind::Water;
        assert_eq!(chunk.tile(5, 3).kind, TileKind::Water);
        assert_eq!(chunk.tile(0, 0).kind, TileKind::Grass);
    }

    #[test]
    fn chebyshev_dist() {
        let a = ChunkCoord(0, 0);
        let b = ChunkCoord(3, 2);
        assert_eq!(a.chebyshev_dist(b), 3);
    }
}
