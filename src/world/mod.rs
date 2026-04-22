use bevy::prelude::*;

pub mod chunk;
pub mod tile;
pub mod terrain;
pub mod spatial;
pub mod seasons;
pub mod globe;
pub mod chunk_streaming;

pub use chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
pub use tile::{TileData, TileKind};

fn insert_globe(app: &mut App) {
    app.insert_resource(globe::load_or_generate(42));
}

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        insert_globe(app);
        app.insert_resource(ChunkMap::default())
            .insert_resource(spatial::SpatialIndex::default())
            .insert_resource(seasons::Calendar::default())
            .add_systems(Startup, terrain::spawn_world_system);
    }
}
