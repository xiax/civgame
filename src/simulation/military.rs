use ahash::AHashMap;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::simulation::combat::{CombatTarget, Health};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI};
use crate::simulation::schedule::SimClock;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::TILE_SIZE;

/// Tracks `HotspotKind::RallyPoint` registrations so they can be unregistered
/// when no drafted unit is still routing to them. Without this they'd
/// accumulate forever — every right-click leaks a flow-field rebuild slot.
#[derive(Resource, Default)]
pub struct ActiveRallyPoints {
    /// tile -> last sim tick a drafted unit was still routing to it.
    pub last_seen: AHashMap<(i16, i16, i8), u64>,
}

const RALLY_EXPIRE_TICKS: u64 = 60;

/// Drives the two military-mode tasks each tick.
///
/// `MilitaryMove`: arrival → idle in place (the unit holds its position
/// until the next order).
///
/// `MilitaryAttack`: re-target the foe each tick (it may have moved),
/// re-issue routing if its tile changed, and once Chebyshev-adjacent at the
/// same Z, set `CombatTarget` so `combat_system` swings on the next tick.
/// When the foe dies or vanishes, drop back to idle (still drafted).
pub fn military_task_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    health_q: Query<&Health>,
    transform_q: Query<&Transform>,
    mut q: Query<
        (
            Entity,
            &mut PersonAI,
            &Transform,
            &mut CombatTarget,
            &LodLevel,
        ),
        With<Drafted>,
    >,
    mut rally: ResMut<ActiveRallyPoints>,
    clock: Res<SimClock>,
) {
    let move_id = TaskKind::MilitaryMove as u16;
    let attack_id = TaskKind::MilitaryAttack as u16;

    for (_entity, mut ai, transform, mut combat, lod) in q.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        if ai.task_id == move_id {
            // Refresh rally-point timestamp so the expire system keeps the
            // flow field around while units are still en route.
            rally
                .last_seen
                .insert((ai.dest_tile.0, ai.dest_tile.1, ai.target_z), clock.tick);

            // Arrival: movement_system flips state to Working when the agent
            // steps onto the dest tile. For a Move order there is no work to
            // do, so we go straight back to Idle.
            if ai.state == AiState::Working
                && (cur_tx, cur_ty) == (ai.dest_tile.0 as i32, ai.dest_tile.1 as i32)
            {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
            }
            continue;
        }

        if ai.task_id == attack_id {
            // Foe gone or dead → idle in place, stay drafted.
            let foe = match ai.target_entity {
                Some(e) => e,
                None => {
                    ai.task_id = PersonAI::UNEMPLOYED;
                    ai.state = AiState::Idle;
                    combat.0 = None;
                    continue;
                }
            };
            let foe_alive = health_q.get(foe).map(|h| !h.is_dead()).unwrap_or(false);
            let foe_transform = transform_q.get(foe).ok();
            if !foe_alive || foe_transform.is_none() {
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.state = AiState::Idle;
                ai.target_entity = None;
                combat.0 = None;
                continue;
            }
            let foe_t = foe_transform.unwrap();
            let foe_tx = (foe_t.translation.x / TILE_SIZE).floor() as i32;
            let foe_ty = (foe_t.translation.y / TILE_SIZE).floor() as i32;
            let foe_z = chunk_map.surface_z_at(foe_tx, foe_ty) as i8;

            rally
                .last_seen
                .insert((foe_tx as i16, foe_ty as i16, foe_z), clock.tick);

            let dx = (foe_tx - cur_tx).abs();
            let dy = (foe_ty - cur_ty).abs();
            let adjacent = dx.max(dy) <= 1 && (ai.current_z as i32 - foe_z as i32).abs() <= 1;

            if adjacent {
                // Within strike range — let combat_system handle the swing.
                combat.0 = Some(foe);
            } else {
                // Re-route if the foe has moved off our last destination.
                if ai.dest_tile != (foe_tx as i16, foe_ty as i16) {
                    let cur_chunk = ChunkCoord(
                        cur_tx.div_euclid(CHUNK_SIZE as i32),
                        cur_ty.div_euclid(CHUNK_SIZE as i32),
                    );
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        (foe_tx as i16, foe_ty as i16),
                        TaskKind::MilitaryAttack,
                        Some(foe),
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                }
                combat.0 = None;
            }
        }
    }
}

/// Garbage-collects rally-point flow fields after `RALLY_EXPIRE_TICKS` ticks
/// without any drafted unit still routing to them.
pub fn expire_rally_points_system(
    clock: Res<SimClock>,
    mut rally: ResMut<ActiveRallyPoints>,
    mut hotspots: ResMut<HotspotFlowFields>,
) {
    let now = clock.tick;
    let stale: Vec<(i16, i16, i8)> = rally
        .last_seen
        .iter()
        .filter(|(_, &t)| now.saturating_sub(t) > RALLY_EXPIRE_TICKS)
        .map(|(k, _)| *k)
        .collect();
    for tile in stale {
        rally.last_seen.remove(&tile);
        hotspots.unregister(tile, HotspotKind::RallyPoint);
    }
}
