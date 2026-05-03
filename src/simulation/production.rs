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
use crate::economy::goods::{Bulk, Good};
use crate::economy::item::Item;
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
                let seed_and_plant = if agent.quantity_of(Good::GrainSeed) > 0 {
                    Some((Good::GrainSeed, PlantKind::Grain))
                } else if agent.quantity_of(Good::BerrySeed) > 0 {
                    Some((Good::BerrySeed, PlantKind::BerryBush))
                } else {
                    None
                };
                if !plant_map.0.contains_key(&(tx, ty)) {
                    if let Some((seed_good, plant_kind)) = seed_and_plant {
                        agent.remove_good(seed_good, 1);
                        spawn_plant_at(
                            &mut commands,
                            &mut plant_map,
                            &mut plant_sprite_index,
                            tx,
                            ty,
                            plant_kind,
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
                            let tile = (tx as i32, ty as i32);
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
                // Preserve the manufactured stats (material + quality +
                // weapon/armor stats) by transferring the actual `Item`,
                // not a freshly-constructed commodity. Without this, an
                // Iron Spear withdrawn from storage would arrive in
                // inventory as a stat-less commodity and the equip step
                // would wield it for zero damage_bonus.
                agent.add_item(gi.item, 1);
                if gi.qty == 1 {
                    commands.entity(gi_entity).despawn_recursive();
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
        let pickup_item = Item::new_commodity(target_good);

        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if remaining == 0 {
                break;
            }
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.qty == 0 || gi.item.good != target_good {
                    continue;
                }
                let want = remaining.min(gi.qty);

                // Hands first (bulk-aware, large weight cap), then fall back to
                // weight-capped personal inventory for any residual. Stone (and
                // other TwoHand goods) weigh as much as the entire inventory cap,
                // so without the hand path even a single seed in inventory would
                // cause the executor to silently destroy units that don't fit.
                let after_hands = carrier.try_pick_up(pickup_item, want);
                let in_hands = want - after_hands;
                let after_inv = if after_hands > 0 {
                    agent.add_good(target_good, after_hands)
                } else {
                    0
                };
                let in_inv = after_hands - after_inv;
                let taken = in_hands + in_inv;
                if taken == 0 {
                    continue;
                }
                if gi.qty == taken {
                    commands.entity(gi_entity).despawn_recursive();
                } else {
                    gi.qty -= taken;
                }
                remaining -= taken;
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

/// Withdraw one edible good from a faction storage tile into the agent's hands or inventory.
/// Driven by `TaskKind::WithdrawFood`. Agent works from a tile adjacent to the
/// storage tile (`ai.dest_tile`) and reaches over to pull one item off the stack.
///
/// Before picking up food, any `Bulk::TwoHand` building materials (Stone, Wood, Iron)
/// sitting in the agent's personal inventory are returned to the faction storage tile.
/// These goods fill the entire 5 kg inventory cap and prevent food from being stored,
/// so clearing them first ensures the agent can always accept the food.
///
/// Food is then placed hands-first (matching how `withdraw_material_task_system` works)
/// so an agent whose inventory is still tight can still receive food in hand.
pub fn withdraw_food_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    storage_tile_map: Res<StorageTileMap>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Carrier,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, mut carrier, member, slot, lod) in query.iter_mut() {
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

        // Return TwoHand building materials from personal inventory to the faction
        // storage tile. Stone/Wood/Iron each weigh 5 kg (the full inventory cap), so
        // keeping them in a hungry worker's pocket blocks all food intake.
        let mut deposit_buf = [(Good::Stone, 0u32); 8];
        let mut deposit_len = 0usize;
        for &(item, qty) in agent.inventory.iter() {
            if qty > 0 && item.good.bulk() == Bulk::TwoHand {
                deposit_buf[deposit_len] = (item.good, qty);
                deposit_len += 1;
            }
        }
        for &(good, qty) in &deposit_buf[..deposit_len] {
            agent.remove_good(good, qty);
            spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                tx as i32,
                ty as i32,
                good,
                qty,
            );
        }

        let mut withdrew = false;
        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.item.good.is_edible() && gi.qty > 0 {
                    // Try hands first; fall back to inventory for any leftover.
                    let food_item = Item::new_commodity(gi.item.good);
                    let after_hands = carrier.try_pick_up(food_item, 1);
                    let leftover = if after_hands > 0 {
                        agent.add_good(gi.item.good, after_hands)
                    } else {
                        0
                    };
                    let taken = 1u32.saturating_sub(leftover);
                    if taken > 0 {
                        if gi.qty == 1 {
                            commands.entity(gi_entity).despawn_recursive();
                        } else {
                            gi.qty -= 1;
                        }
                        withdrew = true;
                    }
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

/// Identifies which physical store an edible candidate lives in.
/// Inventory stacks are indexed; hand slots are singletons.
#[derive(Clone, Copy)]
enum EdibleSlot {
    Inventory(usize),
    HandLeft,
    HandRight,
}

/// Total edible quantity available to an agent across both their personal
/// inventory and their hands. Foraged food (Bulk::Small) often lives in hands
/// because `gather::route_yield` routes through `Carrier::try_pick_up` first.
pub fn total_edible(agent: &EconomicAgent, carrier: &Carrier) -> u32 {
    let mut from_hands = 0u32;
    for slot in [carrier.left, carrier.right].into_iter().flatten() {
        if slot.item.good.is_edible() {
            from_hands = from_hands.saturating_add(slot.qty);
        }
    }
    agent.total_food().saturating_add(from_hands)
}

/// Multi-tick eating: consumes one edible from inventory or hands after
/// `TICKS_EAT` ticks in the Working state, then reduces hunger and (for Fruit)
/// yields a Seed. Driven by `TaskKind::Eat`. The Eat task is dispatched
/// in-place by the goal dispatcher or as the final step of food-gathering
/// plans.
pub fn eat_task_system(
    clock: Res<SimClock>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Needs,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, mut carrier, mut needs, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::Eat as u16 {
            continue;
        }

        // No food on hand or in inventory — nothing to eat. Abort cleanly.
        if total_edible(&agent, &carrier) == 0 {
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

            // Snapshot edible slots across inventory + hands. Bounded by
            // INVENTORY_SLOTS (6) + 2 hands; allocation is per eating-tick on
            // an active eater, which is rare.
            let mut min_nut: u32 = u32::MAX;
            let mut max_nut: u32 = 0;
            let mut best_cover: Option<(EdibleSlot, u32, Good)> = None;
            let mut best_largest: Option<(EdibleSlot, u32, Good)> = None;
            let mut consider = |src: EdibleSlot,
                                good: Good,
                                qty: u32,
                                hunger: f32,
                                min_nut: &mut u32,
                                max_nut: &mut u32,
                                best_cover: &mut Option<(EdibleSlot, u32, Good)>,
                                best_largest: &mut Option<(EdibleSlot, u32, Good)>| {
                if qty == 0 || !good.is_edible() {
                    return;
                }
                let nut = good.nutrition() as u32;
                if nut < *min_nut {
                    *min_nut = nut;
                }
                if nut >= *max_nut {
                    *max_nut = nut;
                    *best_largest = Some((src, nut, good));
                }
                if (nut as f32) >= hunger {
                    match *best_cover {
                        Some((_, prev, _)) if nut >= prev => {}
                        _ => *best_cover = Some((src, nut, good)),
                    }
                }
            };

            for (idx, (it, q)) in agent.inventory.iter().enumerate() {
                consider(
                    EdibleSlot::Inventory(idx),
                    it.good,
                    *q,
                    needs.hunger,
                    &mut min_nut,
                    &mut max_nut,
                    &mut best_cover,
                    &mut best_largest,
                );
            }
            if let Some(s) = carrier.left {
                consider(
                    EdibleSlot::HandLeft,
                    s.item.good,
                    s.qty,
                    needs.hunger,
                    &mut min_nut,
                    &mut max_nut,
                    &mut best_cover,
                    &mut best_largest,
                );
            }
            if let Some(s) = carrier.right {
                consider(
                    EdibleSlot::HandRight,
                    s.item.good,
                    s.qty,
                    needs.hunger,
                    &mut min_nut,
                    &mut max_nut,
                    &mut best_cover,
                    &mut best_largest,
                );
            }

            if min_nut == u32::MAX {
                break; // No edibles left.
            }

            // Satiety stop: next bite would be more than 50% waste.
            if needs.hunger * 2.0 < min_nut as f32 {
                break;
            }

            let (src, _nut, good) = match best_cover.or(best_largest) {
                Some(x) => x,
                None => break,
            };

            match src {
                EdibleSlot::Inventory(idx) => {
                    agent.inventory[idx].1 -= 1;
                }
                EdibleSlot::HandLeft | EdibleSlot::HandRight => {
                    // remove_good walks both hand slots; one unit always lands.
                    carrier.remove_good(good, 1);
                }
            }
            needs.hunger = (needs.hunger - good.nutrition() as f32).max(0.0);
            if good == Good::Fruit {
                fruits_consumed += 1;
            }
        }
        for _ in 0..fruits_consumed {
            agent.add_good(Good::BerrySeed, 1);
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
