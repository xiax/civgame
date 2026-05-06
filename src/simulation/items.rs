use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::carry::Carrier;
use crate::simulation::faction::{FactionMember, SOLO};
use crate::simulation::gather::GatherRoutingResources;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, world_to_tile, TILE_SIZE};
use bevy::prelude::*;

#[derive(Component, Clone, Copy, Default, Debug)]
pub struct TargetItem(pub Option<Entity>);

#[derive(Component, Clone, Copy)]
pub struct GroundItem {
    pub item: Item,
    pub qty: u32,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EquipmentSlot {
    MainHand = 0,
    OffHand = 1,
    HeadArmor = 2,
    TorsoArmor = 3,
    LegArmor = 4,
    ArmArmor = 5,
}

#[derive(Component, Default, Clone)]
pub struct Equipment {
    pub items: bevy::utils::HashMap<EquipmentSlot, Item>,
}

impl Equipment {
    /// True if any equipped slot holds an Item with the given `good`. Used by
    /// `forbids_good` preconditions so a wielded weapon counts the same as one
    /// in inventory or hands.
    pub fn has_good(&self, good: Good) -> bool {
        self.items.values().any(|it| it.good() == good)
    }
}

/// Deposits `qty` of `good` at tile `(tx, ty)` as a commodity stack.
/// Convenience wrapper for callers that don't have a full `Item` (combat
/// drops, scavenge spills, etc.) — equivalent to passing
/// `Item::new_commodity(good)` to `spawn_or_merge_ground_item_full`.
pub fn spawn_or_merge_ground_item(
    commands: &mut Commands,
    spatial: &SpatialIndex,
    item_query: &mut Query<&mut GroundItem>,
    tx: i32,
    ty: i32,
    good: Good,
    qty: u32,
) {
    spawn_or_merge_ground_item_full(
        commands,
        spatial,
        item_query,
        tx,
        ty,
        Item::new_commodity(good),
        qty,
    );
}

/// Deposits `qty` units of the exact `item` at tile `(tx, ty)`. Merges into
/// an existing GroundItem whose `item` matches *fully* (good + material +
/// quality + display_name + stats) so a manufactured Iron Spear stacks with
/// other Iron Spears but not with commodity Weapon stacks. Use this from
/// paths that have a real `Item` value (deposit/withdraw, equip overflow)
/// so material/quality survive a storage round-trip — without it, every
/// stored item is reduced to a commodity and equipped weapons lose their
/// damage bonus.
pub fn spawn_or_merge_ground_item_full(
    commands: &mut Commands,
    spatial: &SpatialIndex,
    item_query: &mut Query<&mut GroundItem>,
    tx: i32,
    ty: i32,
    item: Item,
    qty: u32,
) {
    for &entity in spatial.get(tx, ty) {
        if let Ok(mut gi) = item_query.get_mut(entity) {
            if gi.item == item {
                gi.qty = gi.qty.saturating_add(qty);
                return;
            }
        }
    }
    let world_pos = tile_to_world(tx, ty);
    commands.spawn((
        GroundItem { item, qty },
        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
    ));
}

/// Returns the equipment slots that a given good can be placed into.
pub fn valid_equip_slots(good: Good) -> &'static [EquipmentSlot] {
    match good {
        Good::Weapon => &[EquipmentSlot::MainHand, EquipmentSlot::OffHand],
        Good::Shield => &[EquipmentSlot::OffHand],
        Good::Armor => &[EquipmentSlot::TorsoArmor],
        Good::Cloth => &[EquipmentSlot::TorsoArmor],
        _ => &[],
    }
}

/// Recompute `EconomicAgent.bonus_cap_g` from currently-equipped items. Currently
/// the only contributing slot is TorsoArmor (e.g. cloth or armor) which grants a
/// modest pack/pocket bonus. Runs on `Changed<Equipment>`.
pub fn recompute_inventory_capacity_system(
    mut q: Query<(&Equipment, &mut EconomicAgent), Changed<Equipment>>,
) {
    for (equipment, mut agent) in q.iter_mut() {
        let mut bonus = 0u32;
        if let Some(item) = equipment.items.get(&EquipmentSlot::TorsoArmor) {
            bonus = bonus.saturating_add(match item.good() {
                Good::Cloth => 2_000,
                Good::Armor => 1_000,
                _ => 0,
            });
        }
        agent.bonus_cap_g = bonus;
    }
}

/// True if this good is "personal" — small enough and useful enough that the agent
/// keeps it in their personal inventory rather than carrying it in their hands.
/// Hungry agents personalize edibles too (so they can eat them later).
fn personal_pickup(good: Good, needs: &Needs) -> bool {
    if good.is_seed() {
        return true;
    }
    match good {
        Good::Tools => true,
        Good::Fruit | Good::Meat | Good::Grain => needs.hunger > 80.0,
        _ => false,
    }
}

