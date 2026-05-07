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
use crate::economy::goods::Bulk;
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

/// Remove one unit of `id` from wherever the agent holds it, preferring
/// hands. Used by executors that don't care which store the consumable came
/// from (e.g. planting a seed that may have just been harvested into hands or
/// withdrawn into inventory).
pub fn consume_one_resource(
    agent: &mut EconomicAgent,
    carrier: &mut Carrier,
    id: crate::economy::resource_catalog::ResourceId,
) {
    if carrier.quantity_of_resource(id) > 0 {
        carrier.remove_resource(id, 1);
    } else {
        agent.remove_resource(id, 1);
    }
}

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
    mut discovery_events: EventWriter<crate::simulation::knowledge::DiscoveryActionEvent>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Skills,
        &mut Needs,
        &BucketSlot,
        &LodLevel,
        Option<&FactionMember>,
        Option<&JobClaim>,
    )>,
) {
    for (
        actor,
        mut ai,
        mut aq,
        mut agent,
        mut carrier,
        mut skills,
        mut needs,
        slot,
        lod,
        faction_member,
        claim_opt,
    ) in query.iter_mut()
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
                // Walk PlantKind::ALL so adding a new seed/plant pair only
                // requires editing PlantKind::seed_good(). Seeds may live in
                // hands (harvest co-yields route through Carrier) OR
                // inventory (withdrawn from storage), so check both stores.
                let seed_and_plant = PlantKind::ALL.iter().copied().find_map(|kind| {
                    let seed_id = kind.seed_resource()?;
                    let held =
                        agent.quantity_of_resource(seed_id) + carrier.quantity_of_resource(seed_id);
                    (held > 0).then_some((seed_id, kind))
                });
                if !plant_map.0.contains_key(&(tx, ty)) {
                    if let Some((seed_id, plant_kind)) = seed_and_plant {
                        consume_one_resource(&mut agent, &mut carrier, seed_id);
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
                        discovery_events.send(crate::simulation::knowledge::DiscoveryActionEvent {
                            actor,
                            activity: ActivityKind::Farming,
                        });
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
                // Phase 5e-v: drain the typed channel so an HTN
                // PlantFromStorage chain (or PlayPlant — both use this branch)
                // doesn't leave a stale `Task::Planter` / `Task::Idle`
                // mismatch behind. PlayPlant doesn't yet emit a typed task,
                // so `advance()` is a no-op for that path; harmless.
                aq.advance();
            } else {
                // Check if tile is still valid for planting
                if plant_map.0.contains_key(&(tx, ty)) {
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                    aq.advance();
                }
            }
        }

        if task == TaskKind::PlayThrow as u16 {
            if ai.work_progress >= TICKS_PLAY_THROW {
                ai.work_progress = 0;
                let stone_id = crate::economy::core_ids::stone();
                if agent.quantity_of_resource(stone_id) > 0 {
                    agent.remove_resource(stone_id, 1);
                    skills.gain_xp(SkillKind::Combat, 2);
                    if let Some(fm) = faction_member {
                        if let Some(fd) = faction_registry.factions.get_mut(&fm.faction_id) {
                            fd.activity_log.increment(ActivityKind::Combat);
                        }
                    }
                    discovery_events.send(crate::simulation::knowledge::DiscoveryActionEvent {
                        actor,
                        activity: ActivityKind::Combat,
                    });
                    needs.willpower = (needs.willpower + WILLPOWER_PLAY_BURST).clamp(0.0, 255.0);
                }
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                aq.advance();
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
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    use crate::simulation::typed_task::{Task, WithdrawGoodFilter};

    for (mut ai, mut aq, mut agent, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::WithdrawGood as u16 {
            continue;
        }

        // Phase 3b-i: filter comes from the typed task variant. If the typed
        // task disagrees with task_id, the dispatcher forgot to populate it —
        // bail rather than read stale `craft_recipe_id`.
        let Some(filter) = aq.current.as_withdraw_good() else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            aq.advance();
            continue;
        };

        if member.faction_id == SOLO {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            aq.advance();
            continue;
        }

        let (tx, ty) = ai.dest_tile;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            aq.advance();
            continue;
        }

        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.qty == 0 {
                    continue;
                }
                let matches = match filter {
                    WithdrawGoodFilter::AnyEntertainment => {
                        gi.item.resource_id.entertainment_value() > 0
                    }
                    WithdrawGoodFilter::Specific(rid) => gi.item.resource_id == rid,
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
        aq.advance();
    }
}

/// Withdraw the specific good and quantity committed by the dispatching step
/// resolver from a faction storage tile. Driven by `TaskKind::WithdrawMaterial`.
/// The intent lives on the typed `Task::WithdrawMaterial { good, qty }`
/// variant (Phase 3b-ii / 3b-iii); without it the task aborts — every withdraw
/// step commits a target up front at dispatch, so a missing typed task means
/// the dispatch path skipped (e.g. plan was preempted) and the safe thing to
/// do is bail.
///
/// On entry the executor first drops any hand stack whose good doesn't match
/// the target so the agent's hands are free for the deposit step that
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
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    chunk_graph: Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: Res<crate::pathfinding::chunk_router::ChunkRouter>,
    chunk_connectivity: Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
    co_query: Query<&crate::simulation::crafting::CraftOrder>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
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
    use crate::simulation::typed_task::Task;
    use crate::world::terrain::world_to_tile;

    for (
        entity,
        mut ai,
        mut aq,
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

        let cur_tile = world_to_tile(transform.translation.truncate());
        let cur_chunk = crate::world::chunk::ChunkCoord(
            cur_tile.0.div_euclid(crate::world::chunk::CHUNK_SIZE as i32),
            cur_tile.1.div_euclid(crate::world::chunk::CHUNK_SIZE as i32),
        );

        // Phase 3b-ii: target good + qty come from the typed variant. If the
        // typed task disagrees with task_id, the dispatcher forgot to populate
        // it — bail rather than fall back to stale legacy intent.
        let Some((target_resource, target_qty)) = aq.current.as_withdraw_material() else {
            finish_withdraw_material(
                &mut ai,
                &mut aq,
                &storage_reservations,
                &chunk_map,
                &chunk_graph,
                &chunk_router,
                &chunk_connectivity,
                &bp_query,
                &co_query,
                cur_tile,
                cur_chunk,
            );
            continue;
        };

        if member.faction_id == SOLO {
            finish_withdraw_material(
                &mut ai,
                &mut aq,
                &storage_reservations,
                &chunk_map,
                &chunk_graph,
                &chunk_router,
                &chunk_connectivity,
                &bp_query,
                &co_query,
                cur_tile,
                cur_chunk,
            );
            continue;
        }

        let (tx, ty) = ai.dest_tile;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            // Storage tile is no longer owned by our faction — abort.
            finish_withdraw_material(
                &mut ai,
                &mut aq,
                &storage_reservations,
                &chunk_map,
                &chunk_graph,
                &chunk_router,
                &chunk_connectivity,
                &bp_query,
                &co_query,
                cur_tile,
                cur_chunk,
            );
            continue;
        }

        // Drop any held stacks whose good doesn't match the target so the
        // agent's hands are free for the next haul step. Stacks of the same
        // good are kept (they'll be top-ups on the deposit). Drops to
        // ground at the agent's current world tile, not the storage tile,
        // so the spill doesn't pollute the stockpile.
        if !carrier.is_empty() {
            let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
            // Check left / right; collect mismatched stacks first to avoid
            // borrowing issues across the spawn call.
            let mut to_drop: Vec<(crate::economy::resource_catalog::ResourceId, u32)> =
                Vec::new();
            if let Some(s) = carrier.left {
                if s.item.resource_id != target_resource {
                    to_drop.push((s.item.resource_id, s.qty));
                }
            }
            if let Some(s) = carrier.right {
                if s.item.resource_id != target_resource {
                    to_drop.push((s.item.resource_id, s.qty));
                }
            }
            for (rid, qty) in to_drop {
                carrier.remove_resource(rid, qty);
                spawn_or_merge_ground_item(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    agent_tx,
                    agent_ty,
                    rid,
                    qty,
                );
            }
        }

        let mut remaining = target_qty as u32;
        let promised = target_qty as u32;
        let pickup_item = Item::new_commodity(target_resource);

        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if remaining == 0 {
                break;
            }
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.qty == 0 || gi.item.resource_id != target_resource {
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
                    agent.add_resource(target_resource, after_hands)
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

        finish_withdraw_material(
            &mut ai,
            &mut aq,
            &storage_reservations,
            &chunk_map,
            &chunk_graph,
            &chunk_router,
            &chunk_connectivity,
            &bp_query,
            &co_query,
            cur_tile,
            cur_chunk,
        );
    }
}

