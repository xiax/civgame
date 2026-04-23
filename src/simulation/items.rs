use bevy::prelude::*;
use crate::economy::agent::EconomicAgent;
use crate::economy::item::Item;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::simulation::combat::BodyPart;

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
}

#[derive(Component, Clone, Debug)]
pub struct ArmorStats {
    pub damage_reduction: u8,
    pub coverage: u8,
    pub covered_parts: Vec<BodyPart>,
}

/// Sequential, after death_system.
/// Iterates ground items; the first active Working agent with a food goal at the
/// same tile claims the item.  Only one agent claims each item per frame.
pub fn item_pickup_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    item_query: Query<(Entity, &GroundItem, &Transform)>,
    mut pickers: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &AgentGoal,
        &BucketSlot,
        &LodLevel,
    ), With<Person>>,
) {
    for (item_e, item, item_transform) in &item_query {
        let tx = (item_transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (item_transform.translation.y / TILE_SIZE).floor() as i32;

        for &candidate in spatial.get(tx, ty) {
            let Ok((mut ai, mut agent, goal, slot, lod)) = pickers.get_mut(candidate) else {
                continue;
            };
            if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
            if ai.state != AiState::Working { continue; }
                        if !matches!(goal, AgentGoal::Survive | AgentGoal::Gather) { continue; }

            agent.add_good(item.item.good, item.qty);
            commands.entity(item_e).despawn();
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            break;
        }
    }
}
