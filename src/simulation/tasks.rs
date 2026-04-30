use super::construction::{Bed, HomeBed};
use super::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::goals::AgentGoal;
use super::items::GroundItem;
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::PlayerOrder;
use super::person::{AiState, Drafted, PersonAI};
use super::plan::ActivePlan;
use super::plants::{GrowthStage, Plant, PlantKind, PlantMap};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
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
    TameAnimal = 18,  // work adjacent to a wild horse for ~100 ticks to tame it
    Craft = 19,       // craft an item in-place using inventory ingredients
    Deconstruct = 20, // dismantle placed furniture (e.g. bed) and carry recovered wood to storage
    Lead = 21,        // tribal chief stations at faction home and issues build orders
    Terraform = 22,   // level a footprint tile to a target Z (dig down or fill up by one Z step)
    HaulMaterials = 23, // carry inventory goods to a blueprint and drop them into its deposit slots
    MilitaryMove = 24,  // drafted unit walking to a player-issued destination, idles on arrival
    MilitaryAttack = 25, // drafted unit chasing a target entity to attack it adjacent
}

/// How many free hands the task requires the agent to have before they can begin
/// (or continue) work. Hauling and gathering tasks are EXEMPT — the load is the
/// whole point. Tasks like Sleep/Socialize/Reproduce return 0 because they don't
/// "use" hands, but we drop hand-held loads at task-entry separately (see
/// `goal_dispatch_system`).
pub fn task_requires_free_hands(task_id: u16) -> u8 {
    match task_id {
        x if x == TaskKind::Craft as u16 => 2,
        x if x == TaskKind::Construct as u16
            || x == TaskKind::ConstructBed as u16
            || x == TaskKind::Dig as u16
            || x == TaskKind::Terraform as u16
            || x == TaskKind::Deconstruct as u16
            || x == TaskKind::Gather as u16
            || x == TaskKind::Planter as u16
            || x == TaskKind::Hunter as u16
            || x == TaskKind::Raid as u16
            || x == TaskKind::Defend as u16
            || x == TaskKind::MilitaryAttack as u16
            || x == TaskKind::TameAnimal as u16 =>
        {
            1
        }
        _ => 0,
    }
}

/// Tasks that should drop hand-held loads at entry (the activity is incompatible
/// with carrying things). Stacks become GroundItems at the agent's tile.
pub fn task_drops_hand_load(task_id: u16) -> bool {
    task_id == TaskKind::Sleep as u16
        || task_id == TaskKind::Socialize as u16
        || task_id == TaskKind::Reproduce as u16
        || task_id == TaskKind::Eat as u16
}

/// Returns true for tasks where the agent works from an adjacent tile rather than
/// stepping onto the resource tile itself.
pub fn task_interacts_from_adjacent(task_id: u16) -> bool {
    task_id == TaskKind::Gather as u16
        || task_id == TaskKind::Dig as u16
        || task_id == TaskKind::Planter as u16
        || task_id == TaskKind::Construct as u16
        || task_id == TaskKind::ConstructBed as u16
        || task_id == TaskKind::HaulMaterials as u16
        || task_id == TaskKind::DepositResource as u16
        || task_id == TaskKind::TameAnimal as u16
        || task_id == TaskKind::Deconstruct as u16
        || task_id == TaskKind::Reproduce as u16
        || task_id == TaskKind::Terraform as u16
        || task_id == TaskKind::Scavenge as u16
        || task_id == TaskKind::WithdrawFood as u16
        || task_id == TaskKind::Socialize as u16
        || task_id == TaskKind::Raid as u16
        || task_id == TaskKind::Defend as u16
        || task_id == TaskKind::Lead as u16
        || task_id == TaskKind::MilitaryAttack as u16
}

