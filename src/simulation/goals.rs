use bevy::prelude::*;
use super::construction::AutonomousBuildingToggle;
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI, PlayerOrder};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
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

pub fn goal_update_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    calendar: Res<Calendar>,
    auto_build: Res<AutonomousBuildingToggle>,
    mut query: Query<(
        &mut AgentGoal,
        &Needs,
        &Personality,
        &EconomicAgent,
        &PersonAI,
        &BucketSlot,
        &LodLevel,
        &FactionMember,
    ), Without<PlayerOrder>>,
) {
    query.par_iter_mut().for_each(|(mut goal, needs, personality, agent, ai, slot, lod, member)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }

        // Only update goal periodically or when idle to prevent excessive jitter
        if ai.job_id != PersonAI::UNEMPLOYED && clock.tick % 32 != 0 {
            return;
        }
        
        // Don't interrupt combat or sleep
        if matches!(ai.state, AiState::Attacking | AiState::Sleeping) {
            return;
        }

        // Faction war state overrides individual needs
        if member.faction_id != SOLO {
            if registry.is_under_raid(member.faction_id) {
                *goal = AgentGoal::Defend;
                return;
            }
            if registry.raid_target(member.faction_id).is_some() && needs.hunger < 160.0 {
                *goal = AgentGoal::Raid;
                return;
            }
        }

        let social_threshold    = if *personality == Personality::Socialite { 120.0 } else { 160.0 };
        let reproduce_threshold = if *personality == Personality::Nurturer  { 140.0 } else { 180.0 };

        let can_return_camp = member.faction_id != SOLO && {
            let per_member: f32 = match calendar.season {
                Season::Summer => 30.0,
                Season::Autumn => 25.0,
                Season::Spring => 15.0,
                Season::Winter =>  5.0,
            };
            let cap = registry.factions.get(&member.faction_id)
                .map(|f| f.member_count as f32 * per_member)
                .unwrap_or(0.0);
            registry.food_stock(member.faction_id) < cap
        };

        let faction_has_food = member.faction_id != SOLO && registry.food_stock(member.faction_id) >= 1.0;
        let is_starving = needs.hunger > 100.0 && agent.quantity_of(Good::Food) == 0;

        let new_goal = if is_starving && faction_has_food {
            AgentGoal::ReturnCamp
        } else if needs.hunger > 120.0 || (needs.hunger > 60.0 && agent.quantity_of(Good::Food) < 3) {
            AgentGoal::Survive
        } else if needs.sleep > 180.0 {
            AgentGoal::Sleep
        } else if agent.quantity_of(Good::Food) > 0 && can_return_camp {
            AgentGoal::ReturnCamp
        } else if needs.shelter > 80.0 && member.faction_id != SOLO && auto_build.0 {
            AgentGoal::Build
        } else if needs.reproduction > reproduce_threshold {
            AgentGoal::Reproduce
        } else if needs.social > social_threshold {
            AgentGoal::Socialize
        } else {
            AgentGoal::Gather
        };

        *goal = new_goal;
    });
}
