use bevy::prelude::*;
use super::construction::{AutonomousBuildingToggle, Blueprint, BlueprintMap};
use crate::world::chunk::ChunkMap;
use crate::simulation::plants::{PlantMap, Plant, GrowthStage};
use crate::simulation::items::{GroundItem, TargetItem};
use crate::simulation::tasks::TaskKind;
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI, PlayerOrder};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::world::seasons::{Calendar, Season};

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Personality {
    #[default]
    Gatherer  = 0,
    Socialite = 1,
    Explorer  = 2,
    Nurturer  = 3,
}

impl Personality {
    pub fn name(self) -> &'static str {
        match self {
            Personality::Gatherer  => "Gatherer",
            Personality::Socialite => "Socialite",
            Personality::Explorer  => "Explorer",
            Personality::Nurturer  => "Nurturer",
        }
    }

    pub fn random() -> Self {
        match fastrand::u8(..4) {
            0 => Personality::Gatherer,
            1 => Personality::Socialite,
            2 => Personality::Explorer,
            _ => Personality::Nurturer,
        }
    }
}

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AgentGoal {
    #[default]
    Gather     = 0,
    Survive    = 1,
    ReturnCamp = 2,
    Socialize  = 3,
    Reproduce  = 4,
    Raid       = 5,
    Defend     = 6,
    Sleep      = 7,
    Build      = 8,
}

impl AgentGoal {
    pub fn name(self) -> &'static str {
        match self {
            AgentGoal::Gather     => "Gather",
            AgentGoal::Survive    => "Survive",
            AgentGoal::ReturnCamp => "ReturnCamp",
            AgentGoal::Socialize  => "Socialize",
            AgentGoal::Reproduce  => "Reproduce",
            AgentGoal::Raid       => "Raid",
            AgentGoal::Defend     => "Defend",
            AgentGoal::Sleep      => "Sleep",
            AgentGoal::Build      => "Build",
        }
    }
}

