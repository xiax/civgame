use bevy::prelude::*;

pub mod chunk;
pub mod chunk_streaming;
pub mod globe;
pub mod seasons;
pub mod spatial;
pub mod terrain;
pub mod tile;

pub use chunk::ChunkMap;

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
            .insert_resource(terrain::WorldGen::new())
            .add_event::<chunk_streaming::TileChangedEvent>()
            .add_systems(Startup, terrain::spawn_world_system)
            .add_systems(PostUpdate, chunk_streaming::refresh_changed_tiles_system);
    }
}