/// Phase 5c-ii-d-ii-a chain handoff: called by every `item_pickup_system`
/// exit path instead of inlining the legacy reset block. Performs the
/// standard Idle reset + `aq.advance()`, *and* if the prefetch ring promotes
/// a `Task::DepositToFactionStorage { .. }` into `current`, routes the agent
/// to the nearest faction storage tile and primes
/// `task_id = TaskKind::DepositResource` so `drop_items_at_destination_system`
/// picks up next tick. Mirrors `gather.rs::finish_gather`.
///
/// On routing failure (no faction storage, all storage unreachable, or SOLO
/// agent — the dispatcher already gates SOLO out, so this is defensive) the
/// chain is dropped via `aq.cancel()`. The agent stays Idle with full hands;
/// the next dispatcher tick will either re-dispatch a fresh chain or fall
/// through to `Explore`.
fn finish_scavenge(
    ai: &mut PersonAI,
    aq: &mut ActionQueue,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    faction_id: Option<u32>,
    chunk_map: &ChunkMap,
    routing: &GatherRoutingResources,
) {
    ai.state = AiState::Idle;
    ai.task_id = PersonAI::UNEMPLOYED;
    ai.target_entity = None;
    aq.advance();

    match aq.current {
        Task::DepositToFactionStorage { .. } => {
            // 5c-ii-d-ii-a: AcquireGood scavenge tail. Walk to nearest faction
            // storage and prime DepositResource.
            let Some(fid) = faction_id else {
                aq.cancel();
                return;
            };
            let Some(storage_tile) =
                routing.storage_tile_map.nearest_for_faction(fid, cur_tile)
            else {
                aq.cancel();
                return;
            };
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                storage_tile,
                TaskKind::DepositResource,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                chunk_map,
                &routing.chunk_connectivity,
            );
            if !dispatched {
                aq.cancel();
            }
        }
        Task::Eat => {
            // 5c-ii-d-iii-ii: AcquireFood scavenge tail. The food is in the
            // agent's hands now; eat it on the spot — no routing needed,
            // just prime the legacy Eat channel directly. Mirrors
            // `production::finish_withdraw_food`'s Task::Eat handoff for the
            // [WithdrawFood, Eat] chain.
            ai.state = AiState::Working;
            ai.task_id = TaskKind::Eat as u16;
            ai.work_progress = 0;
        }
        _ => {
            // Idle or unrecognised follow-up: ring is empty or contains a
            // task not expected after Scavenge. Default exit (already set
            // above) is correct; nothing more to do.
        }
    }
}

/// Sequential, after death_system.
/// Agents explicitly targeting a GroundItem pick it up once they arrive.
pub fn item_pickup_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    routing: GatherRoutingResources,
    mut item_query: Query<&mut GroundItem>,
    mut pickers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut EconomicAgent,
            &mut Carrier,
            &Needs,
            &mut TargetItem,
            &BucketSlot,
            &LodLevel,
            &Transform,
            Option<&FactionMember>,
            Option<&mut ActivePlan>,
        ),
        With<Person>,
    >,
) {
    for (
        _entity,
        mut ai,
        mut aq,
        mut agent,
        mut carrier,
        needs,
        mut target_item,
        slot,
        lod,
        transform,
        faction_member,
        mut active_plan_opt,
    ) in pickers.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::Scavenge as u16 {
            continue;
        }

        // Agent's current tile + chunk for `finish_scavenge`'s routing
        // decision when the prefetch ring promotes a `DepositToFactionStorage`.
        let cur_tile = world_to_tile(transform.translation.truncate());
        let cur_chunk = ChunkCoord(
            cur_tile.0.div_euclid(CHUNK_SIZE as i32),
            cur_tile.1.div_euclid(CHUNK_SIZE as i32),
        );
        let faction_id = faction_member
            .map(|fm| fm.faction_id)
            .filter(|&id| id != SOLO);

        // Phase 3b-vi: target comes from typed `Task::Scavenge`, falling back
        // to legacy `target_item.0` for any unmigrated dispatch path.
        let Some(target_ent) = aq.current.as_scavenge().or(target_item.0) else {
            // No target, but in scavenge state? Cleanup.
            target_item.0 = None;
            finish_scavenge(
                &mut ai,
                &mut aq,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
            );
            continue;
        };

        if let Ok(mut item) = item_query.get_mut(target_ent) {
            let take_qty = if item.item.good().is_edible() {
                let nutrition = item.item.good().nutrition();
                if nutrition == 0 {
                    item.qty
                } else {
                    let bites = ((needs.hunger / nutrition as f32).ceil() as u32).max(1);
                    bites.min(item.qty)
                }
            } else {
                item.qty
            };

            // Personal goods → inventory; hauling goods → hands. Fall back to the other
            // bucket if the primary is full (e.g. inventory cap exceeded).
            let leftover = if personal_pickup(item.item.good(), needs) {
                let inv_left = agent.add_item(item.item, take_qty);
                if inv_left > 0 {
                    carrier.try_pick_up(item.item, inv_left)
                } else {
                    0
                }
            } else {
                let hand_left = carrier.try_pick_up(item.item, take_qty);
                if hand_left > 0 {
                    agent.add_item(item.item, hand_left)
                } else {
                    0
                }
            };
            let actually_taken = take_qty - leftover;

            if let Some(ref mut plan) = active_plan_opt {
                plan.reward_acc += actually_taken as f32 * plan.reward_scale;
            }

            if actually_taken >= item.qty {
                commands.entity(target_ent).despawn_recursive();
            } else {
                item.qty -= actually_taken;
            }

            target_item.0 = None;
            finish_scavenge(
                &mut ai,
                &mut aq,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
            );
        } else {
            // Targeted item is gone (stolen or rotted)
            target_item.0 = None;
            finish_scavenge(
                &mut ai,
                &mut aq,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
            );
        }
    }
}

