use super::animals::{Cat, Cow, Horse, Pig, Tamed};
use super::carry::Carrier;
use super::faction::{
    release_reservation, FactionMember, FactionRegistry, StorageReservations, StorageTileMap, SOLO,
};
use super::items::{spawn_or_merge_ground_item, GroundItem};
use super::jobs::{
    planting_area_contains, record_progress, JobBoard, JobClaim, JobCompletedEvent, JobKind,
};
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillKind, Skills};
use super::tasks::TaskKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap, PlantSpriteIndex,
};
use crate::simulation::technology::{
    ActivityKind, ANIMAL_HUSBANDRY, DOG_DOMESTICATION, HORSE_TAMING,
};
use crate::world::spatial::SpatialIndex;
use ahash::AHashMap;
use bevy::prelude::*;

pub const TICKS_FARMER_PLANT: u8 = 40;

/// Ticks the agent spends winding up a play-throw before the rock leaves their
/// hand. Short — throwing a rock is a quick action.
const TICKS_PLAY_THROW: u8 = 30;

/// One-shot willpower bonus applied when a PlayPlant or PlayThrow task
/// completes successfully. Picked to roughly match the per-task gain a solo
/// PlaySolo session would deliver, so all three Play plans feel comparable.
const WILLPOWER_PLAY_BURST: f32 = 60.0;

/// `work_progress` units required before the Eat task consumes a food item.
/// Movement's Working-state tick adds ~1 progress per active sim tick.
const TICKS_EAT: u8 = 8;

// Tile depletion — tracks how many times each tile has been harvested recently.
// Absent from map = fully recovered. Higher value = more depleted.
const REGEN_INTERVAL: u64 = 2000; // ticks between each +1 recovery per tile

#[derive(Resource, Default)]
pub struct TileDepletion(pub AHashMap<(i32, i32), u8>);

impl TileDepletion {
    fn is_exhausted(&self, tx: i32, ty: i32, max: u8) -> bool {
        self.0.get(&(tx, ty)).copied().unwrap_or(0) >= max
    }

    fn deplete(&mut self, tx: i32, ty: i32) {
        *self.0.entry((tx, ty)).or_insert(0) += 1;
    }
}

pub fn tile_regen_system(clock: Res<SimClock>, mut depletion: ResMut<TileDepletion>) {
    if clock.tick % REGEN_INTERVAL != 0 {
        return;
    }
    depletion.0.retain(|_, v| {
        *v = v.saturating_sub(1);
        *v > 0
    });
}

