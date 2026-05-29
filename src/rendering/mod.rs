use crate::world::chunk_streaming;
use bevy::prelude::*;

pub mod animations;
pub mod camera;
pub mod color_map;
pub mod day_night;
pub mod entity_sprites;
pub mod fog;
pub mod path_debug;
pub mod pixel_art;
pub mod plant_sprites;
pub mod projection;
pub mod sprite_library;
pub mod tile_render;
pub mod vehicle_part_sprites;
pub mod water_current_render;

pub struct RenderingPlugin;

impl Plugin for RenderingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(camera::CameraState::default())
            .insert_resource(camera::CameraViewZ::default())
            .insert_resource(projection::MapViewMode::default())
            .insert_resource(projection::MapProjection::default())
            .insert_resource(chunk_streaming::TileMaterials::default())
            .insert_resource(chunk_streaming::TileSpriteIndex::default())
            .insert_resource(chunk_streaming::ChunkBoundaryOverlay::default())
            .insert_resource(path_debug::PathDebugOverlay::default())
            .insert_resource(fog::FogMap::default())
            .insert_resource(fog::FogTileMaterials::default())
            .insert_resource(pixel_art::AnimalTextures::default())
            .insert_resource(plant_sprites::PlantSpriteSet::default())
            .insert_resource(water_current_render::CurrentStreakIndex::default())
            .add_systems(
                Startup,
                (
                    camera::setup_camera,
                    day_night::spawn_day_night_overlay,
                    pixel_art::setup_pixel_art,
                    pixel_art::setup_animal_textures,
                    plant_sprites::setup_plant_sprites,
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
                    chunk_streaming::update_tile_z_view_system.after(camera::camera_input_system),
                    // fog_update_system stays in Update: it owns
                    // `fog_map.dirty_tiles`, which `apply_fog_to_tiles_system`
                    // (PostUpdate) consumes. If fog moved to FixedUpdate
                    // and FixedUpdate ran twice in one frame, the second
                    // run's `dirty_tiles.clear()` would wipe the first
                    // run's diff before PostUpdate ever applied it —
                    // visibility changes get silently dropped.
                    fog::fog_update_system,
                    chunk_streaming::toggle_chunk_boundary_overlay_system,
                    chunk_streaming::chunk_boundary_gizmo_system,
                    path_debug::selected_agent_path_gizmo_system,
                    path_debug::flow_field_gizmo_system,
                    path_debug::chunk_graph_gizmo_system,
                    path_debug::connectivity_component_gizmo_system,
                    path_debug::recent_failures_gizmo_system,
                    path_debug::selected_agent_failures_gizmo_system,
                    projection::update_skirt_visibility_system,
                    entity_sprites::update_edge_wall_geometry_system
                        .after(entity_sprites::spawn_edge_wall_sprites)
                        .after(entity_sprites::spawn_edge_door_sprites),
                    day_night::update_day_night_overlay_system,
                    water_current_render::water_current_render_system,
                    water_current_render::animate_current_streaks_system,
                )
                    .run_if(in_state(crate::GameState::Playing)),
            )
            // Chunk streaming pipeline runs on FixedUpdate (20 Hz) —
            // the unload pass walks `chunk_map.0.keys()` and the load
            // pass scans every focus disc; at 60+ Hz that's enough to
            // spike frame time. fog_update_system intentionally stays
            // on Update (see comment above); it tolerates a stale
            // chunk_map for one fixed-tick on the chunk-load boundary.
            .add_systems(
                FixedUpdate,
                (
                    chunk_streaming::update_chunk_retention_system
                        .before(chunk_streaming::chunk_streaming_system),
                    chunk_streaming::update_simulation_focus_system
                        .before(chunk_streaming::chunk_streaming_system),
                    chunk_streaming::chunk_streaming_system,
                    // Back-fill south skirts for cliffs whose southern
                    // neighbour just loaded — runs after streaming so it
                    // sees this tick's `ChunkLoadedEvent`s.
                    chunk_streaming::attach_late_south_skirts_system
                        .after(chunk_streaming::chunk_streaming_system),
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
                    entity_sprites::spawn_edge_wall_sprites,
                    entity_sprites::spawn_edge_door_sprites,
                    entity_sprites::spawn_campfire_sprites,
                    entity_sprites::spawn_door_sprites,
                    entity_sprites::spawn_table_sprites,
                    entity_sprites::spawn_chair_sprites,
                    entity_sprites::spawn_workbench_sprites,
                    entity_sprites::spawn_loom_sprites,
                    entity_sprites::spawn_well_sprites,
                    entity_sprites::spawn_tent_shelter_sprites,
                    entity_sprites::refresh_vehicle_sprites_system,
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
            .add_systems(PostUpdate, fog::apply_fog_to_tiles_system)
            // Tilt-view projection layer. PreUpdate restores logical
            // Transform values so sim systems read top-down coords;
            // PostUpdate re-projects after every Transform writer is done.
            // Both systems early-return in TopDown mode for bit-exact
            // identity behaviour.
            .add_systems(
                PreUpdate,
                projection::revert_view_projection_system
                    .run_if(in_state(crate::GameState::Playing)),
            )
            .add_systems(
                PostUpdate,
                projection::apply_view_projection_system
                    .run_if(in_state(crate::GameState::Playing)),
            )
            .add_systems(
                Update,
                (
                    projection::toggle_view_mode_system,
                    projection::camera_recenter_on_mode_change_system
                        .after(projection::toggle_view_mode_system),
                )
                    .run_if(in_state(crate::GameState::Playing)),
            )
            // Auto-attach `ProjectedAnchor::Dynamic` to every world-living
            // entity carrying one of these marker components. Saves
            // touching ~40 spawn sites scattered across construction.rs /
            // person.rs / animals / nomad / reproduction.
            .add_systems(
                Update,
                (
                    projection::auto_attach_dynamic::<crate::simulation::person::Person>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Wolf>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Deer>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Horse>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Cow>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Pig>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Rabbit>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Fox>,
                    projection::auto_attach_dynamic::<crate::simulation::animals::Cat>,
                    projection::auto_attach_dynamic::<crate::simulation::corpse::Corpse>,
                    projection::auto_attach_dynamic::<crate::simulation::plants::Plant>,
                    projection::auto_attach_dynamic::<crate::simulation::items::GroundItem>,
                )
                    .run_if(in_state(crate::GameState::Playing)),
            )
            .add_systems(
                Update,
                (
                    projection::auto_attach_dynamic::<crate::simulation::construction::Bed>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Wall>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Door>,
                    projection::auto_attach_dynamic::<
                        crate::simulation::construction::EdgeWallVisual,
                    >,
                    projection::auto_attach_dynamic::<
                        crate::simulation::construction::EdgeDoorVisual,
                    >,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Workbench>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Loom>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Table>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Chair>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Campfire>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Granary>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Shrine>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Market>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Barracks>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Monument>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Bridge>,
                    projection::auto_attach_dynamic::<crate::simulation::construction::Blueprint>,
                    projection::auto_attach_dynamic::<crate::simulation::faction::FactionCenter>,
                    projection::auto_attach_dynamic::<crate::simulation::vehicle::Vehicle>,
                )
                    .run_if(in_state(crate::GameState::Playing)),
            );
    }
}
