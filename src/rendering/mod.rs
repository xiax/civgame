use crate::simulation::plants;
use crate::world::chunk_streaming;
use bevy::prelude::*;

pub mod animations;
pub mod camera;
pub mod color_map;
pub mod entity_sprites;
pub mod fog;
pub mod path_debug;
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
            .insert_resource(chunk_streaming::ChunkBoundaryOverlay::default())
            .insert_resource(path_debug::PathDebugOverlay::default())
            .insert_resource(fog::FogMap::default())
            .insert_resource(fog::FogTileMaterials::default())
            .add_systems(
                Startup,
                (
                    camera::setup_camera,
                    pixel_art::setup_pixel_art,
                    sprite_library::setup_sprite_library,
                ),
            )
            .add_systems(
                OnEnter(crate::GameState::Playing),
                camera::position_camera_for_spawn,
            )
            .add_systems(
                PostStartup,
                (
                    chunk_streaming::setup_tile_materials,
                    fog::setup_fog_tile_materials.after(chunk_streaming::setup_tile_materials),
                ),
            )
            .add_systems(
                Update,
                (
                    camera::camera_input_system,
                    entity_sprites::toggle_art_mode,
                    entity_sprites::handle_art_mode_change,
                    chunk_streaming::update_chunk_retention_system
                        .before(chunk_streaming::chunk_streaming_system),
                    chunk_streaming::update_simulation_focus_system
                        .before(chunk_streaming::chunk_streaming_system),
                    chunk_streaming::chunk_streaming_system.after(camera::camera_input_system),
                    chunk_streaming::update_tile_z_view_system.after(camera::camera_input_system),
                    fog::fog_update_system.after(chunk_streaming::chunk_streaming_system),
                    chunk_streaming::toggle_chunk_boundary_overlay_system,
                    chunk_streaming::chunk_boundary_gizmo_system,
                    path_debug::selected_agent_path_gizmo_system,
                    path_debug::flow_field_gizmo_system,
                    path_debug::chunk_graph_gizmo_system,
                    path_debug::connectivity_component_gizmo_system,
                    path_debug::recent_failures_gizmo_system,
                    path_debug::selected_agent_failures_gizmo_system,
                )
                    .run_if(in_state(crate::GameState::Playing)),
            )
            .add_systems(
                Update,
                (
                    entity_sprites::spawn_person_sprites,
                    entity_sprites::animate_person_sprites,
                    entity_sprites::spawn_faction_center_sprites,
                    entity_sprites::spawn_bed_sprites,
                    entity_sprites::spawn_wall_sprites,
                    entity_sprites::spawn_campfire_sprites,
                    entity_sprites::spawn_door_sprites,
                    entity_sprites::spawn_table_sprites,
                    entity_sprites::spawn_chair_sprites,
                    entity_sprites::spawn_workbench_sprites,
                    entity_sprites::spawn_loom_sprites,
                    entity_sprites::spawn_blueprint_sprites,
                ),
            )
            .add_systems(
                Update,
                (
                    entity_sprites::spawn_plant_sprites,
                    entity_sprites::spawn_ground_item_sprites,
                    entity_sprites::update_plant_sprites,
                    entity_sprites::spawn_wolf_sprites,
                    entity_sprites::spawn_deer_sprites,
                    entity_sprites::spawn_horse_sprites,
                    entity_sprites::animate_wolves_system,
                    entity_sprites::animate_deer_system,
                    entity_sprites::animate_horses_system,
                    animations::handle_combat_events,
                    animations::update_animations,
                    plants::plant_growth_system,
                    plants::seed_scatter_system.after(plants::plant_growth_system),
                ),
            )
            .add_systems(
                Update,
                (
                    entity_sprites::spawn_cow_sprites,
                    entity_sprites::spawn_rabbit_sprites,
                    entity_sprites::spawn_pig_sprites,
                    entity_sprites::spawn_fox_sprites,
                    entity_sprites::spawn_cat_sprites,
                    entity_sprites::animate_cows_system,
                    entity_sprites::animate_rabbits_system,
                    entity_sprites::animate_pigs_system,
                    entity_sprites::animate_foxes_system,
                    entity_sprites::animate_cats_system,
                ),
            )
            .add_systems(Update, entity_sprites::update_clothing_from_equipment)
            .add_systems(
                Update,
                crate::simulation::settlement::zone_overlay_gizmo_system,
            )
            .add_systems(
                Update,
                (
                    entity_sprites::update_entity_z_visibility_system
                        .after(camera::camera_input_system)
                        .after(fog::fog_update_system),
                    entity_sprites::apply_entity_fog_tint_system
                        .after(entity_sprites::update_entity_z_visibility_system)
                        .after(entity_sprites::animate_person_sprites)
                        .after(entity_sprites::update_clothing_from_equipment)
                        .after(animations::update_animations),
                ),
            )
            .add_systems(PostUpdate, fog::apply_fog_to_tiles_system);
    }
}
