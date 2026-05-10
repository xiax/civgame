//! Generic construction-obstacle abstraction.
//!
//! Anything that should be removed before a structure goes up at a given
//! `(tile, z)` cell carries a [`ConstructionObstacle`] component. Plants
//! attach `WorkerClear` so workers harvest them via `Task::ClearObstacle`;
//! loose rocks attach `Relocate` so they're shoved to an adjacent free
//! tile synchronously when the blueprint is queued. Adding a future
//! obstacle kind is one component-attachment + (optionally) one arm in
//! `resolve_clear_yields` — no consumer code in `construction.rs` /
//! `terraform.rs` needs to learn about the new type.

use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::construction::{Blueprint, BlueprintMap};
use crate::simulation::plants::{
    despawn_plant_internals, Plant, PlantMap, PlantSpriteIndex,
};
use crate::simulation::skills::SkillKind;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::{Indexed, SpatialIndex};
use crate::world::terrain::tile_to_world;

#[derive(Component, Clone, Copy, Debug)]
pub struct ConstructionObstacle {
    pub resolution: ObstacleResolution,
}

#[derive(Clone, Copy, Debug)]
pub enum ObstacleResolution {
    /// Worker walks adjacent, performs a `ClearObstacle` task, the entity
    /// despawns when complete and any loot drops on the obstacle's tile.
    WorkerClear {
        work_ticks: u32,
        skill: SkillKind,
        skill_xp: u32,
    },
    /// System synchronously moves the entity to the nearest free tile
    /// outside the footprint. No worker, no time cost, no yield change.
    Relocate,
}

/// One obstacle entity inside a footprint.
#[derive(Clone, Copy, Debug)]
pub struct ObstacleHit {
    pub entity: Entity,
    pub resolution: ObstacleResolution,
    pub tile: (i32, i32),
}

/// Read-only scan: returns every `ConstructionObstacle` entity occupying
/// a cell in `tiles` at the given `z`. Caller decides how to apply each
/// hit (see `apply_obstacle_hits` for the standard policy).
pub fn scan_footprint(
    tiles: &[(i32, i32)],
    z: i8,
    spatial: &SpatialIndex,
    obstacles: &Query<(&Indexed, &ConstructionObstacle)>,
) -> Vec<ObstacleHit> {
    let mut hits = Vec::new();
    for &tile in tiles {
        for &entity in spatial.get(tile.0, tile.1) {
            let Ok((indexed, obs)) = obstacles.get(entity) else {
                continue;
            };
            if indexed.z != z as i32 {
                continue;
            }
            hits.push(ObstacleHit {
                entity,
                resolution: obs.resolution,
                tile,
            });
        }
    }
    hits
}

/// Spiral chebyshev outward from `from` (radius cap 6) for a tile that's
/// passable, outside `footprint_tiles`, and not already occupied by
/// another entity in the spatial index or by a blueprint. On hit, mutate
/// the entity's `Transform`. (`Indexed` reconciles next tick via
/// `sync_indexed_after_move_system` reading `Changed<Transform>`.) On
/// miss, leave it (degenerate; building will sit over it harmlessly).
///
/// `is_blueprint_tile` lets the caller exclude tiles already taken by
/// another blueprint without forcing this module to depend on
/// `BlueprintMap`'s concrete type.
pub fn relocate_entity_aside(
    entity: Entity,
    from: (i32, i32),
    footprint_tiles: &[(i32, i32)],
    chunk_map: &ChunkMap,
    spatial: &SpatialIndex,
    transforms: &mut Query<&mut Transform>,
    is_blueprint_tile: &dyn Fn((i32, i32)) -> bool,
) {
    const RADIUS_CAP: i32 = 6;
    for radius in 1..=RADIUS_CAP {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx.abs() != radius && dy.abs() != radius {
                    continue;
                }
                let nx = from.0 + dx;
                let ny = from.1 + dy;
                let cand = (nx, ny);
                if footprint_tiles.contains(&cand) {
                    continue;
                }
                if !chunk_map.is_passable(nx, ny) {
                    continue;
                }
                if !spatial.get(nx, ny).is_empty() {
                    continue;
                }
                if is_blueprint_tile(cand) {
                    continue;
                }
                let pos = tile_to_world(nx, ny);
                if let Ok(mut t) = transforms.get_mut(entity) {
                    t.translation.x = pos.x;
                    t.translation.y = pos.y;
                }
                return;
            }
        }
    }
}

