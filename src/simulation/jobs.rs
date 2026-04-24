use bevy::prelude::*;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::tile::TileKind;
use crate::world::terrain::{WORLD_CHUNKS_X, WORLD_CHUNKS_Y, TILE_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::economy::agent::EconomicAgent;
use crate::pathfinding::chunk_graph::ChunkGraph;
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::person::PlayerOrder;
use super::goals::AgentGoal;
use super::items::GroundItem;
use super::memory::RelationshipMemory;
use super::person::{AiState, PersonAI};
use super::lod::LodLevel;
use super::reproduction::BiologicalSex;
use super::plants::{PlantMap, Plant, GrowthStage, PlantKind};

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Idle    = 0,
    Gather  = 1,
    Trader  = 2,
    Raid    = 3,
    Defend  = 4,
    Planter = 5,
    Hunter  = 6,
    Scavenge = 7,
}

pub fn find_nearest_tile(
    chunk_map: &ChunkMap,
    from: (i32, i32),
    radius: i32,
    kinds: &[TileKind],
) -> Option<(i16, i16)> {
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if let Some(kind) = chunk_map.tile_kind_at(tx, ty) {
                if kinds.contains(&kind) {
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((tx as i16, ty as i16));
                    }
                }
            }
        }
    }
    best
}

pub fn find_nearest_plant(
    plant_map: &PlantMap,
    from: (i32, i32),
    radius: i32,
    plant_query: &Query<&Plant>,
    mature_only: bool,
    kind_filter: Option<PlantKind>,
) -> Option<(i16, i16)> {
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if let Some(&entity) = plant_map.0.get(&(tx, ty)) {
                if let Ok(plant) = plant_query.get(entity) {
                    if mature_only && plant.stage != GrowthStage::Mature { continue; }
                    if let Some(k) = kind_filter {
                        if plant.kind != k { continue; }
                    }
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((tx as i16, ty as i16));
                    }
                }
            }
        }
    }
    best
}

pub fn find_nearest_item(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    item_check: &Query<(), With<GroundItem>>,
) -> Option<(Entity, i16, i16)> {
    let mut best: Option<(Entity, i16, i16)> = None;
    let mut best_dist = i32::MAX;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            for &e in spatial.get(from.0 + dx, from.1 + dy) {
                if item_check.get(e).is_ok() {
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((e, (from.0 + dx) as i16, (from.1 + dy) as i16));
                    }
                }
            }
        }
    }
    best
}

pub fn find_nearest_unplanted_farmland(
    chunk_map: &ChunkMap,
    plant_map: &PlantMap,
    from: (i32, i32),
    radius: i32,
) -> Option<(i16, i16)> {
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if plant_map.0.contains_key(&(tx, ty)) { continue; }
            if chunk_map.tile_kind_at(tx, ty) == Some(TileKind::Farmland) {
                let dist = dx.abs() + dy.abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx as i16, ty as i16));
                }
            }
        }
    }
    best
}

pub fn assign_job_with_routing(
    ai: &mut PersonAI,
    cur_chunk: ChunkCoord,
    target: (i16, i16),
    job: JobKind,
    chunk_graph: &ChunkGraph,
    chunk_map: &ChunkMap,
) {
    let dest_chunk = ChunkCoord(
        (target.0 as i32).div_euclid(CHUNK_SIZE as i32),
        (target.1 as i32).div_euclid(CHUNK_SIZE as i32),
    );
    ai.job_id = job as u16;
    if dest_chunk == cur_chunk {
        ai.state = AiState::Seeking;
        ai.target_tile = target;
    } else if let Some(wp) = chunk_graph.next_waypoint(cur_chunk, dest_chunk, chunk_map) {
        ai.state = AiState::Routing;
        ai.target_tile = wp;
    } else {
        ai.state = AiState::Seeking;
        ai.target_tile = target;
    }
}

