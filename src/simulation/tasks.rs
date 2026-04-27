use super::construction::{Bed, HomeBed};
use super::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::goals::AgentGoal;
use super::items::GroundItem;
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::PlayerOrder;
use super::person::{AiState, PersonAI};
use super::plan::ActivePlan;
use super::plants::{GrowthStage, Plant, PlantKind, PlantMap};
use super::reproduction::BiologicalSex;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;
use bevy::prelude::*;

/// Represents the current active task an agent is performing.
/// Tasks are transient and managed by either the plan system or the goal dispatch system.
/// An agent is "unemployed" when they are between tasks or idling.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Idle = 0,
    Gather = 1,
    Trader = 2,
    Raid = 3,
    Defend = 4,
    Planter = 5,
    Hunter = 6,
    Scavenge = 7,
    Construct = 8,         // build wall tile
    ConstructBed = 9,      // spawn bed entity
    DepositResource = 10,  // return to camp and deposit goods
    Socialize = 11,
    Reproduce = 12,
    Explore = 13,
    Dig = 14, // dig down at surface or mine a wall tile
    Sleep = 15,
    Eat = 16,          // consume one food item from inventory over several ticks
    WithdrawFood = 17, // pull one food item from a faction storage tile into inventory
}

/// Returns true for tasks where the agent works from an adjacent tile rather than
/// stepping onto the resource tile itself.
pub fn task_interacts_from_adjacent(task_id: u16) -> bool {
    task_id == TaskKind::Gather as u16
        || task_id == TaskKind::Dig as u16
        || task_id == TaskKind::Planter as u16
        || task_id == TaskKind::Construct as u16
        || task_id == TaskKind::ConstructBed as u16
        || task_id == TaskKind::DepositResource as u16
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
) -> Option<(Entity, i16, i16)> {
    let mut best: Option<(Entity, i16, i16)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if let Some(&entity) = plant_map.0.get(&(tx, ty)) {
                if let Ok(plant) = plant_query.get(entity) {
                    if mature_only && plant.stage != GrowthStage::Mature {
                        continue;
                    }
                    if let Some(k) = kind_filter {
                        if plant.kind != k {
                            continue;
                        }
                    }
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((entity, tx as i16, ty as i16));
                    }
                }
            }
        }
    }
    best
}