/// Yields produced when a `WorkerClear` obstacle is cleared. Plants drop
/// their normal harvest yield (no-tool variant); future obstacle kinds
/// add arms here. Empty Vec means "no loot".
pub fn resolve_clear_yields(
    entity: Entity,
    plants: &Query<&Plant>,
) -> Vec<(ResourceId, u32)> {
    let mut out = Vec::new();
    if let Ok(plant) = plants.get(entity) {
        let (id, qty) = plant.kind.harvest_yield(false);
        if qty > 0 {
            out.push((id, qty));
        }
        for (extra_id, extra_qty) in plant.kind.harvest_extra_yields() {
            if extra_qty > 0 {
                out.push((extra_id, extra_qty));
            }
        }
    }
    out
}

/// Reactive system: every newly-spawned `Blueprint` gets its footprint
/// scanned for `ConstructionObstacle` entities. `WorkerClear` hits go
/// onto `bp.pending_clear` for the `ClearObstacle` task pipeline; the
/// rare `Relocate` hits (loose rocks) are moved aside synchronously
/// here. Runs in `Sequential` after movement (so any rocks moved this
/// tick by `clear_obstacle_task_system` are already in their new tile)
/// and before `construction_system` (so the build gate sees the
/// populated `pending_clear` on the same tick the bp was spawned).
pub fn populate_pending_clear_system(
    mut new_blueprints: Query<(&mut Blueprint,), Added<Blueprint>>,
    spatial: Res<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    blueprint_map: Res<BlueprintMap>,
    obstacles: Query<(&Indexed, &ConstructionObstacle)>,
    mut transforms: Query<&mut Transform>,
) {
    for (mut bp,) in new_blueprints.iter_mut() {
        let tile = bp.tile;
        let z = bp.target_z;
        let footprint = [tile];
        let hits = scan_footprint(&footprint, z, &spatial, &obstacles);
        for hit in hits {
            match hit.resolution {
                ObstacleResolution::WorkerClear { .. } => {
                    if !bp.pending_clear.contains(&hit.entity) {
                        bp.pending_clear.push(hit.entity);
                    }
                }
                ObstacleResolution::Relocate => {
                    let bp_map = &blueprint_map;
                    relocate_entity_aside(
                        hit.entity,
                        hit.tile,
                        &footprint,
                        &chunk_map,
                        &spatial,
                        &mut transforms,
                        &|tile| bp_map.0.contains_key(&tile),
                    );
                }
            }
        }
    }
}

/// Synchronous variant of the worker-clear pipeline: despawn every
/// `WorkerClear` obstacle in the footprint immediately and drop its
/// yields on its tile as `GroundItem`s. `Relocate` obstacles still
/// move aside. Used at game-start seeding and at terraform completion
/// where there is no worker to dispatch.
///
/// The caller threads in the resources / queries it already owns;
/// keeps this module decoupled from any specific spawn helper.
pub fn resolve_footprint_sync(
    tiles: &[(i32, i32)],
    z: i8,
    commands: &mut Commands,
    spatial: &SpatialIndex,
    chunk_map: &ChunkMap,
    obstacles: &Query<(&Indexed, &ConstructionObstacle)>,
    plants: &Query<&Plant>,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    transforms: &mut Query<&mut Transform>,
    is_blueprint_tile: &dyn Fn((i32, i32)) -> bool,
    spawn_drop: &mut dyn FnMut(&mut Commands, i32, i32, ResourceId, u32),
) {
    let hits = scan_footprint(tiles, z, spatial, obstacles);
    for hit in hits {
        match hit.resolution {
            ObstacleResolution::WorkerClear { .. } => {
                for (rid, qty) in resolve_clear_yields(hit.entity, plants) {
                    spawn_drop(commands, hit.tile.0, hit.tile.1, rid, qty);
                }
                if plants.get(hit.entity).is_ok() {
                    despawn_plant_internals(
                        commands,
                        hit.entity,
                        hit.tile,
                        plant_map,
                        plant_sprite_index,
                    );
                } else {
                    commands.entity(hit.entity).despawn_recursive();
                }
            }
            ObstacleResolution::Relocate => {
                relocate_entity_aside(
                    hit.entity,
                    hit.tile,
                    tiles,
                    chunk_map,
                    spatial,
                    transforms,
                    is_blueprint_tile,
                );
            }
        }
    }
}