/// Find nearest entity of opposite sex in same faction within `radius` tiles.
/// Prefers partners with higher relationship affinity.
fn find_nearby_partner(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    my_sex: BiologicalSex,
    my_faction: u32,
    sex_query: &Query<(&BiologicalSex, &FactionMember)>,
    self_entity: Entity,
    rel: Option<&RelationshipMemory>,
) -> Option<(i16, i16)> {
    let mut best_pos = None;
    let mut best_affinity = i8::MIN;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            for &other in spatial.get(from.0 + dx, from.1 + dy) {
                if other == self_entity { continue; }
                if let Ok((other_sex, other_fm)) = sex_query.get(other) {
                    if *other_sex != my_sex && other_fm.faction_id == my_faction {
                        let affinity = rel.map(|r| r.get_affinity(other)).unwrap_or(0);
                        if best_pos.is_none() || affinity > best_affinity {
                            best_affinity = affinity;
                            best_pos = Some(((from.0 + dx) as i16, (from.1 + dy) as i16));
                        }
                    }
                }
            }
        }
    }
    best_pos
}

/// Handles goals that don't use the plan system:
/// Reproduce, Socialize, ReturnCamp, Raid, Defend.
pub fn goal_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial: Res<SpatialIndex>,
    faction_registry: Res<FactionRegistry>,
    sex_query: Query<(&BiologicalSex, &FactionMember)>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &EconomicAgent,
        &AgentGoal,
        &FactionMember,
        &BiologicalSex,
        &Transform,
        &LodLevel,
        Option<&RelationshipMemory>,
    ), Without<PlayerOrder>>,
) {
    query.par_iter_mut().for_each(|(
        entity, mut ai, _agent, goal, member, sex, transform, lod, rel_opt,
    )| {
        if *lod == LodLevel::Dormant {
            return;
        }
        if matches!(ai.state, AiState::Working | AiState::Seeking | AiState::Routing) {
            return;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        match goal {
            AgentGoal::ReturnCamp => {
                if member.faction_id != SOLO {
                    if let Some(home) = faction_registry.home_tile(member.faction_id) {
                        assign_job_with_routing(&mut ai, cur_chunk, home, JobKind::Idle, &chunk_graph, &chunk_map);
                    }
                }
            }

            AgentGoal::Socialize => {
                let radius = 15i32;
                'find: for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let tx = cur_tx + dx;
                        let ty = cur_ty + dy;
                        for &other in spatial.get(tx, ty) {
                            if other == entity { continue; }
                            assign_job_with_routing(
                                &mut ai, cur_chunk,
                                (tx as i16, ty as i16),
                                JobKind::Idle,
                                &chunk_graph, &chunk_map,
                            );
                            break 'find;
                        }
                    }
                }
            }

            AgentGoal::Reproduce => {
                if member.faction_id != SOLO {
                    if let Some(target) = find_nearby_partner(
                        &spatial, (cur_tx, cur_ty), 15, *sex,
                        member.faction_id, &sex_query, entity, rel_opt,
                    ) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Idle, &chunk_graph, &chunk_map);
                    }
                }
            }

            AgentGoal::Raid => {
                if member.faction_id != SOLO {
                    if let Some(target_id) = faction_registry.raid_target(member.faction_id) {
                        if let Some(enemy_home) = faction_registry.home_tile(target_id) {
                            assign_job_with_routing(
                                &mut ai, cur_chunk, enemy_home,
                                JobKind::Raid, &chunk_graph, &chunk_map,
                            );
                        }
                    }
                }
            }

            AgentGoal::Defend => {
                if member.faction_id != SOLO {
                    if let Some(home) = faction_registry.home_tile(member.faction_id) {
                        assign_job_with_routing(
                            &mut ai, cur_chunk, home,
                            JobKind::Defend, &chunk_graph, &chunk_map,
                        );
                    }
                }
            }

            AgentGoal::Sleep => {
                ai.state = AiState::Sleeping;
            }

            // Survive and Gather are handled by plan_execution_system
            AgentGoal::Survive | AgentGoal::Gather => {}
        }
    });
}