#[derive(Component, Default)]
pub struct GoalReason(pub &'static str);

pub fn goal_update_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    calendar: Res<Calendar>,
    auto_build: Res<AutonomousBuildingToggle>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    item_query: Query<(), With<GroundItem>>,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    mut query: Query<(
        Entity,
        &mut AgentGoal,
        &Needs,
        &Personality,
        &EconomicAgent,
        &mut PersonAI,
        &BucketSlot,
        &LodLevel,
        &FactionMember,
        &mut TargetItem,
        Option<&mut GoalReason>,
    ), Without<PlayerOrder>>,
) {
    for (entity, mut goal, needs, personality, agent, mut ai, slot, lod, member, mut target_item, reason_opt) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        // 1. Cooldown & Staggered Update Fix
        if ai.task_id != PersonAI::UNEMPLOYED && clock.tick.saturating_sub(ai.last_goal_eval_tick) < 32 {
            continue;
        }
        ai.last_goal_eval_tick = clock.tick;

        // 2. Target Validation (for moving agents)
        if matches!(ai.state, AiState::Routing | AiState::Seeking) {
            let mut invalid = false;
            let tid = ai.task_id;

            if tid == TaskKind::Gather as u16 {
                // If targeting a plant, check if it still exists and is mature
                if let Some(ent) = ai.target_entity {
                    if let Ok(plant) = plant_query.get(ent) {
                        if plant.stage != GrowthStage::Mature { invalid = true; }
                    } else {
                        invalid = true;
                    }
                } else {
                    // Targeting a tile (Stone)
                    let tx = ai.dest_tile.0 as i32;
                    let ty = ai.dest_tile.1 as i32;
                    if !matches!(chunk_map.tile_kind_at(tx, ty), Some(crate::world::tile::TileKind::Stone)) {
                        invalid = true;
                    }
                }
            } else if tid == TaskKind::Planter as u16 {
                let tx = ai.dest_tile.0 as i32;
                let ty = ai.dest_tile.1 as i32;
                if plant_map.0.contains_key(&(tx, ty)) {
                    invalid = true;
                }
            } else if tid == TaskKind::Scavenge as u16 {
                if let Some(ent) = ai.target_entity {
                    if item_query.get(ent).is_err() { invalid = true; }
                } else {
                    invalid = true;
                }
            } else if tid == TaskKind::Construct as u16 || tid == TaskKind::ConstructBed as u16 {
                // Invalidate if the target blueprint entity no longer exists.
                match ai.target_entity {
                    Some(ent) if bp_query.get(ent).is_err() => { invalid = true; }
                    None => { invalid = true; }
                    _ => {}
                }
            }

            if invalid {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                target_item.0 = None;
            }
        }

        // Don't interrupt combat or sleep
        if matches!(ai.state, AiState::Attacking | AiState::Sleeping) {
            continue;
        }

        // Faction war state overrides individual needs
        if member.faction_id != SOLO {
            if registry.is_under_raid(member.faction_id) {
                if *goal != AgentGoal::Defend {
                    *goal = AgentGoal::Defend;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt { r.0 = "Under Raid"; } else { commands.entity(entity).insert(GoalReason("Under Raid")); }
                continue;
            }
            if registry.raid_target(member.faction_id).is_some() && needs.hunger < 120.0 {
                if *goal != AgentGoal::Raid {
                    *goal = AgentGoal::Raid;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt { r.0 = "Participating in Raid"; } else { commands.entity(entity).insert(GoalReason("Participating in Raid")); }
                continue;
            }
        }

        let social_threshold    = if *personality == Personality::Socialite { 120.0 } else { 160.0 };
        let reproduce_threshold = if *personality == Personality::Nurturer  { 140.0 } else { 180.0 };

        let (faction_food_ratio, can_return_camp) = if member.faction_id != SOLO {
            let per_member: f32 = match calendar.season {
                Season::Summer => 30.0,
                Season::Autumn => 25.0,
                Season::Spring => 15.0,
                Season::Winter =>  5.0,
            };
            let cap = registry.factions.get(&member.faction_id)
                .map(|f| f.member_count as f32 * per_member)
                .unwrap_or(0.0);
            
            let stock = registry.food_stock(member.faction_id);
            let ratio = if cap > 0.0 { stock / cap } else { 1.0 };
            (ratio, stock < cap)
        } else {
            (1.0, false)
        };

        let faction_has_food = member.faction_id != SOLO && registry.food_stock(member.faction_id) >= 1.0;
        let is_starving = needs.hunger > 100.0 && agent.total_food() == 0;

        let has_build_site = member.faction_id != SOLO
            && auto_build.0
            && bp_map.0.values().any(|&bp_e|
                bp_query.get(bp_e).map(|bp| bp.faction_id == member.faction_id).unwrap_or(false)
            );

        // Stable probabilistic selection: agents with high hash_val will gather if food ratio is low.
        let agent_hash_val = ((entity.index() as u64 * 2654435761) % 100) as f32 / 100.0;
        let prioritize_gather = faction_food_ratio < 1.0 && agent_hash_val > faction_food_ratio;

        let (new_goal, reason) = if is_starving && faction_has_food {
            (AgentGoal::ReturnCamp, "Starving (Faction has food)")
        } else if needs.hunger > 120.0 || (needs.hunger > 60.0 && agent.total_food() < 3) {
            (AgentGoal::Survive, "Hungry")
        } else if needs.sleep > 180.0 {
            (AgentGoal::Sleep, "Tired")
        } else if agent.total_food() > 0 && can_return_camp {
            (AgentGoal::ReturnCamp, "Returning Surplus Food")
        } else if needs.reproduction > reproduce_threshold {
            (AgentGoal::Reproduce, "Reproduction Need")
        } else if needs.social > social_threshold {
            (AgentGoal::Socialize, "Social Need")
        } else if prioritize_gather {
            (AgentGoal::Gather, "Prioritized Gathering (Food Low)")
        } else if has_build_site {
            (AgentGoal::Build, "Building Projects")
        } else {
            (AgentGoal::Gather, "General Gathering")
        };

        if *goal != new_goal {
            *goal = new_goal;
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
        }
        if let Some(mut r) = reason_opt {
            r.0 = reason;
        } else {
            commands.entity(entity).insert(GoalReason(reason));
        }
    }
}