/// Shared exit path for `withdraw_material_task_system`. Releases the storage
/// reservation, advances the prefetched ring, and primes the legacy channel
/// for the next leg of the chain. When the next task is a chained
/// `Task::HaulToBlueprint { blueprint }` (the canonical 5c-ii-b shape produced
/// by `WithdrawAndHaulToBlueprintMethod` under the HTN AcquireGood pipeline),
/// looks up the blueprint's tile and routes the agent there with
/// `TaskKind::HaulMaterials`. From there `construction_system`'s hauler branch
/// takes over.
///
/// Other (non-HaulToBlueprint / Idle) follow-ups fall back to a clean
/// `(Idle, UNEMPLOYED)` slot — defensive against future methods that chain
/// something other than HaulToBlueprint behind WithdrawMaterial. A blueprint
/// entity that's been despawned (mid-chain race) collapses to Idle so the
/// agent doesn't strand on a missing target.
fn finish_withdraw_material(
    ai: &mut PersonAI,
    aq: &mut crate::simulation::typed_task::ActionQueue,
    storage_reservations: &StorageReservations,
    chunk_map: &crate::world::chunk::ChunkMap,
    chunk_graph: &crate::pathfinding::chunk_graph::ChunkGraph,
    chunk_router: &crate::pathfinding::chunk_router::ChunkRouter,
    chunk_connectivity: &crate::pathfinding::connectivity::ChunkConnectivity,
    bp_query: &Query<&crate::simulation::construction::Blueprint>,
    co_query: &Query<&crate::simulation::crafting::CraftOrder>,
    cur_tile: (i32, i32),
    cur_chunk: crate::world::chunk::ChunkCoord,
) {
    use crate::simulation::tasks::assign_task_with_routing;
    use crate::simulation::typed_task::Task;

    release_reservation(storage_reservations, ai);
    ai.work_progress = 0;
    aq.advance();

    match aq.current {
        Task::HaulToBlueprint { blueprint } => {
            // Blueprint may have been satisfied / despawned between dispatch
            // and arrival. Drop the chain to Idle so the goal-dispatch path
            // can re-evaluate next tick rather than strand the agent.
            let Ok(bp) = bp_query.get(blueprint) else {
                aq.cancel();
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                return;
            };
            let bp_tile = (bp.tile.0, bp.tile.1);
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                bp_tile,
                TaskKind::HaulMaterials,
                Some(blueprint),
                chunk_graph,
                chunk_router,
                chunk_map,
                chunk_connectivity,
            );
            if !dispatched {
                aq.cancel();
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
            }
        }
        Task::HaulToCraftOrder { order } => {
            // Phase 5e-xi-a: DeliverMaterialToCraftOrder chain.
            // `WithdrawAndHaulToCraftOrderMethod` expands to
            // `[WithdrawMaterial, HaulToCraftOrder]`; once the material is in
            // hand, route to the order's anchor tile. Despawned/satisfied
            // orders silently degrade to Idle so the agent re-evaluates.
            let Ok(order_data) = co_query.get(order) else {
                aq.cancel();
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                return;
            };
            let dest = order_data.anchor_tile;
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                dest,
                TaskKind::HaulToCraftOrder,
                Some(order),
                chunk_graph,
                chunk_router,
                chunk_map,
                chunk_connectivity,
            );
            if !dispatched {
                aq.cancel();
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
            }
        }
        Task::Equip { .. } => {
            // Phase 5e-ii: hunter-arm chain. `WithdrawAndEquipHuntingSpearMethod`
            // expands to [WithdrawMaterial, Equip]; once the spear is in hand
            // (or inventory), the trailing Equip is in-place — no routing
            // needed. Prime the legacy channel so `equip_task_system` picks
            // up next tick. Mirrors `finish_withdraw_food`'s priming pattern
            // for the AcquireFood Eat tail.
            ai.state = AiState::Working;
            ai.task_id = TaskKind::Equip as u16;
            ai.work_progress = 0;
        }
        Task::Planter { tile } => {
            // Phase 5e-v: PlantFromStorage chain. `WithdrawAndPlantSeedMethod`
            // expands to [WithdrawMaterial, Planter { tile }]; once the seed is
            // in hand (or inventory) the agent walks to the destination
            // farmland tile picked at dispatch time, then plants.
            // Routing is required because the planter executor works on the
            // tile itself (not in-place) and the destination differs from the
            // storage tile.
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Planter,
                None,
                chunk_graph,
                chunk_router,
                chunk_map,
                chunk_connectivity,
            );
            if !dispatched {
                aq.cancel();
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
            }
        }
        Task::PlayThrow => {
            // Phase 5e-xii-b: PlayByThrowingRocks chain.
            // `WithdrawAndThrowStonesAsPlayMethod` expands to
            // [WithdrawMaterial { stone, 1 }, PlayThrow]; once the stone is
            // in hand (or inventory), the throw is in-place — no routing.
            // Prime the legacy channel so `production_system`'s PlayThrow
            // branch picks up next tick. Mirrors `Equip`'s priming pattern
            // for the hunter-arm chain.
            ai.state = AiState::Working;
            ai.task_id = TaskKind::PlayThrow as u16;
            ai.work_progress = 0;
        }
        Task::Play { partner: _ } => {
            // Phase 5e-xii-c: PlayWithStoredToy chain.
            // `WithdrawAndPlayWithToyMethod` expands to
            // [WithdrawMaterial { toy, 1 }, Play { partner: None }]; once the
            // toy is in hand, solo play is in-place — no routing.
            // `play_system`'s solo branch reads the held entertainment value
            // from `Carrier`. Prime the legacy channel so the play_system
            // executor picks up next tick.
            ai.state = AiState::Working;
            ai.task_id = TaskKind::Play as u16;
            ai.work_progress = 0;
            // target_entity should already be None (the WithdrawMaterial head
            // dispatch passed None to `assign_task_with_routing`) — the solo
            // branch in play_system needs target_entity = None to fall through
            // to the held-item path.
            ai.target_entity = None;
        }
        Task::PlayPlant { tile } => {
            // Phase 5e-xii-d: PlayByPlanting / PlayByPlantingBerry chain.
            // `WithdrawAndPlantGrainSeedAsPlayMethod` /
            // `WithdrawAndPlantBerrySeedAsPlayMethod` expand to
            // [WithdrawMaterial { seed, 1 }, PlayPlant { tile }]; once the
            // seed is in hand, the agent walks to the destination grass tile
            // picked at dispatch time and plants. Mirrors the Planter chain
            // handoff but routes via `TaskKind::PlayPlant` so the
            // production_system Planter branch fires the willpower burst
            // (`is_play = true`).
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::PlayPlant,
                None,
                chunk_graph,
                chunk_router,
                chunk_map,
                chunk_connectivity,
            );
            if !dispatched {
                aq.cancel();
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
            }
        }
        Task::Idle => {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
        }
        _ => {
            // No other task family is expected as a chained follow-up to
            // WithdrawMaterial. Drop the entire chain to Idle so a
            // mis-built expansion can't strand the agent.
            aq.cancel();
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
        }
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
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut Carrier,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut aq, mut agent, mut carrier, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::WithdrawFood as u16 {
            continue;
        }
        if member.faction_id == SOLO {
            finish_withdraw_food(&mut ai, &mut aq);
            continue;
        }

        let (tx, ty) = ai.dest_tile;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            // Storage tile is no longer owned by our faction (or never was) — abort.
            finish_withdraw_food(&mut ai, &mut aq);
            continue;
        }

        // Return TwoHand building materials from personal inventory to the faction
        // storage tile. Stone/Wood/Iron each weigh 5 kg (the full inventory cap), so
        // keeping them in a hungry worker's pocket blocks all food intake.
        let mut deposit_buf = [(crate::economy::resource_catalog::ResourceId::NONE, 0u32); 8];
        let mut deposit_len = 0usize;
        for &(item, qty) in agent.inventory.iter() {
            if qty > 0 && item.resource_id.bulk() == Bulk::TwoHand {
                deposit_buf[deposit_len] = (item.resource_id, qty);
                deposit_len += 1;
            }
        }
        for &(rid, qty) in &deposit_buf[..deposit_len] {
            agent.remove_resource(rid, qty);
            spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                tx as i32,
                ty as i32,
                rid,
                qty,
            );
        }

        let mut withdrew = false;
        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                if gi.item.resource_id.is_edible() && gi.qty > 0 {
                    // Try hands first; fall back to inventory for any leftover.
                    let food_item = Item::new_commodity(gi.item.resource_id);
                    let after_hands = carrier.try_pick_up(food_item, 1);
                    let leftover = if after_hands > 0 {
                        agent.add_resource(gi.item.resource_id, after_hands)
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
        finish_withdraw_food(&mut ai, &mut aq);
    }
}