pub fn production_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut faction_registry: ResMut<FactionRegistry>,
    mut board: ResMut<JobBoard>,
    mut job_completed: EventWriter<JobCompletedEvent>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &mut Needs,
        &BucketSlot,
        &LodLevel,
        Option<&FactionMember>,
        Option<&JobClaim>,
    )>,
) {
    for (mut ai, mut agent, mut skills, mut needs, slot, lod, faction_member, claim_opt) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }

        let tx = ai.dest_tile.0 as i32;
        let ty = ai.dest_tile.1 as i32;
        let task = ai.task_id;

        // Planter and PlayPlant share the plant-on-grass pipeline. The only
        // difference is that PlayPlant frames the activity as recreation,
        // adding a one-shot willpower burst on completion.
        if task == TaskKind::Planter as u16 || task == TaskKind::PlayPlant as u16 {
            let is_play = task == TaskKind::PlayPlant as u16;
            if ai.work_progress >= TICKS_FARMER_PLANT {
                ai.work_progress = 0;
                if !plant_map.0.contains_key(&(tx, ty)) && agent.quantity_of(Good::Seed) > 0 {
                    agent.remove_good(Good::Seed, 1);
                    spawn_plant_at(
                        &mut commands,
                        &mut plant_map,
                        &mut plant_sprite_index,
                        tx,
                        ty,
                        PlantKind::Grain,
                        GrowthStage::Seed,
                    );
                    skills.gain_xp(SkillKind::Farming, 3);
                    if let Some(fm) = faction_member {
                        if let Some(fd) = faction_registry.factions.get_mut(&fm.faction_id) {
                            fd.activity_log.increment(ActivityKind::Farming);
                        }
                    }
                    // Credit a Farm posting if this worker holds one and the
                    // tile falls within the posting's designated area.
                    if let Some(claim) = claim_opt {
                        let tile = (tx as i16, ty as i16);
                        let in_area = board
                            .get(claim.job_id)
                            .map(|p| planting_area_contains(&p.progress, tile))
                            .unwrap_or(false);
                        if in_area {
                            record_progress(
                                &mut commands,
                                &mut board,
                                &mut job_completed,
                                claim,
                                JobKind::Farm,
                                1,
                            );
                        }
                    }
                    if is_play {
                        needs.willpower =
                            (needs.willpower + WILLPOWER_PLAY_BURST).clamp(0.0, 255.0);
                    }
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            } else {
                // Check if tile is still valid for planting
                if plant_map.0.contains_key(&(tx, ty)) {
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
            }
        }

        if task == TaskKind::PlayThrow as u16 {
            if ai.work_progress >= TICKS_PLAY_THROW {
                ai.work_progress = 0;
                if agent.quantity_of(Good::Stone) > 0 {
                    agent.remove_good(Good::Stone, 1);
                    skills.gain_xp(SkillKind::Combat, 2);
                    if let Some(fm) = faction_member {
                        if let Some(fd) = faction_registry.factions.get_mut(&fm.faction_id) {
                            fd.activity_log.increment(ActivityKind::Combat);
                        }
                    }
                    needs.willpower = (needs.willpower + WILLPOWER_PLAY_BURST).clamp(0.0, 255.0);
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
        }

        if agent.is_inventory_full() {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
        }
    }
}

/// Withdraw one of a specific good (or any entertainment-valued good if
/// `craft_recipe_id == 255`) from a faction storage tile. Driven by
/// `TaskKind::WithdrawGood`; mirrors `withdraw_material_task_system` but the
/// filter comes from the dispatching step rather than blueprint demand.
pub fn withdraw_good_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    storage_tile_map: Res<StorageTileMap>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    const ENTERTAINMENT_SENTINEL: u8 = 255;

    for (mut ai, mut agent, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::WithdrawGood as u16 {
            continue;
        }

        if member.faction_id == SOLO {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        let (tx, ty) = ai.dest_tile;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        let filter = ai.craft_recipe_id;
        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.qty == 0 {
                    continue;
                }
                let matches = if filter == ENTERTAINMENT_SENTINEL {
                    gi.item.good.entertainment_value() > 0
                } else {
                    gi.item.good as u8 == filter
                };
                if !matches {
                    continue;
                }
                agent.add_good(gi.item.good, 1);
                if gi.qty == 1 {
                    commands.entity(gi_entity).despawn();
                } else {
                    gi.qty -= 1;
                }
                break;
            }
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
    }
}

/// Withdraw the specific good and quantity committed by the dispatching step
/// resolver from a faction storage tile. Driven by `TaskKind::WithdrawMaterial`.
/// The intent (`PersonAI.withdraw_good` / `withdraw_qty`) is set in
/// `plan_execution_system` when a `StepTarget::WithdrawForFactionNeed`
/// resolves; this system consumes it. Without an intent, the task aborts —
/// every withdraw step commits a target up front, so an empty intent means
/// the dispatch path skipped (e.g. plan was preempted) and the safe thing to
/// do is bail.
///
/// On entry the executor first drops any hand stack whose good doesn't match
/// `withdraw_good` so the agent's hands are free for the deposit step that
/// usually follows. If the spatial scan finds zero of the target good (race
/// — another agent already drained the stack), the active plan is marked
/// failed via `PlanHistory` so the agent doesn't immediately walk back to
/// the same dry tile next tick. Every exit path releases the storage
/// reservation tracked on `PersonAI`.
pub fn withdraw_material_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<StorageReservations>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Carrier,
        &Transform,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
        Option<&mut crate::simulation::plan::PlanHistory>,
        Option<&crate::simulation::plan::ActivePlan>,
    )>,
) {
    use crate::simulation::plan::PlanOutcome;
    use crate::world::terrain::world_to_tile;

    for (
        entity,
        mut ai,
        mut agent,
        mut carrier,
        transform,
        member,
        slot,
        lod,
        mut plan_history_opt,
        active_plan_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::WithdrawMaterial as u16 {
            continue;
        }

        if member.faction_id == SOLO {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.withdraw_good = None;
            ai.withdraw_qty = 0;
            release_reservation(&storage_reservations, &mut ai);
            continue;
        }

        let (tx, ty) = ai.dest_tile;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            // Storage tile is no longer owned by our faction — abort.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.withdraw_good = None;
            ai.withdraw_qty = 0;
            release_reservation(&storage_reservations, &mut ai);
            continue;
        }

        let Some(target_good) = ai.withdraw_good else {
            // No targeted intent — nothing to withdraw. Older opportunistic
            // behavior is intentionally gone: the resolver must commit a good.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.withdraw_qty = 0;
            release_reservation(&storage_reservations, &mut ai);
            continue;
        };

        // Drop any held stacks whose good doesn't match the target so the
        // agent's hands are free for the next haul step. Stacks of the same
        // good are kept (they'll be top-ups on the deposit). Drops to
        // ground at the agent's current world tile, not the storage tile,
        // so the spill doesn't pollute the stockpile.
        if !carrier.is_empty() {
            let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
            // Check left / right; collect mismatched stacks first to avoid
            // borrowing issues across the spawn call.
            let mut to_drop: Vec<(Good, u32)> = Vec::new();
            if let Some(s) = carrier.left {
                if s.item.good != target_good {
                    to_drop.push((s.item.good, s.qty));
                }
            }
            if let Some(s) = carrier.right {
                if s.item.good != target_good {
                    to_drop.push((s.item.good, s.qty));
                }
            }
            for (good, qty) in to_drop {
                carrier.remove_good(good, qty);
                spawn_or_merge_ground_item(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    agent_tx,
                    agent_ty,
                    good,
                    qty,
                );
            }
        }

        let mut remaining = ai.withdraw_qty as u32;
        let promised = ai.withdraw_qty as u32;

        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if remaining == 0 {
                break;
            }
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.qty == 0 || gi.item.good != target_good {
                    continue;
                }
                let take = remaining.min(gi.qty);
                agent.add_good(gi.item.good, take);
                if gi.qty == take {
                    commands.entity(gi_entity).despawn();
                } else {
                    gi.qty -= take;
                }
                remaining -= take;
            }
        }

        let taken = promised.saturating_sub(remaining);
        if taken == 0 {
            // Race: the stack we reserved was emptied between dispatch and
            // arrival. Mark the active plan failed so PlanHistory penalizes
            // it briefly — without this the next step (HaulTo*) silently
            // no-ops with empty hands and the agent walks back to the same
            // dry tile on the next dispatch cycle.
            if let Some(plan) = active_plan_opt {
                if let Some(history) = plan_history_opt.as_deref_mut() {
                    history.push(plan.plan_id, PlanOutcome::FailedNoTarget, clock.tick);
                }
                commands.entity(entity).remove::<crate::simulation::plan::ActivePlan>();
            }
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
        ai.withdraw_good = None;
        ai.withdraw_qty = 0;
        release_reservation(&storage_reservations, &mut ai);
    }
}

