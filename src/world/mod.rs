use bevy::prelude::*;

pub mod biome;
pub mod chunk;
pub mod chunk_streaming;
pub mod climate;
pub mod erosion;
pub mod globe;
pub mod hydrology;
pub mod plates;
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
        // Resource catalog must be inserted before any system reads
        // resource-typed data. We install it here (alongside the globe)
        // because WorldPlugin already loads file-system data at build.
        // The catalog is shared two ways: a Bevy `Resource` for systems
        // that take `Res<ResourceCatalog>`, and a process-global
        // `OnceLock` (`core_ids::install_catalog`) for the legacy
        // `Good::*` methods which can't take system params.
        let catalog = crate::economy::resource_catalog::load_resource_catalog();
        crate::economy::core_ids::install_catalog(catalog.clone());
        app.insert_resource(catalog);

        insert_globe(app);
        app.world_mut()
            .register_component_hooks::<spatial::Indexed>()
            .on_remove(spatial::on_indexed_remove);
        app.insert_resource(ChunkMap::default())
            .insert_resource(spatial::SpatialIndex::default())
            .insert_resource(seasons::Calendar::default())
            .insert_resource(terrain::WorldGen::new())
            .insert_resource(chunk_streaming::ChunkRetention::default())
            .add_event::<chunk_streaming::TileChangedEvent>()
            .add_event::<chunk_streaming::ChunkLoadedEvent>()
            .add_event::<chunk_streaming::ChunkUnloadedEvent>()
            .add_systems(OnEnter(crate::GameState::Playing), terrain::spawn_world_system)
            .add_systems(PostUpdate, chunk_streaming::refresh_changed_tiles_system);
    }
}