/// Shared exit path for `withdraw_food_task_system`. Pops the prefetched queue
/// into `aq.current`; if the next task is a chained `Task::Eat` (the canonical
/// shape produced by `WithdrawFromStorageMethod` under the HTN AcquireFood
/// pipeline) primes the legacy channel directly so `eat_task_system` picks up
/// without re-entering dispatch. Other (non-Eat / Idle) follow-ups fall back to
/// a clean `(Idle, UNEMPLOYED)` slot — defensive against future methods that
/// chain something other than Eat behind WithdrawFood.
fn finish_withdraw_food(
    ai: &mut PersonAI,
    aq: &mut crate::simulation::typed_task::ActionQueue,
) {
    use crate::simulation::typed_task::Task;
    aq.advance();
    ai.work_progress = 0;
    match aq.current {
        Task::Eat => {
            // Hand off straight into the Eat executor.
            ai.state = AiState::Working;
            ai.task_id = TaskKind::Eat as u16;
        }
        Task::Idle => {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
        }
        _ => {
            // No other task family is expected as a chained follow-up to
            // WithdrawFood at 5b-iii-ii. Drop the entire chain to Idle so a
            // mis-built expansion can't strand the agent.
            aq.cancel();
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
        }
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
        if slot.item.resource_id.is_edible() {
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
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Needs,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut aq, mut agent, mut carrier, mut needs, slot, lod) in query.iter_mut() {
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
            // Phase 6b-ii: drain the typed channel so
            // `htn_method_completion_system` can record `MethodOutcome::Success`
            // for the dispatching method (or `htn_eat_dispatch_system` doesn't
            // pile duplicate Eat tasks onto the queue ring next tick).
            aq.advance();
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
            let mut best_cover: Option<(
                EdibleSlot,
                u32,
                crate::economy::resource_catalog::ResourceId,
            )> = None;
            let mut best_largest: Option<(
                EdibleSlot,
                u32,
                crate::economy::resource_catalog::ResourceId,
            )> = None;
            let mut consider =
                |src: EdibleSlot,
                 rid: crate::economy::resource_catalog::ResourceId,
                 qty: u32,
                 hunger: f32,
                 min_nut: &mut u32,
                 max_nut: &mut u32,
                 best_cover: &mut Option<(
                    EdibleSlot,
                    u32,
                    crate::economy::resource_catalog::ResourceId,
                )>,
                 best_largest: &mut Option<(
                    EdibleSlot,
                    u32,
                    crate::economy::resource_catalog::ResourceId,
                )>| {
                    if qty == 0 || !rid.is_edible() {
                        return;
                    }
                    let nut = rid.nutrition() as u32;
                    if nut < *min_nut {
                        *min_nut = nut;
                    }
                    if nut >= *max_nut {
                        *max_nut = nut;
                        *best_largest = Some((src, nut, rid));
                    }
                    if (nut as f32) >= hunger {
                        match *best_cover {
                            Some((_, prev, _)) if nut >= prev => {}
                            _ => *best_cover = Some((src, nut, rid)),
                        }
                    }
                };

            for (idx, (it, q)) in agent.inventory.iter().enumerate() {
                consider(
                    EdibleSlot::Inventory(idx),
                    it.resource_id,
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
                    s.item.resource_id,
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
                    s.item.resource_id,
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

            let (src, _nut, rid) = match best_cover.or(best_largest) {
                Some(x) => x,
                None => break,
            };

            match src {
                EdibleSlot::Inventory(idx) => {
                    agent.inventory[idx].1 -= 1;
                }
                EdibleSlot::HandLeft | EdibleSlot::HandRight => {
                    // remove_resource walks both hand slots; one unit always lands.
                    carrier.remove_resource(rid, 1);
                }
            }
            needs.hunger = (needs.hunger - rid.nutrition() as f32).max(0.0);
            if Some(rid) == crate::economy::core_ids::Fruit.get().copied() {
                fruits_consumed += 1;
            }
        }
        if fruits_consumed > 0 {
            let berry_seed = crate::economy::core_ids::berry_seed();
            for _ in 0..fruits_consumed {
                agent.add_resource(berry_seed, 1);
            }
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
        // Phase 6b-ii: drain the typed channel on natural completion so
        // `htn_method_completion_system` (Economy, after deposit) sees
        // `aq.current == Idle` and records the dispatching method's
        // `MethodOutcome::Success` against `MethodHistory`.
        aq.advance();
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
    mut query: Query<(
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
    untamed_horses: Query<(), (With<Horse>, Without<Tamed>)>,
    untamed_cows: Query<(), (With<Cow>, Without<Tamed>)>,
    untamed_pigs: Query<(), (With<Pig>, Without<Tamed>)>,
    untamed_cats: Query<(), (With<Cat>, Without<Tamed>)>,
) {
    const TICKS_TAME: u8 = 100;

    for (mut ai, mut aq, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::TameAnimal as u16 {
            continue;
        }

        // Phase 5e-iv: prefer the typed task's target for chain-integrity;
        // fall back to legacy `ai.target_entity` so the plan-driven path
        // (still the only producer pre-migration) keeps working.
        let Some(target) = aq.current.as_tame_animal().or(ai.target_entity) else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            aq.advance();
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
            aq.advance();
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
            aq.advance();
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
            aq.advance();
        }
    }
}