/// Withdraw one edible good from a faction storage tile into the agent's inventory.
/// Driven by `TaskKind::WithdrawFood`. Agent works from a tile adjacent to the
/// storage tile (`ai.dest_tile`) and reaches over to pull one item off the stack.
pub fn withdraw_food_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    storage_tile_map: Res<StorageTileMap>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::WithdrawFood as u16 {
            continue;
        }
        if member.faction_id == SOLO {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        let (tx, ty) = ai.dest_tile;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            // Storage tile is no longer owned by our faction (or never was) — abort.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        let mut withdrew = false;
        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.item.good.is_edible() && gi.qty > 0 {
                    agent.add_good(gi.item.good, 1);
                    if gi.qty == 1 {
                        commands.entity(gi_entity).despawn();
                    } else {
                        gi.qty -= 1;
                    }
                    withdrew = true;
                    break;
                }
            }
        }

        // Whether or not food was found, the task ends this tick.
        let _ = withdrew;
        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
    }
}

/// Multi-tick eating: consumes one edible from inventory after `TICKS_EAT` ticks
/// in the Working state, then reduces hunger and (for Fruit) yields a Seed.
/// Driven by `TaskKind::Eat`. The Eat task is dispatched in-place by the goal
/// dispatcher or as the final step of food-gathering plans.
pub fn eat_task_system(
    clock: Res<SimClock>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Needs,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, mut needs, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::Eat as u16 {
            continue;
        }

        // No food on hand — nothing to eat. Abort cleanly.
        if agent.total_food() == 0 {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        }

        // Only accumulate while in Working state — movement_system increments
        // work_progress for Working agents.
        if ai.state != AiState::Working || ai.work_progress < TICKS_EAT {
            continue;
        }

        // Eat one item at a time. Each iteration: pick the smallest available
        // edible whose nutrition still covers remaining hunger (avoids using a
        // 255-nutrition Meat to satisfy 30 hunger). If no single food covers
        // remaining hunger, fall back to the largest available so we make
        // progress. Stop early when the next bite would be majority waste —
        // i.e. remaining hunger is below half the smallest available food's
        // nutrition. This bounds per-meal waste to <1 unit of the smallest
        // edible the agent is carrying.
        let mut fruits_consumed: u32 = 0;
        loop {
            if needs.hunger <= 0.0 {
                break;
            }

            // Snapshot edible slots in this iteration.
            let mut min_nut: u32 = u32::MAX;
            let mut max_nut: u32 = 0;
            let mut best_cover: Option<(usize, u32)> = None; // (slot_idx, nutrition)
            let mut best_largest: Option<(usize, u32)> = None;
            for (idx, (it, q)) in agent.inventory.iter().enumerate() {
                if !it.good.is_edible() || *q == 0 {
                    continue;
                }
                let nut = it.good.nutrition() as u32;
                if nut < min_nut {
                    min_nut = nut;
                }
                if nut >= max_nut {
                    max_nut = nut;
                    best_largest = Some((idx, nut));
                }
                if (nut as f32) >= needs.hunger {
                    match best_cover {
                        Some((_, prev)) if nut >= prev => {}
                        _ => best_cover = Some((idx, nut)),
                    }
                }
            }

            if min_nut == u32::MAX {
                break; // No edibles left.
            }

            // Satiety stop: next bite would be more than 50% waste.
            if needs.hunger * 2.0 < min_nut as f32 {
                break;
            }

            let pick_idx = match best_cover.or(best_largest) {
                Some((i, _)) => i,
                None => break,
            };

            let (it, q) = &mut agent.inventory[pick_idx];
            *q -= 1;
            needs.hunger = (needs.hunger - it.good.nutrition() as f32).max(0.0);
            if it.good == Good::Fruit {
                fruits_consumed += 1;
            }
        }
        for _ in 0..fruits_consumed {
            agent.add_good(Good::Seed, 1);
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
    }
}