/// Spiral search outward from `target` for the closest tile that is passable
/// at its surface Z and reachable (via `ChunkConnectivity`) from
/// `agent_origin`. Used as a "wander toward target" fallback when the strict
/// adjacency pick in `assign_task_with_routing` finds no usable tile — the
/// agent walks toward the goal so the next dispatch tick can retry adjacency
/// from a closer position.
pub fn nearest_reachable_tile_near(
    chunk_map: &ChunkMap,
    chunk_connectivity: &ChunkConnectivity,
    target: (i32, i32),
    agent_origin: (ChunkCoord, i8),
    radius: i32,
) -> Option<(i16, i16)> {
    let csz = CHUNK_SIZE as i32;
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;
    for r in 1..=radius {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // ring-only iteration
                }
                let tx = target.0 + dx;
                let ty = target.1 + dy;
                let tz = chunk_map.surface_z_at(tx, ty);
                if !chunk_map.passable_at(tx, ty, tz) {
                    continue;
                }
                let n_chunk = ChunkCoord(tx.div_euclid(csz), ty.div_euclid(csz));
                if !chunk_connectivity.is_reachable(agent_origin, (n_chunk, tz as i8)) {
                    continue;
                }
                let dist = dx.abs() + dy.abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx as i16, ty as i16));
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    best
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
    chunk_router: &ChunkRouter,
    chunk_map: &ChunkMap,
    chunk_connectivity: &ChunkConnectivity,
) -> bool {
    // Resource's standable Z — used to gate which adjacent tiles can actually
    // reach the work tile. The agent's `target_z` is set below to the route
    // tile's Z (where they'll stand), not this — otherwise flow-field seeds
    // and Working-transition checks fire at the wrong Z on sloped ground.
    let resource_z = chunk_map.nearest_standable_z(
        target.0 as i32,
        target.1 as i32,
        ai.current_z as i32,
    ) as i8;

    // For tasks where the agent works from beside the target (not on it), route to
    // the nearest passable adjacent tile within ±1 Z of the resource so the agent
    // can actually interact once they arrive. Also require the candidate tile to
    // share a connectivity component with the agent — otherwise the path worker
    // will reject every request to that tile and the agent will loop forever on
    // the same unreachable resource.
    let route_target = if task_interacts_from_adjacent(task as u16) {
        let (tx, ty) = (target.0 as i32, target.1 as i32);
        let (ax, ay) = (cur_tile.0 as i32, cur_tile.1 as i32);
        const ADJ: [(i32, i32); 8] = [(-1,0),(1,0),(0,-1),(0,1),(-1,-1),(1,-1),(-1,1),(1,1)];
        let agent_z = ai.current_z;
        let csz = CHUNK_SIZE as i32;
        let picked = ADJ
            .iter()
            .map(|&(dx, dy)| (tx + dx, ty + dy))
            .filter(|&(ntx, nty)| {
                let nz = chunk_map.surface_z_at(ntx, nty);
                if !chunk_map.passable_at(ntx, nty, nz) {
                    return false;
                }
                let dz = nz as i32 - resource_z as i32;
                if !(-1..=1).contains(&dz) {
                    return false;
                }
                let n_chunk = ChunkCoord(ntx.div_euclid(csz), nty.div_euclid(csz));
                chunk_connectivity.is_reachable((cur_chunk, agent_z), (n_chunk, nz as i8))
            })
            .min_by_key(|&(ntx, nty)| (ntx - ax).abs() + (nty - ay).abs())
            .map(|(ntx, nty)| (ntx as i16, nty as i16));
        match picked {
            Some(t) => t,
            None => {
                // No reachable adjacent tile (camp perimeter blocked, target
                // sealed in by walls / blueprints, etc.). Fall back to the
                // nearest passable, reachable tile in a small radius around
                // the target — agent walks toward it and the next dispatch
                // tick re-tries adjacency from a closer position. Without
                // this fallback the agent loops Idle forever on a goal whose
                // adjacency happens to be temporarily blocked.
                match nearest_reachable_tile_near(
                    chunk_map,
                    chunk_connectivity,
                    (tx, ty),
                    (cur_chunk, agent_z),
                    8,
                ) {
                    Some(t) => t,
                    None => {
                        // Truly unreachable from this connectivity component.
                        // Clear target so the inspector reflects reality and
                        // the caller can mark the goal failed.
                        ai.target_tile = cur_tile;
                        ai.dest_tile = cur_tile;
                        ai.target_entity = None;
                        return false;
                    }
                }
            }
        }
    } else {
        target
    };

    ai.task_id = task as u16;
    ai.dest_tile = target;
    ai.target_entity = target_entity;

    ai.target_z = chunk_map.nearest_standable_z(
        route_target.0 as i32,
        route_target.1 as i32,
        ai.current_z as i32,
    ) as i8;

    let route_chunk = ChunkCoord(
        (route_target.0 as i32).div_euclid(CHUNK_SIZE as i32),
        (route_target.1 as i32).div_euclid(CHUNK_SIZE as i32),
    );
    if route_chunk == cur_chunk {
        ai.state = AiState::Seeking;
        ai.target_tile = route_target;
    } else if let Some(wp) =
        chunk_router.first_waypoint(chunk_graph, cur_chunk, route_chunk, ai.current_z)
    {
        ai.state = AiState::Routing;
        ai.target_tile = wp;
    } else {
        ai.state = AiState::Seeking;
        ai.target_tile = route_target;
    }
    true
}

