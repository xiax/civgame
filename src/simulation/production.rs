use super::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::items::GroundItem;
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillKind, Skills};
use super::tasks::TaskKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap, PlantSpriteIndex,
};
use crate::simulation::technology::ActivityKind;
use ahash::AHashMap;
use bevy::prelude::*;

pub const TICKS_FARMER_PLANT: u8 = 40;

const FOOD_NUTRITION: u8 = 255;
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
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&FactionMember>,
    )>,
) {
    for (mut ai, mut agent, mut skills, slot, lod, faction_member) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;
        let task = ai.task_id;

        if task == TaskKind::Planter as u16 {
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

        if agent.is_inventory_full() {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
        }
    }
}

/// Withdraw one edible good from a faction storage tile into the agent's inventory.
/// Driven by `TaskKind::WithdrawFood`. Agent must be standing on the storage tile.
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
        &Transform,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, member, transform, slot, lod) in query.iter_mut() {
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

        let tx = (transform.translation.x / TILE_SIZE).floor() as i16;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i16;

        if storage_tile_map.tiles.get(&(tx, ty)) != Some(&member.faction_id) {
            // Not actually on a storage tile owned by our faction — abort.
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

        // Time to consume.
        let mut consumed_fruit = false;
        for (it, q) in agent.inventory.iter_mut() {
            if it.good.is_edible() && *q > 0 {
                *q -= 1;
                if it.good == Good::Fruit {
                    consumed_fruit = true;
                }
                break;
            }
        }
        needs.hunger = (needs.hunger - FOOD_NUTRITION as f32).max(0.0);
        if consumed_fruit {
            agent.add_good(Good::Seed, 1);
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
    }
}
