//! Executor for `Task::ClearObstacle`. Worker walks adjacent to a
//! `ConstructionObstacle`-tagged entity inside a blueprint footprint,
//! accumulates `ai.work_progress` against the obstacle's `work_ticks`,
//! and on completion drops yields on the ground, despawns the entity,
//! and pops it from the blueprint's `pending_clear`.

use bevy::prelude::*;

use crate::simulation::construction::{Blueprint, BlueprintMap, StructureIndex};
use crate::simulation::gather::spawn_ground_drop;
use crate::simulation::lod::LodLevel;
use crate::simulation::obstacle::relocate_entity_aside;
use crate::simulation::obstacle::{
    resolve_clear_yields, resolve_footprint_sync, ConstructionObstacle, ObstacleResolution,
};
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plants::{despawn_plant_internals, Plant, PlantMap, PlantSpriteIndex};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::Skills;
use crate::simulation::tasks::TaskKind;
use crate::simulation::typed_task::ActionQueue;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::{Indexed, SpatialIndex};
use crate::world::terrain::TILE_SIZE;

pub fn clear_obstacle_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    obstacles: Query<(&Indexed, &ConstructionObstacle)>,
    plants: Query<&Plant>,
    mut blueprints: Query<&mut Blueprint>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut workers: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut Skills,
            &BucketSlot,
            &LodLevel,
        ),
        With<Person>,
    >,
) {
    for (mut ai, mut aq, mut skills, slot, lod) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || aq.current_task_kind() != TaskKind::ClearObstacle as u16
        {
            continue;
        }

        let Some((obstacle_entity, bp_entity)) = aq.current.as_clear_obstacle() else {
            ai.state = AiState::Idle;
            ai.work_progress = 0;
            ai.target_entity = None;
            aq.advance();
            continue;
        };

        // Obstacle entity gone (raced with another worker, or despawned by
        // terraform / blueprint relocation): pop from pending_clear and exit.
        let Ok((indexed, obstacle)) = obstacles.get(obstacle_entity) else {
            if let Ok(mut bp) = blueprints.get_mut(bp_entity) {
                bp.pending_clear.retain(|&e| e != obstacle_entity);
            }
            ai.state = AiState::Idle;
            ai.work_progress = 0;
            ai.target_entity = None;
            aq.advance();
            continue;
        };

        let ObstacleResolution::WorkerClear {
            work_ticks,
            skill,
            skill_xp,
        } = obstacle.resolution
        else {
            // Relocate-resolution obstacle should never have entered the worker
            // pipeline; skip and clean up.
            if let Ok(mut bp) = blueprints.get_mut(bp_entity) {
                bp.pending_clear.retain(|&e| e != obstacle_entity);
            }
            ai.state = AiState::Idle;
            ai.work_progress = 0;
            ai.target_entity = None;
            aq.advance();
            continue;
        };

        if (ai.work_progress as u32) < work_ticks {
            continue;
        }

        // Completion: drop yields on ground, despawn entity, pop pending_clear,
        // award XP, reset state.
        let tile = indexed.tile;
        for (rid, qty) in resolve_clear_yields(obstacle_entity, &plants) {
            spawn_ground_drop(&mut commands, tile.0, tile.1, rid, qty);
        }
        if plants.get(obstacle_entity).is_ok() {
            despawn_plant_internals(
                &mut commands,
                obstacle_entity,
                tile,
                &mut plant_map,
                &mut plant_sprite_index,
            );
        } else {
            commands.entity(obstacle_entity).despawn_recursive();
        }

        if let Ok(mut bp) = blueprints.get_mut(bp_entity) {
            bp.pending_clear.retain(|&e| e != obstacle_entity);
        }

        skills.gain_xp(skill, skill_xp);
        ai.state = AiState::Idle;
        ai.work_progress = 0;
        ai.target_entity = None;
        aq.advance();
    }
}