/// Find the highest-multiplier `Item` in `agent.inventory` (and then `carrier`)
/// whose `good` matches `wanted`. Returns the `Item` value (a copy — `Item` is
/// `Copy`) and the bucket it was found in, so the caller can decrement the
/// matching stack. Inventory is preferred over hands so a worker who carries
/// loose stones doesn't accidentally try to wield one when the spear is in
/// inventory.
fn find_best_matching_item(
    agent: &EconomicAgent,
    carrier: &Carrier,
    wanted: crate::economy::resource_catalog::ResourceId,
) -> Option<(Item, EquipSource)> {
    let mut best: Option<(Item, EquipSource)> = None;
    let mut best_mult = f32::NEG_INFINITY;
    for (item, qty) in &agent.inventory {
        if *qty == 0 || item.resource_id != wanted {
            continue;
        }
        let m = item.multiplier();
        if m > best_mult {
            best_mult = m;
            best = Some((*item, EquipSource::Inventory));
        }
    }
    for stack in [carrier.left, carrier.right].iter().flatten() {
        if stack.item.resource_id != wanted {
            continue;
        }
        let m = stack.item.multiplier();
        if m > best_mult {
            best_mult = m;
            best = Some((stack.item, EquipSource::Hands));
        }
    }
    best
}

#[derive(Clone, Copy)]
enum EquipSource {
    Inventory,
    Hands,
}

/// Sequential, after `item_pickup_system`, before `combat_system`.
/// Instant transfer of a single matching `Item` from inventory or Carrier
/// into the target `Equipment` slot. Slot + good come from the typed
/// `Task::Equip { slot, good }` variant (Phase 3d-i), set by the dispatcher
/// when the step's `StepTarget::EquipItem` was committed. If the slot was
/// already occupied, the previous item gets pushed back to inventory; if
/// inventory is full it is dumped as a `GroundItem` at the agent's tile so
/// combat stats aren't silently lost.
pub fn equip_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut q: Query<
        (
            &mut PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &mut EconomicAgent,
            &mut Carrier,
            &mut Equipment,
            &Transform,
            &BucketSlot,
            &LodLevel,
        ),
        With<Person>,
    >,
) {
    for (mut ai, mut aq, mut agent, mut carrier, mut equipment, transform, slot, lod) in q.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::Equip as u16 {
            continue;
        }
        // Equip is in-place — fire as soon as the dispatcher pushes the agent
        // into the task. No routing or arrival check needed (target is SelfPosition).

        let Some((target_slot, wanted)) = aq.current.as_equip() else {
            // Inconsistent state: task_id says Equip but typed task disagrees.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            aq.advance();
            continue;
        };

        let Some((to_equip, source)) = find_best_matching_item(&agent, &carrier, wanted) else {
            // Nothing to wield — bail and let the plan layer record a
            // FailedNoTarget on its next dispatch tick.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            aq.advance();
            continue;
        };

        match source {
            EquipSource::Inventory => {
                agent.remove_item(to_equip, 1);
            }
            EquipSource::Hands => {
                carrier.remove_item(to_equip, 1);
            }
        }

        let displaced = equipment.items.insert(target_slot, to_equip);

        if let Some(prev) = displaced {
            // Try to put the old item back into inventory; spill to ground if
            // inventory can't accept it. Use the full-Item helper so the
            // displaced piece keeps its material/quality/stats — otherwise a
            // looter picking it up later would get a stat-less commodity.
            let leftover = agent.add_item(prev, 1);
            if leftover > 0 {
                let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    prev,
                    leftover,
                );
            }
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        aq.advance();
    }
}
