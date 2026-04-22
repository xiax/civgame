use bevy::prelude::*;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::tile::TileKind;
use crate::world::terrain::{WORLD_CHUNKS_X, WORLD_CHUNKS_Y, TILE_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::simulation::plants::PlantMap;
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::goals::AgentGoal;
use super::items::GroundItem;
use super::needs::Needs;
use super::person::{AiState, PersonAI};
use super::lod::LodLevel;
use super::reproduction::BiologicalSex;
use super::skills::{SkillKind, Skills};

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Idle       = 0,
    Farmer     = 1,
    Woodcutter = 2,
    Miner      = 3,
    Trader     = 4,
    Forager    = 5,
    Raid       = 6,
    Defend     = 7,
    Planter    = 8,
}

fn find_nearest_tile(
    chunk_map: &ChunkMap,
    from: (i32, i32),
    radius: i32,
    kinds: &[TileKind],
) -> Option<(i16, i16)> {
    let total_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = (from.0 + dx).clamp(0, total_x - 1);
            let ty = (from.1 + dy).clamp(0, total_y - 1);
            if let Some(tile) = chunk_map.tile_at(tx, ty) {
                if kinds.contains(&tile.kind) {
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

fn find_nearest_item(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    item_check: &Query<(), With<GroundItem>>,
) -> Option<(i16, i16)> {
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            for &e in spatial.get(from.0 + dx, from.1 + dy) {
                if item_check.get(e).is_ok() {
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some(((from.0 + dx) as i16, (from.1 + dy) as i16));
                    }
                }
            }
        }
    }
    best
}