/// One-shot pass that clears obstacles standing on seeded-structure tiles.
/// Game-start seeding (`seed_starting_buildings_system`,
/// `seed_nomadic_camp`) plops fully-built structures directly without
/// going through the blueprint pipeline, so `populate_pending_clear_system`
/// never sees them. This system reads `StructureIndex` (populated by
/// the `StructureLabel` add-hook) and synchronously despawns
/// `WorkerClear` obstacles + relocates `Relocate` obstacles on every
/// seeded tile. Runs `OnEnter(Playing)` after the seeders.
pub fn clear_obstacles_under_seeded_structures(
    mut commands: Commands,
    structure_index: Res<StructureIndex>,
    spatial: Res<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    blueprint_map: Res<BlueprintMap>,
    obstacles: Query<(&Indexed, &ConstructionObstacle)>,
    plants: Query<&Plant>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut transforms: Query<&mut Transform>,
) {
    let tiles: Vec<(i32, i32)> = structure_index.0.keys().copied().collect();
    if tiles.is_empty() {
        return;
    }
    let bp_map_ref = &blueprint_map;
    resolve_footprint_sync(
        &tiles,
        0,
        &mut commands,
        &spatial,
        &chunk_map,
        &obstacles,
        &plants,
        &mut plant_map,
        &mut plant_sprite_index,
        &mut transforms,
        &|tile| bp_map_ref.0.contains_key(&tile),
        &mut |c, tx, ty, rid, qty| {
            spawn_ground_drop(c, tx, ty, rid, qty);
        },
    );
}

/// Reactive cleanup for obstacles that spawn *after* a structure / blueprint
/// is already there. The classic case: chunk streaming runs in FixedUpdate,
/// so loose rocks scatter into chunks that streamed in well after the
/// `OnEnter(Playing)` seeded-structure pass — those rocks would otherwise
/// sit under huts and palisades forever. Reads the obstacle's `Transform`
/// (avoids the one-tick `Indexed.tile` settle race) and, if its tile is
/// already a `StructureIndex` or `BlueprintMap` entry, applies the
/// resolution synchronously: `WorkerClear` despawns + drops yields,
/// `Relocate` shoves the entity aside.
pub fn react_obstacle_under_structure_system(
    mut commands: Commands,
    structure_index: Res<StructureIndex>,
    blueprint_map: Res<BlueprintMap>,
    spatial: Res<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    plants: Query<&Plant>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut params: ParamSet<(
        Query<(Entity, &Transform, &ConstructionObstacle), Added<ConstructionObstacle>>,
        Query<&mut Transform>,
    )>,
) {
    // Phase 1: snapshot (entity, tile, resolution) for newly-added obstacles
    // that landed on an occupied tile. Drops the read-borrow before mutation.
    let mut hits: Vec<(Entity, (i32, i32), ObstacleResolution)> = Vec::new();
    for (entity, transform, obstacle) in params.p0().iter() {
        let tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        if !structure_index.0.contains_key(&tile) && !blueprint_map.0.contains_key(&tile) {
            continue;
        }
        hits.push((entity, tile, obstacle.resolution));
    }
    if hits.is_empty() {
        return;
    }

    // Phase 2: apply resolution. WorkerClear despawns immediately (no
    // worker to dispatch — the structure / blueprint already exists);
    // Relocate moves the entity aside.
    for (entity, tile, resolution) in hits {
        match resolution {
            ObstacleResolution::WorkerClear { .. } => {
                for (rid, qty) in resolve_clear_yields(entity, &plants) {
                    spawn_ground_drop(&mut commands, tile.0, tile.1, rid, qty);
                }
                if plants.get(entity).is_ok() {
                    despawn_plant_internals(
                        &mut commands,
                        entity,
                        tile,
                        &mut plant_map,
                        &mut plant_sprite_index,
                    );
                } else {
                    commands.entity(entity).despawn_recursive();
                }
            }
            ObstacleResolution::Relocate => {
                let footprint = [tile];
                let bp_map_ref = &blueprint_map;
                let mut transforms = params.p1();
                relocate_entity_aside(
                    entity,
                    tile,
                    &footprint,
                    &chunk_map,
                    &spatial,
                    &mut transforms,
                    &|t| bp_map_ref.0.contains_key(&t),
                );
            }
        }
    }
}
