use ahash::AHashMap;
use bevy::prelude::*;

use crate::simulation::faction::{FactionChief, FactionMember, FactionRegistry, Lifestyle};
use crate::simulation::goals::AgentGoal;
use crate::simulation::jobs::JobClaim;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Drafted, Person, PersonAI, Profession, TraderPlan};
use crate::simulation::schedule::SimClock;
use crate::simulation::typed_task::{ActionQueue, Task};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgeBand {
    Child,
    Adult,
    Elder,
}

impl Default for AgeBand {
    fn default() -> Self {
        Self::Adult
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WealthBand {
    Poor,
    Stable,
    Wealthy,
}

impl Default for WealthBand {
    fn default() -> Self {
        Self::Stable
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CohortKey {
    pub faction_id: u32,
    pub settlement_or_camp: Option<u32>,
    pub profession: Profession,
    pub age_band: AgeBand,
    pub wealth_band: WealthBand,
    pub lifestyle: LifestyleTag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LifestyleTag {
    Settled,
    Nomadic,
}

impl From<Lifestyle> for LifestyleTag {
    fn from(value: Lifestyle) -> Self {
        match value {
            Lifestyle::Settled => Self::Settled,
            Lifestyle::Nomadic => Self::Nomadic,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CohortState {
    pub population: u32,
    pub avg_hunger: f32,
    pub avg_sleep: f32,
    pub avg_mood: f32,
    pub avg_wealth: f32,
    pub skill_means: [f32; crate::simulation::skills::SKILL_COUNT],
    pub disposition_mean: crate::simulation::goal_scorers::Disposition,
    pub food_consumed: f32,
    pub food_produced: f32,
    pub births: u32,
    pub deaths: u32,
}

impl Default for CohortState {
    fn default() -> Self {
        Self {
            population: 0,
            avg_hunger: 0.0,
            avg_sleep: 0.0,
            avg_mood: 0.0,
            avg_wealth: 0.0,
            skill_means: [0.0; crate::simulation::skills::SKILL_COUNT],
            disposition_mean: crate::simulation::goal_scorers::Disposition::default(),
            food_consumed: 0.0,
            food_produced: 0.0,
            births: 0,
            deaths: 0,
        }
    }
}

impl CohortState {
    pub fn observe_agent(
        &mut self,
        needs: &Needs,
        wealth: f32,
        skills: &crate::simulation::skills::Skills,
        disposition: crate::simulation::goal_scorers::Disposition,
    ) {
        let n = self.population as f32;
        self.population += 1;
        let next = self.population as f32;
        self.avg_hunger = (self.avg_hunger * n + needs.hunger) / next;
        self.avg_sleep = (self.avg_sleep * n + needs.sleep) / next;
        self.avg_wealth = (self.avg_wealth * n + wealth) / next;
        for i in 0..crate::simulation::skills::SKILL_COUNT {
            self.skill_means[i] = (self.skill_means[i] * n + skills.0[i] as f32) / next;
        }
        self.disposition_mean = mean_disposition(self.disposition_mean, n, disposition, next);
    }
}

fn mean_disposition(
    current: crate::simulation::goal_scorers::Disposition,
    old_n: f32,
    incoming: crate::simulation::goal_scorers::Disposition,
    new_n: f32,
) -> crate::simulation::goal_scorers::Disposition {
    let avg = |a: u8, b: u8| -> u8 { ((a as f32 * old_n + b as f32) / new_n).round() as u8 };
    crate::simulation::goal_scorers::Disposition {
        entrepreneurial: avg(current.entrepreneurial, incoming.entrepreneurial),
        gregariousness: avg(current.gregariousness, incoming.gregariousness),
        curiosity: avg(current.curiosity, incoming.curiosity),
        martial: avg(current.martial, incoming.martial),
    }
}

#[derive(Resource, Default, Debug)]
pub struct CohortRegistry {
    pub cohorts: AHashMap<CohortKey, CohortState>,
    pub rebuilt_at_tick: u64,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedFullSim {
    pub reason: FullSimPinReason,
    pub since_tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullSimPinReason {
    PlayerCritical,
    Commanded,
    Chief,
    Drafted,
    Combat,
}

pub fn cohort_pin_full_sim_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    q: Query<
        (
            Entity,
            &PersonAI,
            &AgentGoal,
            Option<&FactionChief>,
            Option<&crate::simulation::player_command::Commanded>,
            Option<&Drafted>,
            Option<&PinnedFullSim>,
        ),
        With<Person>,
    >,
) {
    for (entity, ai, goal, chief, commanded, drafted, existing_pin) in q.iter() {
        let reason = if commanded.is_some() || matches!(*goal, AgentGoal::FollowingPlayerCommand) {
            Some(FullSimPinReason::Commanded)
        } else if drafted.is_some() {
            Some(FullSimPinReason::Drafted)
        } else if chief.is_some() {
            Some(FullSimPinReason::Chief)
        } else if matches!(ai.state, AiState::Attacking) {
            Some(FullSimPinReason::Combat)
        } else {
            None
        };
        if let Some(reason) = reason {
            if existing_pin.map(|pin| pin.reason) != Some(reason) {
                commands.entity(entity).insert(PinnedFullSim {
                    reason,
                    since_tick: clock.tick,
                });
            }
        } else if existing_pin.is_some() {
            commands.entity(entity).remove::<PinnedFullSim>();
        }
    }
}

pub fn can_demote_to_cohort(
    ai: &PersonAI,
    goal: AgentGoal,
    lod: LodLevel,
    aq: &ActionQueue,
    has_pin: bool,
    has_claim: bool,
    has_trader_plan: bool,
) -> bool {
    if lod != LodLevel::Aggregate {
        return false;
    }
    if has_pin || has_claim || has_trader_plan {
        return false;
    }
    if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
        return false;
    }
    if !matches!(aq.current, Task::Idle) || !aq.queued_is_empty() {
        return false;
    }
    !matches!(
        goal,
        AgentGoal::FollowingPlayerCommand
            | AgentGoal::Raid
            | AgentGoal::Defend
            | AgentGoal::Rescue
            | AgentGoal::MigrateToCamp
            | AgentGoal::Scout
    )
}

pub fn rebuild_cohort_registry_system(
    clock: Res<SimClock>,
    factions: Res<FactionRegistry>,
    mut registry: ResMut<CohortRegistry>,
    q: Query<
        (
            &LodLevel,
            &FactionMember,
            &Profession,
            &Needs,
            &crate::economy::agent::EconomicAgent,
            &crate::simulation::skills::Skills,
            Option<&crate::simulation::goal_scorers::Disposition>,
        ),
        With<Person>,
    >,
) {
    if clock.tick % 20 != 0 {
        return;
    }
    registry.cohorts.clear();
    registry.rebuilt_at_tick = clock.tick;
    for (lod, member, profession, needs, economy, skills, disposition) in q.iter() {
        if *lod != LodLevel::Aggregate {
            continue;
        }
        let lifestyle = factions
            .factions
            .get(&member.faction_id)
            .map(|f| f.lifestyle)
            .unwrap_or_default();
        let key = CohortKey {
            faction_id: member.faction_id,
            settlement_or_camp: None,
            profession: *profession,
            age_band: AgeBand::Adult,
            wealth_band: wealth_band(economy.currency),
            lifestyle: lifestyle.into(),
        };
        registry.cohorts.entry(key).or_default().observe_agent(
            needs,
            economy.currency,
            skills,
            disposition.copied().unwrap_or_default(),
        );
    }
}

fn wealth_band(currency: f32) -> WealthBand {
    if currency >= 100.0 {
        WealthBand::Wealthy
    } else if currency <= 5.0 {
        WealthBand::Poor
    } else {
        WealthBand::Stable
    }
}

#[allow(clippy::type_complexity)]
pub fn cohort_demote_candidate_count_system(
    clock: Res<SimClock>,
    mut registry: ResMut<CohortRegistry>,
    q: Query<
        (
            &PersonAI,
            &AgentGoal,
            &LodLevel,
            &ActionQueue,
            Option<&PinnedFullSim>,
            Option<&JobClaim>,
            Option<&TraderPlan>,
        ),
        With<Person>,
    >,
) {
    if clock.tick % 20 != 0 {
        return;
    }
    let count = q
        .iter()
        .filter(|(ai, goal, lod, aq, pin, claim, trader)| {
            can_demote_to_cohort(
                ai,
                **goal,
                **lod,
                aq,
                pin.is_some(),
                claim.is_some(),
                trader.is_some(),
            )
        })
        .count() as u32;
    // Store the candidate count in a synthetic summary row so the
    // debug surface can verify demotion eligibility without mutating
    // live agents yet.
    if count > 0 {
        let key = CohortKey {
            faction_id: 0,
            settlement_or_camp: None,
            profession: Profession::None,
            age_band: AgeBand::Adult,
            wealth_band: WealthBand::Stable,
            lifestyle: LifestyleTag::Settled,
        };
        registry.cohorts.entry(key).or_default().population = count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demotion_requires_aggregate_idle_unpinned_agent() {
        let ai = PersonAI {
            task_id: PersonAI::UNEMPLOYED,
            state: AiState::Idle,
            ..PersonAI::default()
        };
        let aq = ActionQueue::idle();
        assert!(can_demote_to_cohort(
            &ai,
            AgentGoal::GatherFood,
            LodLevel::Aggregate,
            &aq,
            false,
            false,
            false
        ));
        assert!(!can_demote_to_cohort(
            &ai,
            AgentGoal::GatherFood,
            LodLevel::Full,
            &aq,
            false,
            false,
            false
        ));
        assert!(!can_demote_to_cohort(
            &ai,
            AgentGoal::FollowingPlayerCommand,
            LodLevel::Aggregate,
            &aq,
            false,
            false,
            false
        ));
        assert!(!can_demote_to_cohort(
            &ai,
            AgentGoal::GatherFood,
            LodLevel::Aggregate,
            &aq,
            true,
            false,
            false
        ));
    }

    #[test]
    fn cohort_observation_averages_needs_and_wealth() {
        let mut state = CohortState::default();
        let mut needs_a = Needs::default();
        needs_a.hunger = 10.0;
        needs_a.sleep = 20.0;
        let mut needs_b = Needs::default();
        needs_b.hunger = 30.0;
        needs_b.sleep = 60.0;
        let skills = crate::simulation::skills::Skills::default();
        state.observe_agent(
            &needs_a,
            5.0,
            &skills,
            crate::simulation::goal_scorers::Disposition::default(),
        );
        state.observe_agent(
            &needs_b,
            15.0,
            &skills,
            crate::simulation::goal_scorers::Disposition::default(),
        );
        assert_eq!(state.population, 2);
        assert!((state.avg_hunger - 20.0).abs() < f32::EPSILON);
        assert!((state.avg_sleep - 40.0).abs() < f32::EPSILON);
        assert!((state.avg_wealth - 10.0).abs() < f32::EPSILON);
    }
}
