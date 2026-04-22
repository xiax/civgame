use bevy::prelude::*;
use crate::simulation::plants;
use crate::world::chunk_streaming;

pub mod camera;
pub mod tile_render;
pub mod entity_sprites;
pub mod color_map;

pub struct RenderingPlugin;

impl Plugin for RenderingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(camera::CameraState::default())
            .insert_resource(chunk_streaming::TileMaterials::default())
            .insert_resource(chunk_streaming::TileSpriteIndex::default())
            .add_systems(Startup, camera::setup_camera)
            .add_systems(
                PostStartup,
                (
                    chunk_streaming::setup_tile_materials,
                    plants::setup_plant_materials,
                    chunk_streaming::spawn_initial_tile_sprites
                        .after(chunk_streaming::setup_tile_materials)
                        .after(plants::setup_plant_materials),
                ),
            )
            .add_systems(
                Update,
                (
                    camera::camera_input_system,
                    chunk_streaming::chunk_streaming_system
                        .after(camera::camera_input_system),
                    entity_sprites::spawn_person_sprites,
                    entity_sprites::spawn_wolf_sprites,
                    entity_sprites::spawn_deer_sprites,
                    entity_sprites::entity_sprite_sync,
                ),
            );
    }
}
