use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::carry::Carrier;
use crate::simulation::combat::BodyPart;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::tile_to_world;
use bevy::prelude::*;

#[derive(Component, Clone, Copy, Default, Debug)]
pub struct TargetItem(pub Option<Entity>);

#[derive(Component, Clone, Copy)]
pub struct GroundItem {
    pub item: Item,
    pub qty: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EquipmentSlot {
    MainHand,
    OffHand,
    HeadArmor,
    TorsoArmor,
    LegArmor,
    ArmArmor,
}

#[derive(Component, Default, Clone)]
pub struct Equipment {
    pub items: bevy::utils::HashMap<EquipmentSlot, Entity>,
}

#[derive(Component, Clone, Copy, Debug)]
pub struct WeaponStats {
    pub damage_bonus: u8,
    pub attack_speed: f32,
}

#[derive(Component, Clone, Debug)]
pub struct ArmorStats {
    pub damage_reduction: u8,
    pub coverage: u8,
    pub covered_parts: Vec<BodyPart>,
}

use super::tasks::TaskKind;

/// Deposits `qty` of `good` at tile `(tx, ty)`.
/// Merges into an existing GroundItem of the same type at that tile if found; otherwise spawns new.
pub fn spawn_or_merge_ground_item(
    commands: &mut Commands,
    spatial: &SpatialIndex,
    item_query: &mut Query<&mut GroundItem>,
    tx: i32,
    ty: i32,
    good: Good,
    qty: u32,
) {
    for &entity in spatial.get(tx, ty) {
        if let Ok(mut gi) = item_query.get_mut(entity) {
            if gi.item.good == good {
                gi.qty = gi.qty.saturating_add(qty);
                return;
            }
        }
    }
    let world_pos = tile_to_world(tx, ty);
    commands.spawn((
        GroundItem {
            item: Item::new_commodity(good),
            qty,
        },
        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
    ));
}

/// Recompute `EconomicAgent.bonus_cap_g` from currently-equipped items. Currently
/// the only contributing slot is TorsoArmor (e.g. cloth or armor) which grants a
/// modest pack/pocket bonus. Runs on `Changed<Equipment>`.
pub fn recompute_inventory_capacity_system(
    mut q: Query<(&Equipment, &mut EconomicAgent), Changed<Equipment>>,
    item_lookup: Query<&GroundItem>,
) {
    for (equipment, mut agent) in q.iter_mut() {
        let mut bonus = 0u32;
        for (slot, &entity) in equipment.items.iter() {
            // Only Torso slot grants pack capacity for now.
            if *slot != EquipmentSlot::TorsoArmor {
                continue;
            }
            // If the equipped entity is a known item (Cloth/Armor/etc), grant the bonus.
            if let Ok(gi) = item_lookup.get(entity) {
                bonus = bonus.saturating_add(match gi.item.good {
                    Good::Cloth => 2_000,
                    Good::Armor => 1_000,
                    _ => 0,
                });
            }
        }
        agent.bonus_cap_g = bonus;
    }
}

/// True if this good is "personal" — small enough and useful enough that the agent
/// keeps it in their personal inventory rather than carrying it in their hands.
/// Hungry agents personalize edibles too (so they can eat them later).
fn personal_pickup(good: Good, needs: &Needs) -> bool {
    match good {
        Good::Tools | Good::Seed => true,
        Good::Fruit | Good::Meat | Good::Grain => needs.hunger > 80.0,
        _ => false,
    }
}

/// Sequential, after death_system.
/// Agents explicitly targeting a GroundItem pick it up once they arrive.
pub fn item_pickup_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut item_query: Query<&mut GroundItem>,
    mut pickers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut EconomicAgent,
            &mut Carrier,
            &Needs,
            &mut TargetItem,
            &BucketSlot,
            &LodLevel,
            Option<&mut ActivePlan>,
        ),
        With<Person>,
    >,
) {
    for (
        _entity,
        mut ai,
        mut agent,
        mut carrier,
        needs,
        mut target_item,
        slot,
        lod,
        mut active_plan_opt,
    ) in pickers.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::Scavenge as u16 {
            continue;
        }

        let Some(target_ent) = target_item.0 else {
            // No target, but in scavenge state? Cleanup.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            continue;
        };

        if let Ok(mut item) = item_query.get_mut(target_ent) {
            let take_qty = if item.item.good.is_edible() {
                let nutrition = item.item.good.nutrition();
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
            let leftover = if personal_pickup(item.item.good, needs) {
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
                commands.entity(target_ent).despawn();
            } else {
                item.qty -= actually_taken;
            }

            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
        } else {
            // Targeted item is gone (stolen or rotted)
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
        }
    }
}
