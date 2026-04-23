use ahash::AHashMap;
use bevy::prelude::*;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap,
    PlantSpriteIndex,
};
use super::faction::{FactionMember, FactionRegistry};
use super::jobs::JobKind;
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillKind, Skills};
use crate::simulation::technology::ActivityKind;
use crate::rendering::pixel_art::EntityTextures;

pub const TICKS_FARMER_PLANT: u8 = 40;

const HUNGER_EAT_THRESHOLD: u8 = 100;
const FOOD_NUTRITION:        u8 = 40;

// Tile depletion — tracks how many times each tile has been harvested recently.
// Absent from map = fully recovered. Higher value = more depleted.
const REGEN_INTERVAL:         u64 = 2000; // ticks between each +1 recovery per tile

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
    if clock.tick % REGEN_INTERVAL != 0 { return; }
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
    textures: Res<EntityTextures>,
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
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
        if ai.state != AiState::Working { continue; }

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;
        let job = ai.job_id;

        if job == JobKind::Planter as u16 {
            if ai.work_progress >= TICKS_FARMER_PLANT {
                ai.work_progress = 0;
                if !plant_map.0.contains_key(&(tx, ty)) && agent.quantity_of(Good::Seed) > 0 {
                    agent.remove_good(Good::Seed, 1);
                    spawn_plant_at(
                        &mut commands,
                        &mut plant_map,
                        &mut plant_sprite_index,
                        &textures,
                        tx, ty,
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
                ai.job_id = PersonAI::UNEMPLOYED;
            } else {
                // Check if tile is still valid for planting
                if plant_map.0.contains_key(&(tx, ty)) {
                    ai.state = AiState::Idle;
                    ai.job_id = PersonAI::UNEMPLOYED;
                }
            }
        }

        if agent.is_inventory_full() {
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
        }
    }
}

pub fn consumption_system(
    clock: Res<SimClock>,
    mut query: Query<(
        &mut EconomicAgent,
        &mut Needs,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    query.par_iter_mut().for_each(|(mut agent, mut needs, slot, lod)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }
        if needs.hunger > HUNGER_EAT_THRESHOLD as f32 && agent.quantity_of(Good::Food) > 0 {
            agent.remove_good(Good::Food, 1);
            needs.hunger = (needs.hunger - FOOD_NUTRITION as f32).max(0.0);
        }
    });
}