/// Handles goals that don't yet use the plan system:
/// Socialize, Raid, Defend, Sleep, Lead.
pub fn goal_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    spatial: Res<SpatialIndex>,
    faction_registry: Res<FactionRegistry>,
    bed_query: Query<&Transform, With<Bed>>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &EconomicAgent,
            &Needs,
            &mut AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            Option<&RelationshipMemory>,
            Option<&ActivePlan>,
            Option<&HomeBed>,
        ),
        (Without<PlayerOrder>, Without<Drafted>),
    >,
) {
    query.par_iter_mut().for_each(
        |(
            entity,
            mut ai,
            agent,
            needs,
            mut goal,
            member,
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
                let expected_task = match *goal {
                    AgentGoal::Socialize => Some(TaskKind::Socialize as u16),
                    AgentGoal::Raid => Some(TaskKind::Raid as u16),
                    AgentGoal::Defend => Some(TaskKind::Defend as u16),
                    AgentGoal::Sleep => Some(TaskKind::Sleep as u16),
                    AgentGoal::Lead => Some(TaskKind::Lead as u16),
                    AgentGoal::Survive if ai.task_id == TaskKind::Eat as u16 => {
                        Some(TaskKind::Eat as u16)
                    }
                    _ => {
                        if ai.task_id == TaskKind::Explore as u16
                            && matches!(
                                *goal,
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

            match *goal {
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
                            &chunk_router,
                            &chunk_map,
                            &chunk_connectivity,
                        );
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
                                    &chunk_router,
                                    &chunk_map,
                                    &chunk_connectivity,
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
                                &chunk_router,
                                &chunk_map,
                                &chunk_connectivity,
                            );
                        }
                    }
                }

                AgentGoal::Lead => {
                    if is_active && ai.task_id == TaskKind::Lead as u16 {
                        return;
                    }
                    if member.faction_id != SOLO {
                        if let Some(home) = faction_registry.home_tile(member.faction_id) {
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx as i16, cur_ty as i16),
                                cur_chunk,
                                home,
                                TaskKind::Lead,
                                None,
                                &chunk_graph,
                                &chunk_router,
                                &chunk_map,
                                &chunk_connectivity,
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
                                &chunk_router,
                                &chunk_map,
                                &chunk_connectivity,
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
                                &chunk_router,
                                &chunk_map,
                                &chunk_connectivity,
                            );
                            return;
                        }
                    }

                    // Solo, no home, or already at home with no bed yet: sleep here.
                    ai.state = AiState::Sleeping;
                    ai.task_id = TaskKind::Sleep as u16;
                }

                // Survive, Gather, Build, TameHorse, Craft, Reproduce, Rescue, ReturnCamp are
                // handled by plan_execution_system. Eating runs through the EatFromInventory /
                // ForageFood / WithdrawAndEat plans whose Eat step is gated on hunger.
                AgentGoal::Survive
                | AgentGoal::GatherFood
                | AgentGoal::GatherWood
                | AgentGoal::GatherStone
                | AgentGoal::Build
                | AgentGoal::TameHorse
                | AgentGoal::Craft
                | AgentGoal::Reproduce
                | AgentGoal::Rescue
                | AgentGoal::ReturnCamp => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::ChunkGraph;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_map_with_chunk(coord: ChunkCoord, surf_z: i8) -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(
            coord,
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    #[test]
    fn assign_task_uses_underground_target_z_when_agent_is_below_surface() {
        // Hill chunk, surface_z = 5. Carve a tunnel cell at (10, 10, 0):
        // floor Dirt at z=0, headspace Air at z=1.
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 5);
        map.set_tile(
            10,
            10,
            0,
            TileData {
                kind: TileKind::Dirt,
                ..Default::default()
            },
        );
        map.set_tile(
            10,
            10,
            1,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        // Agent already standing in the tunnel.
        let mut ai = PersonAI::default();
        ai.current_z = 0;

        let graph = ChunkGraph::default();
        let router = ChunkRouter::default();
        let conn = ChunkConnectivity::default();
        assign_task_with_routing(
            &mut ai,
            (10, 10),
            coord,
            (10, 10),
            TaskKind::Idle,
            None,
            &graph,
            &router,
            &map,
            &conn,
        );

        // target_z must follow the agent's Z (the tunnel floor at z=0),
        // not the surface above (z=5). Without the fix, this would be 5
        // and the flow field would route at the surface, stranding the
        // agent in the tunnel.
        assert_eq!(ai.target_z, 0);
    }
}
