use crate::simulation::plants;
use crate::world::chunk_streaming;
use bevy::prelude::*;

pub mod animations;
pub mod camera;
pub mod color_map;
pub mod entity_sprites;
pub mod fog;
pub mod pixel_art;
pub mod sprite_library;
pub mod tile_render;

pub struct RenderingPlugin;

impl Plugin for RenderingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(camera::CameraState::default())
            .insert_resource(camera::CameraViewZ::default())
            .insert_resource(chunk_streaming::TileMaterials::default())
            .insert_resource(chunk_streaming::TileSpriteIndex::default())
            .insert_resource(fog::FogMap::default())
            .insert_resource(fog::FogTileMaterials::default())
            .add_systems(Startup, (camera::setup_camera, pixel_art::setup_pixel_art, sprite_library::setup_sprite_library))
            .add_systems(PostStartup, (
                chunk_streaming::setup_tile_materials,
                fog::setup_fog_tile_materials.after(chunk_streaming::setup_tile_materials),
            ))
            .add_systems(
                Update,
                (
                    camera::camera_input_system,
                    entity_sprites::toggle_art_mode,
                    entity_sprites::handle_art_mode_change,
                    chunk_streaming::chunk_streaming_system.after(camera::camera_input_system),
                    chunk_streaming::update_tile_z_view_system.after(camera::camera_input_system),
                    fog::fog_update_system.after(chunk_streaming::chunk_streaming_system),
                ),
            )
            .add_systems(
                Update,
                (
                    entity_sprites::spawn_person_sprites,
                    entity_sprites::animate_person_sprites,
                    entity_sprites::spawn_faction_center_sprites,
                    entity_sprites::spawn_bed_sprites,
                    entity_sprites::spawn_wall_sprites,
                    entity_sprites::spawn_blueprint_sprites,
                    entity_sprites::spawn_plant_sprites,
                    entity_sprites::update_plant_sprites,
                    entity_sprites::spawn_wolf_sprites,
                    entity_sprites::spawn_deer_sprites,
                    animations::handle_combat_events,
                    animations::update_animations,
                    plants::plant_growth_system,
                    plants::seed_scatter_system.after(plants::plant_growth_system),
                ),
            )
            .add_systems(Update, entity_sprites::update_clothing_from_equipment)
            .add_systems(
                Update,
                (
                    entity_sprites::update_entity_z_visibility_system
                        .after(camera::camera_input_system)
                        .after(fog::fog_update_system),
                    entity_sprites::apply_entity_fog_tint_system
                        .after(entity_sprites::update_entity_z_visibility_system),
                ),
            )
            .add_systems(
                PostUpdate,
                fog::apply_fog_to_tiles_system,
            );
    }
}