// Bug 2 fix: filter by `good` so agents don't target the wrong item type.
pub fn find_nearest_edible(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    item_query: &Query<&GroundItem>,
    storage_tile_map: &StorageTileMap,
    is_gathering: bool,
) -> Option<(Entity, i16, i16)> {
    let mut best: Option<(Entity, i16, i16)> = None;
    let mut best_dist = i32::MAX;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;

            // Prevent gathering agents from scavaging faction storage tiles
            if is_gathering && storage_tile_map.tiles.contains_key(&(tx as i16, ty as i16)) {
                continue;
            }

            for &e in spatial.get(tx, ty) {
                if let Ok(item) = item_query.get(e) {
                    if item.item.good.is_edible() {
                        let dist = dx.abs() + dy.abs();
                        if dist < best_dist {
                            best_dist = dist;
                            best = Some((e, tx as i16, ty as i16));
                        }
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
    good: Good,
    item_query: &Query<&GroundItem>,
    storage_tile_map: &StorageTileMap,
    is_gathering: bool,
) -> Option<(Entity, i16, i16)> {
    let mut best: Option<(Entity, i16, i16)> = None;
    let mut best_dist = i32::MAX;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;

            // Prevent gathering agents from scavaging faction storage tiles
            if is_gathering && storage_tile_map.tiles.contains_key(&(tx as i16, ty as i16)) {
                continue;
            }

            for &e in spatial.get(tx, ty) {
                if let Ok(item) = item_query.get(e) {
                    if item.item.good == good {
                        let dist = dx.abs() + dy.abs();
                        if dist < best_dist {
                            best_dist = dist;
                            best = Some((e, tx as i16, ty as i16));
                        }
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
            if plant_map.0.contains_key(&(tx, ty)) {
                continue;
            }
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

pub fn assign_task_with_routing(
    ai: &mut PersonAI,
    cur_tile: (i16, i16),
    cur_chunk: ChunkCoord,
    target: (i16, i16),
    task: TaskKind,
    target_entity: Option<Entity>,
    chunk_graph: &ChunkGraph,
    chunk_map: &ChunkMap,
) {
    ai.task_id = task as u16;
    ai.dest_tile = target;
    ai.target_entity = target_entity;
    ai.target_z = chunk_map.surface_z_at(target.0 as i32, target.1 as i32) as i8;

    // For tasks where the agent works from beside the target (not on it), route to
    // the nearest passable adjacent tile so the agent never steps onto the resource.
    let route_target = if task_interacts_from_adjacent(task as u16) {
        let (tx, ty) = (target.0 as i32, target.1 as i32);
        let (ax, ay) = (cur_tile.0 as i32, cur_tile.1 as i32);
        const ADJ: [(i32, i32); 8] = [(-1,0),(1,0),(0,-1),(0,1),(-1,-1),(1,-1),(-1,1),(1,1)];
        ADJ.iter()
            .map(|&(dx, dy)| (tx + dx, ty + dy))
            .filter(|&(ntx, nty)| {
                let nz = chunk_map.surface_z_at(ntx, nty);
                chunk_map.passable_at(ntx, nty, nz)
            })
            .min_by_key(|&(ntx, nty)| (ntx - ax).abs() + (nty - ay).abs())
            .map(|(ntx, nty)| (ntx as i16, nty as i16))
            .unwrap_or(target)
    } else {
        target
    };

    let route_chunk = ChunkCoord(
        (route_target.0 as i32).div_euclid(CHUNK_SIZE as i32),
        (route_target.1 as i32).div_euclid(CHUNK_SIZE as i32),
    );
    if route_chunk == cur_chunk {
        ai.state = AiState::Seeking;
        ai.target_tile = route_target;
    } else if let Some(wp) =
        chunk_graph.next_waypoint(cur_chunk, route_chunk, ai.current_z, chunk_map)
    {
        ai.state = AiState::Routing;
        ai.target_tile = wp;
    } else {
        ai.state = AiState::Seeking;
        ai.target_tile = route_target;
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
) -> Option<(Entity, i16, i16)> {
    let mut best_res = None;
    let mut best_affinity = i8::MIN;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            for &other in spatial.get(from.0 + dx, from.1 + dy) {
                if other == self_entity {
                    continue;
                }
                if let Ok((other_sex, other_fm)) = sex_query.get(other) {
                    if *other_sex != my_sex && other_fm.faction_id == my_faction {
                        let affinity = rel.map(|r| r.get_affinity(other)).unwrap_or(0);
                        if best_res.is_none() || affinity > best_affinity {
                            best_affinity = affinity;
                            best_res = Some((other, (from.0 + dx) as i16, (from.1 + dy) as i16));
                        }
                    }
                }
            }
        }
    }
    best_res
}

/// Handles goals that don't use the plan system:
/// Reproduce, Socialize, ReturnCamp, Raid, Defend, Sleep.
pub fn goal_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial: Res<SpatialIndex>,
    faction_registry: Res<FactionRegistry>,
    storage_tile_map: Res<StorageTileMap>,
    sex_query: Query<(&BiologicalSex, &FactionMember)>,
    bed_query: Query<&Transform, With<Bed>>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &EconomicAgent,
            &Needs,
            &AgentGoal,
            &FactionMember,
            &BiologicalSex,
            &Transform,
            &LodLevel,
            Option<&RelationshipMemory>,
            Option<&ActivePlan>,
            Option<&HomeBed>,
        ),
        Without<PlayerOrder>,
    >,
) {
    query.par_iter_mut().for_each(
        |(
            entity,
            mut ai,
            agent,
            needs,
            goal,
            member,
            sex,
            transform,
            lod,
            rel_opt,
            plan_opt,
            home_bed_opt,
        )| {
            if *lod == LodLevel::Dormant {
                return;
            }

            if plan_opt.is_none() && ai.task_id != PersonAI::UNEMPLOYED {
                let expected_task = match goal {
                    AgentGoal::ReturnCamp => Some(TaskKind::DepositResource as u16),
                    AgentGoal::Socialize => Some(TaskKind::Socialize as u16),
                    AgentGoal::Reproduce => Some(TaskKind::Reproduce as u16),
                    AgentGoal::Raid => Some(TaskKind::Raid as u16),
                    AgentGoal::Defend => Some(TaskKind::Defend as u16),
                    AgentGoal::Sleep => Some(TaskKind::Sleep as u16),
                    AgentGoal::Survive if ai.task_id == TaskKind::Eat as u16 => {
                        Some(TaskKind::Eat as u16)
                    }
                    _ => {
                        if ai.task_id == TaskKind::Explore as u16
                            && matches!(
                                goal,
                                AgentGoal::GatherFood
                                    | AgentGoal::GatherWood
                                    | AgentGoal::GatherStone
                                    | AgentGoal::Survive
                                    | AgentGoal::Build
                            )
                        {
                            Some(TaskKind::Explore as u16)
                        } else {
                            None
                        }
                    }
                };

                if Some(ai.task_id) != expected_task {
                    // Goal changed or task is done; drop the current task.
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
            }

            let is_active = matches!(
                ai.state,
                AiState::Working | AiState::Seeking | AiState::Routing
            );

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            match goal {
                AgentGoal::ReturnCamp => {
                    if member.faction_id != SOLO {
                        if let Some(storage_tile) = storage_tile_map
                            .nearest_for_faction(member.faction_id, (cur_tx, cur_ty))
                        {
                            // Already heading to a storage tile with the deposit task — don't reassign.
                            if is_active
                                && ai.task_id == TaskKind::DepositResource as u16
                                && ai.dest_tile == storage_tile
                            {
                                return;
                            }
                            // Only dispatch when there's something to do: food to deposit, or
                            // agent is starving and needs to withdraw from faction stock.
                            let is_starving =
                                needs.hunger > 120.0 && agent.total_food() == 0;
                            if !is_starving && agent.total_food() == 0 {
                                return;
                            }
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx as i16, cur_ty as i16),
                                cur_chunk,
                                storage_tile,
                                TaskKind::DepositResource,
                                None,
                                &chunk_graph,
                                &chunk_map,
                            );
                        }
                    }
                }

                AgentGoal::Socialize => {
                    if is_active && ai.task_id == TaskKind::Socialize as u16 {
                        return;
                    }
                    // Prefer liked agents over merely nearest: score = affinity*3 - dist.
                    // Degrades to pure distance when affinity is zero for all candidates.
                    let radius = 15i32;
                    let mut best_target: Option<(i16, i16, Entity)> = None;
                    let mut best_score = i32::MIN;
                    for dy in -radius..=radius {
                        for dx in -radius..=radius {
                            let tx = cur_tx + dx;
                            let ty = cur_ty + dy;
                            for &other in spatial.get(tx, ty) {
                                if other == entity {
                                    continue;
                                }
                                let dist = dx.abs() + dy.abs();
                                let affinity = rel_opt
                                    .map(|r| r.get_affinity(other) as i32)
                                    .unwrap_or(0);
                                let score = affinity * 3 - dist;
                                if score > best_score {
                                    best_score = score;
                                    best_target = Some((tx as i16, ty as i16, other));
                                }
                            }
                        }
                    }
                    if let Some((tx, ty, other)) = best_target {
                        assign_task_with_routing(
                            &mut ai,
                            (cur_tx as i16, cur_ty as i16),
                            cur_chunk,
                            (tx, ty),
                            TaskKind::Socialize,
                            Some(other),
                            &chunk_graph,
                            &chunk_map,
                        );
                    }
                }

                AgentGoal::Reproduce => {
                    if is_active && ai.task_id == TaskKind::Reproduce as u16 {
                        return;
                    }
                    if member.faction_id != SOLO {
                        if let Some((partner, tx, ty)) = find_nearby_partner(
                            &spatial,
                            (cur_tx, cur_ty),
                            15,
                            *sex,
                            member.faction_id,
                            &sex_query,
                            entity,
                            rel_opt,
                        ) {
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx as i16, cur_ty as i16),
                                cur_chunk,
                                (tx, ty),
                                TaskKind::Reproduce,
                                Some(partner),
                                &chunk_graph,
                                &chunk_map,
                            );
                        }
                    }
                }

                AgentGoal::Raid => {
                    if is_active && ai.task_id == TaskKind::Raid as u16 {
                        return;
                    }
                    if member.faction_id != SOLO {
                        if let Some(target_id) = faction_registry.raid_target(member.faction_id) {
                            if let Some(enemy_home) = faction_registry.home_tile(target_id) {
                                assign_task_with_routing(
                                    &mut ai,
                                    (cur_tx as i16, cur_ty as i16),
                                    cur_chunk,
                                    enemy_home,
                                    TaskKind::Raid,
                                    None,
                                    &chunk_graph,
                                    &chunk_map,
                                );
                            }
                        }
                    }
                }

                AgentGoal::Defend => {
                    if is_active && ai.task_id == TaskKind::Defend as u16 {
                        return;
                    }
                    if member.faction_id != SOLO {
                        if let Some(home) = faction_registry.home_tile(member.faction_id) {
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx as i16, cur_ty as i16),
                                cur_chunk,
                                home,
                                TaskKind::Defend,
                                None,
                                &chunk_graph,
                                &chunk_map,
                            );
                        }
                    }
                }

                AgentGoal::Sleep => {
                    if ai.state == AiState::Sleeping {
                        return;
                    }

                    // If arrived at "working" tile for Sleep task, start sleeping
                    if ai.state == AiState::Working && ai.task_id == TaskKind::Sleep as u16 {
                        ai.state = AiState::Sleeping;
                        return;
                    }

                    if is_active && ai.task_id == TaskKind::Sleep as u16 {
                        return;
                    }

                    // 1) Persistent claim: route to my own bed if it still exists.
                    if let Some(bed_entity) = home_bed_opt.and_then(|h| h.0) {
                        if let Ok(bed_transform) = bed_query.get(bed_entity) {
                            let btx = (bed_transform.translation.x / TILE_SIZE).floor() as i16;
                            let bty = (bed_transform.translation.y / TILE_SIZE).floor() as i16;
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx as i16, cur_ty as i16),
                                cur_chunk,
                                (btx, bty),
                                TaskKind::Sleep,
                                Some(bed_entity),
                                &chunk_graph,
                                &chunk_map,
                            );
                            return;
                        }
                    }

                    // 2) No claim yet (or stale): head toward faction home so the
                    //    next assign_beds_system pass can pair us with a free bed.
                    //    Sleep on the ground there until that happens.
                    let home_opt = if member.faction_id != SOLO {
                        faction_registry.home_tile(member.faction_id)
                    } else {
                        None
                    };

                    if let Some(home) = home_opt {
                        let dx = cur_tx - home.0 as i32;
                        let dy = cur_ty - home.1 as i32;
                        if dx * dx + dy * dy > 5 * 5 {
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx as i16, cur_ty as i16),
                                cur_chunk,
                                home,
                                TaskKind::Sleep,
                                None,
                                &chunk_graph,
                                &chunk_map,
                            );
                            return;
                        }
                    }

                    // Solo, no home, or already at home with no bed yet: sleep here.
                    ai.state = AiState::Sleeping;
                    ai.task_id = TaskKind::Sleep as u16;
                }

                AgentGoal::Survive => {
                    // Self-trigger Eat when the agent already has food on hand —
                    // skip the plan system entirely.
                    if needs.hunger > 120.0 && agent.total_food() > 0 {
                        if is_active && ai.task_id == TaskKind::Eat as u16 {
                            return;
                        }
                        let cur_tile = (cur_tx as i16, cur_ty as i16);
                        ai.task_id = TaskKind::Eat as u16;
                        ai.state = AiState::Working;
                        ai.target_tile = cur_tile;
                        ai.dest_tile = cur_tile;
                        ai.work_progress = 0;
                        ai.target_entity = None;
                        return;
                    }
                    if ai.task_id == TaskKind::Explore as u16 && ai.state == AiState::Working {
                        ai.state = AiState::Idle;
                        ai.task_id = PersonAI::UNEMPLOYED;
                    }
                }

                // Gather and Build are handled by plan_execution_system
                AgentGoal::GatherFood
                | AgentGoal::GatherWood
                | AgentGoal::GatherStone
                | AgentGoal::Build => {
                    if ai.task_id == TaskKind::Explore as u16 {
                        if ai.state == AiState::Working {
                            ai.state = AiState::Idle;
                            ai.task_id = PersonAI::UNEMPLOYED;
                        }
                    }
                }
            }
        },
    );
}