fn find_nearest_unplanted_farmland(
    chunk_map: &ChunkMap,
    plant_map: &PlantMap,
    from: (i32, i32),
    radius: i32,
) -> Option<(i16, i16)> {
    let total_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = (from.0 + dx).clamp(0, total_x - 1);
            let ty = (from.1 + dy).clamp(0, total_y - 1);
            if plant_map.0.contains_key(&(tx, ty)) { continue; }
            if let Some(tile) = chunk_map.tile_at(tx, ty) {
                if tile.kind == TileKind::Farmland {
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

fn tile_coord(transform: &Transform) -> (i32, i32) {
    let pos = transform.translation.truncate();
    (
        (pos.x / TILE_SIZE).floor() as i32,
        (pos.y / TILE_SIZE).floor() as i32,
    )
}

fn chunk_of(tx: i32, ty: i32) -> ChunkCoord {
    ChunkCoord(
        tx.div_euclid(CHUNK_SIZE as i32),
        ty.div_euclid(CHUNK_SIZE as i32),
    )
}

fn assign_job_with_routing(
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
fn find_nearby_partner(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    my_sex: BiologicalSex,
    my_faction: u32,
    sex_query: &Query<(&BiologicalSex, &FactionMember)>,
    self_entity: Entity,
) -> Option<(i16, i16)> {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            for &other in spatial.get(from.0 + dx, from.1 + dy) {
                if other == self_entity { continue; }
                if let Ok((other_sex, other_fm)) = sex_query.get(other) {
                    if *other_sex != my_sex && other_fm.faction_id == my_faction {
                        return Some(((from.0 + dx) as i16, (from.1 + dy) as i16));
                    }
                }
            }
        }
    }
    None
}

pub fn job_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial: Res<SpatialIndex>,
    plant_map: Res<PlantMap>,
    faction_registry: Res<FactionRegistry>,
    sex_query: Query<(&BiologicalSex, &FactionMember)>,
    item_check: Query<(), With<GroundItem>>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &Needs,
        &EconomicAgent,
        &AgentGoal,
        &FactionMember,
        &BiologicalSex,
        &Transform,
        &LodLevel,
        &Skills,
    )>,
) {
    query.par_iter_mut().for_each(|(
        entity, mut ai, needs, agent, goal, member, sex, transform, lod, skills
    )| {
        if *lod == LodLevel::Dormant {
            return;
        }
        if matches!(ai.state, AiState::Working | AiState::Seeking | AiState::Routing) {
            return;
        }

        let (cur_tx, cur_ty) = tile_coord(transform);
        let cur_chunk = chunk_of(cur_tx, cur_ty);

        match goal {
            // ── Survive: find food ────────────────────────────────────────────
            AgentGoal::Survive => {
                // Go to camp if faction has food
                if member.faction_id != SOLO {
                    if let Some(home) = faction_registry.home_tile(member.faction_id) {
                        if faction_registry.food_stock(member.faction_id) > 0.0 {
                            ai.job_id = JobKind::Forager as u16;
                            assign_job_with_routing(&mut ai, cur_chunk, home, JobKind::Forager, &chunk_graph, &chunk_map);
                            ai.job_id = JobKind::Forager as u16;
                            return;
                        }
                    }
                }
                // Grab nearby dropped food first
                if let Some(target) = find_nearest_item(&spatial, (cur_tx, cur_ty), 10, &item_check) {
                    assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Forager, &chunk_graph, &chunk_map);
                    return;
                }
                // Forage food
                if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 20, &[TileKind::Farmland]) {
                    assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Farmer, &chunk_graph, &chunk_map);
                    return;
                }
                if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 10, &[TileKind::Grass]) {
                    assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Forager, &chunk_graph, &chunk_map);
                }
            }

            // ── ReturnCamp: carry food to faction home tile ───────────────────
            AgentGoal::ReturnCamp => {
                if member.faction_id != SOLO {
                    if let Some(home) = faction_registry.home_tile(member.faction_id) {
                        assign_job_with_routing(&mut ai, cur_chunk, home, JobKind::Forager, &chunk_graph, &chunk_map);
                    }
                }
            }

            // ── Socialize: seek nearby agent / faction member ─────────────────
            AgentGoal::Socialize => {
                // Look for any adjacent agent to walk toward
                let radius = 15i32;
                'find: for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let tx = cur_tx + dx;
                        let ty = cur_ty + dy;
                        for &other in spatial.get(tx, ty) {
                            if other == entity { continue; }
                            ai.job_id = JobKind::Idle as u16;
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

            // ── Reproduce: seek opposite-sex faction partner ──────────────────
            AgentGoal::Reproduce => {
                if member.faction_id != SOLO {
                    if let Some(target) = find_nearby_partner(
                        &spatial, (cur_tx, cur_ty), 15, *sex,
                        member.faction_id, &sex_query, entity,
                    ) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Idle, &chunk_graph, &chunk_map);
                    }
                }
            }

            // ── Raid: route to enemy camp ─────────────────────────────────────
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

            // ── Defend: rally to own camp ─────────────────────────────────────
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

            // ── Gather: collect resources ─────────────────────────────────────
            AgentGoal::Gather => {
                if needs.hunger <= 100 && agent.has_tool() {
                    if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 30, &[TileKind::Forest]) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Woodcutter, &chunk_graph, &chunk_map);
                        return;
                    }
                    if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 30, &[TileKind::Stone]) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Miner, &chunk_graph, &chunk_map);
                        return;
                    }
                }
                // Plant seeds if agent has seeds and enough farming skill
                if agent.quantity_of(Good::Seed) > 0 && skills.get(SkillKind::Farming) >= 15 {
                    if let Some(target) = find_nearest_unplanted_farmland(&chunk_map, &plant_map, (cur_tx, cur_ty), 15) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Planter, &chunk_graph, &chunk_map);
                        return;
                    }
                }
                if needs.hunger > 80 {
                    if let Some(target) = find_nearest_item(&spatial, (cur_tx, cur_ty), 10, &item_check) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Forager, &chunk_graph, &chunk_map);
                        return;
                    }
                    if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 20, &[TileKind::Farmland]) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Farmer, &chunk_graph, &chunk_map);
                        return;
                    }
                }
                if !agent.is_inventory_full() {
                    if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 10, &[TileKind::Grass]) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Forager, &chunk_graph, &chunk_map);
                        return;
                    }
                    if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 10, &[TileKind::Forest]) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Forager, &chunk_graph, &chunk_map);
                        return;
                    }
                    if let Some(target) = find_nearest_tile(&chunk_map, (cur_tx, cur_ty), 10, &[TileKind::Stone]) {
                        assign_job_with_routing(&mut ai, cur_chunk, target, JobKind::Forager, &chunk_graph, &chunk_map);
                    }
                }
            }
        }
    });
}
