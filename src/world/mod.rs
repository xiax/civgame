use bevy::prelude::*;

use crate::game_state::{RegenerateWorldRequest, WorldSeed};

pub mod biome;
pub mod chunk;
pub mod chunk_streaming;
pub mod climate;
pub mod erosion;
pub mod geomorph;
pub mod globe;
pub mod hydrology;
pub mod locality;
pub mod plates;
pub mod seasons;
pub mod spatial;
pub mod terrain;
pub mod tile;
pub mod water;
pub mod water_current;
pub mod water_runtime;

pub use chunk::ChunkMap;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        // Resource catalog must be inserted before any system reads
        // resource-typed data. We install it here (alongside the globe)
        // because WorldPlugin already loads file-system data at build.
        // The catalog is shared two ways: a Bevy `Resource` for systems
        // that take `Res<ResourceCatalog>`, and a process-global
        // `OnceLock` (`core_ids::install_catalog`) for the legacy
        // `ResourceId::*` accessors which can't take system params.
        let catalog = crate::economy::resource_catalog::load_resource_catalog();
        crate::economy::core_ids::install_catalog(catalog.clone());
        // P5: faction-archetype registry. Built at startup from the four
        // supported legacy archetypes (`derive_from_legacy` is the
        // current backing builder). Future RON loading replaces the
        // builder without touching consumers — the registry is the new
        // forward-facing entry point exposed via
        // `derive_from_archetype_key`.
        let archetype_registry = crate::simulation::archetype::default_registry(&catalog);
        app.insert_resource(catalog);
        app.insert_resource(archetype_registry);

        // World seed drives both the climate globe and the per-tile
        // terrain Perlins. Insert before loading the globe so reroll
        // paths can read it back.
        let seed = WorldSeed::default();
        app.insert_resource(seed);
        app.insert_resource(globe::load_or_generate(seed.0));
        app.insert_resource(terrain::WorldGen::with_seed(seed.0 as u32));

        app.world_mut()
            .register_component_hooks::<spatial::Indexed>()
            .on_remove(spatial::on_indexed_remove);
        app.insert_resource(ChunkMap::default())
            .insert_resource(spatial::SpatialIndex::default())
            .insert_resource(seasons::Calendar::default())
            .insert_resource(chunk_streaming::ChunkRetention::default())
            .insert_resource(chunk_streaming::PendingChunkStreams::default())
            .insert_resource(chunk_streaming::PendingTileRefreshes::default())
            .insert_resource(water_runtime::RuntimeWater::default())
            .insert_resource(water_runtime::WaterSim::default())
            .insert_resource(water_current::WaterCurrentField::default())
            .init_resource::<crate::simulation::perf::PerfWorkBudget>()
            .init_resource::<crate::simulation::perf::BackgroundWorkDiagnostics>()
            .add_event::<chunk_streaming::TileChangedEvent>()
            .add_event::<chunk_streaming::TileCarvedEvent>()
            .add_event::<chunk_streaming::ChunkLoadedEvent>()
            .add_event::<chunk_streaming::ChunkUnloadedEvent>()
            .add_event::<RegenerateWorldRequest>()
            .add_systems(
                Update,
                regenerate_world_system.run_if(in_state(crate::GameState::SpawnSelect)),
            )
            .add_systems(
                OnExit(crate::GameState::SpawnSelect),
                persist_globe_on_commit_system,
            )
            .add_systems(
                OnEnter(crate::GameState::Playing),
                terrain::spawn_world_system,
            )
            // Phase 3 persistent water: re-apply `RuntimeWater` columns and
            // re-stamp tile-replacing structures (Bridge; Dam in Phase 4)
            // onto freshly-streamed chunks. Runs after the streaming load
            // pass so it sees this tick's `ChunkLoadedEvent`s, and before
            // `refresh_changed_tiles_system` (PostUpdate) consumes the
            // `TileChangedEvent`s it emits.
            .add_systems(
                FixedUpdate,
                (
                    // Re-carve dug well stepwell geometry (shaft + helix) lost
                    // when a footprint chunk regenerated, re-stamp constructed
                    // Wall tiles (the delta is lost on regen; the `Wall` entity
                    // in `WallMap` is the durable truth), then re-apply the
                    // `RuntimeWater` columns + Bridge/Dam tiles. Chained so the
                    // water restamp stamps onto the just-re-carved shaft, and
                    // the wall restamp runs after wells so re-carved well-rim
                    // walls already read `Wall` and are skipped.
                    crate::simulation::well::restamp_wells_on_chunk_load,
                    crate::simulation::construction::restamp_walls_on_chunk_load,
                    crate::simulation::excavation::restamp_excavation_on_chunk_load,
                    water_runtime::restamp_runtime_water_on_chunk_load,
                    // Phase 3 swimming: rebuild the per-tile current field
                    // last, after the chunk's water state is finalised.
                    water_current::water_current_field_system,
                )
                    .chain()
                    .after(chunk_streaming::chunk_streaming_system)
                    .run_if(in_state(crate::GameState::Playing)),
            )
            // Phase 5 background fluid sim. Snapshot in PostUpdate, poll in
            // PreUpdate (mirrors the pathfinding async pattern): the poll
            // applies + emits `TileChangedEvent` early so the same-frame
            // PostUpdate pathfinding/sprite pipeline picks up the flips.
            .add_systems(
                PreUpdate,
                water_runtime::poll_water_sim_task_system
                    .run_if(in_state(crate::GameState::Playing)),
            )
            .add_systems(
                PostUpdate,
                (
                    water_runtime::aquifer_seep_emitter_system,
                    water_runtime::spawn_water_sim_task_system,
                )
                    .chain()
                    .run_if(in_state(crate::GameState::Playing)),
            )
            .add_systems(PostUpdate, chunk_streaming::refresh_changed_tiles_system);
    }
}

/// Drains `RegenerateWorldRequest` events fired by spawn-select. Each event
/// rebuilds the climate globe + per-tile noise from the current `WorldSeed`
/// (no disk write — only the chosen world is cached on commit).
fn regenerate_world_system(
    mut events: EventReader<RegenerateWorldRequest>,
    seed: Res<WorldSeed>,
    mut commands: Commands,
    mut spawn_tex: ResMut<crate::ui::spawn_select::SpawnSelectTexture>,
    mut map_tex: ResMut<crate::ui::world_map::WorldMapTexture>,
) {
    if events.is_empty() {
        return;
    }
    events.clear();

    info!("Regenerating world from seed {}", seed.0);
    commands.insert_resource(globe::generate_globe(seed.0));
    commands.insert_resource(terrain::WorldGen::with_seed(seed.0 as u32));
    spawn_tex.clear_handle();
    map_tex.clear_handle();
}

/// On transition out of spawn-select (player has chosen their region),
/// persist the active globe to `world.bin` so the next launch boots into
/// the same world.
fn persist_globe_on_commit_system(globe: Res<globe::Globe>) {
    globe::save_globe(&globe);
}