/// Complete the TameAnimal task after the agent has worked adjacent to a wild
/// horse, cow, pig, or cat long enough. Tech requirement varies by species:
///   horse → HORSE_TAMING
///   cow / pig → ANIMAL_HUSBANDRY
///   cat → DOG_DOMESTICATION (companion-animal tech)
pub fn tame_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    faction_registry: Res<FactionRegistry>,
    mut query: Query<(&mut PersonAI, &FactionMember, &BucketSlot, &LodLevel)>,
    untamed_horses: Query<(), (With<Horse>, Without<Tamed>)>,
    untamed_cows: Query<(), (With<Cow>, Without<Tamed>)>,
    untamed_pigs: Query<(), (With<Pig>, Without<Tamed>)>,
    untamed_cats: Query<(), (With<Cat>, Without<Tamed>)>,
) {
    const TICKS_TAME: u8 = 100;

    for (mut ai, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::TameAnimal as u16 {
            continue;
        }

        // Identify species + required tech for the current target
        let Some(target) = ai.target_entity else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        };
        let required_tech = if untamed_horses.get(target).is_ok() {
            Some(HORSE_TAMING)
        } else if untamed_cows.get(target).is_ok() || untamed_pigs.get(target).is_ok() {
            Some(ANIMAL_HUSBANDRY)
        } else if untamed_cats.get(target).is_ok() {
            Some(DOG_DOMESTICATION)
        } else {
            None
        };

        let Some(tech_id) = required_tech else {
            // Target is gone, dead, already tamed, or not a tameable species
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            ai.target_entity = None;
            continue;
        };

        let has_tech = faction_registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.techs.has(tech_id))
            .unwrap_or(false);
        if !has_tech {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            ai.target_entity = None;
            continue;
        }

        if ai.work_progress >= TICKS_TAME {
            commands.entity(target).insert(Tamed {
                owner_faction: member.faction_id,
            });
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            ai.target_entity = None;
        }
    }
}
