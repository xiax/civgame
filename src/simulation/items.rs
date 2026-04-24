use bevy::prelude::*;
use crate::economy::agent::EconomicAgent;
use crate::economy::item::Item;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::simulation::combat::BodyPart;

#[derive(Component, Clone, Copy, Default, Debug)]
pub struct TargetItem(pub Option<Entity>);

#[derive(Component, Clone, Copy)]
pub struct GroundItem {
    pub item: Item,
    pub qty:  u8,
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

use super::jobs::JobKind;

/// Sequential, after death_system.
/// Agents explicitly targeting a GroundItem pick it up once they arrive.
pub fn item_pickup_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    item_query: Query<&GroundItem>,
    mut pickers: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut TargetItem,
        &BucketSlot,
        &LodLevel,
        Option<&mut ActivePlan>,
        &Transform,
    ), With<Person>>,
) {
    for (
        entity, mut ai, mut agent, mut target_item, slot, lod,
        mut active_plan_opt, transform
    ) in pickers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
        if ai.state != AiState::Working || ai.job_id != JobKind::Scavenge as u16 { continue; }

        let Some(target_ent) = target_item.0 else {
            // No target, but in scavenge state? Cleanup.
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            continue;
        };

        if let Ok(item) = item_query.get(target_ent) {
            // Check if we are at the target
            let tx = (transform.translation.x / TILE_SIZE).floor() as i16;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i16;

            if ai.target_tile == (tx, ty) {
                // Intentional pickup
                agent.add_good(item.item.good, item.qty);
                
                if let Some(ref mut plan) = active_plan_opt {
                    plan.reward_acc += item.qty as f32 * plan.reward_scale;
                }

                commands.entity(target_ent).despawn();
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
                target_item.0 = None;
            }
        } else {
            // Targeted item is gone (stolen or rotted)
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            target_item.0 = None;
        }
    }
}
