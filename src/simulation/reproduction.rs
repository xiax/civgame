use bevy::prelude::*;
use ahash::AHashMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use crate::economy::agent::EconomicAgent;
use super::combat::{CombatTarget, Body};
use super::items::Equipment;
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::goals::{AgentGoal, Personality};
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::neural::UtilityNet;
use super::plan::{KnownPlans, PlanScoringMethod};
use super::person::{AiState, Person, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BiologicalSex {
    Male   = 0,
    Female = 1,
}

impl BiologicalSex {
    pub fn random() -> Self {
        if fastrand::bool() { BiologicalSex::Male } else { BiologicalSex::Female }
    }

    pub fn name(self) -> &'static str {
        match self {
            BiologicalSex::Male   => "Male",
            BiologicalSex::Female => "Female",
        }
    }
}

const REPRODUCE_FEMALE_THRESHOLD: u8 = 180;
const REPRODUCE_MALE_THRESHOLD:   u8 = 150;
const BIRTH_CHANCE:                u32 = 5;   // out of 10,000 per tick
const BIRTH_COOLDOWN_TICKS:        u32 = 324_000; // 90 in-game days

/// Eligible males this tick: entity → faction_id. Updated by collect_male_candidates.
#[derive(Resource, Default)]
pub struct MaleCandidates(pub AHashMap<Entity, u32>);

pub fn collect_male_candidates(
    mut candidates: ResMut<MaleCandidates>,
    query: Query<(Entity, &BiologicalSex, &FactionMember, &Needs)>,
) {
    candidates.0.clear();
    for (entity, sex, member, needs) in &query {
        if *sex == BiologicalSex::Male
            && member.faction_id != SOLO
            && needs.reproduction >= REPRODUCE_MALE_THRESHOLD as f32
        {
            candidates.0.insert(entity, member.faction_id);
        }
    }
}

pub fn birth_cooldown_system(
    clock: Res<SimClock>,
    mut query: Query<(&mut FactionMember, &BucketSlot, &LodLevel)>,
) {
    query.par_iter_mut().for_each(|(mut member, slot, lod)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }
        if member.birth_cooldown > 0 {
            member.birth_cooldown = member.birth_cooldown.saturating_sub(1);
        }
    });
}

pub fn reproduction_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    candidates: Res<MaleCandidates>,
    mut clock: ResMut<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    father_net_query: Query<Option<&UtilityNet>>,
    mut query: Query<(
        Entity,
        &mut Needs,
        &mut FactionMember,
        &BiologicalSex,
        &AgentGoal,
        &Transform,
        &LodLevel,
        &BucketSlot,
        Option<&UtilityNet>,
    )>,
) {
    let mut births: Vec<(Transform, u32, UtilityNet)> = Vec::new();
    let mut resets: Vec<Entity> = Vec::new();

    // Use a temporary vector to avoid borrowing query while mutating it via get_mut
    let mut found_pairs = Vec::new();

    for (entity, needs, member, sex, goal, transform, lod, slot, mother_net) in query.iter() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *sex != BiologicalSex::Female { continue; }
        if *goal != AgentGoal::Reproduce { continue; }
        if needs.reproduction < REPRODUCE_FEMALE_THRESHOLD as f32 { continue; }
        if member.birth_cooldown > 0 { continue; }
        if member.faction_id == SOLO { continue; }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let faction_id = member.faction_id;

        let mut found_father: Option<Entity> = None;
        'search: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other != entity
                        && candidates.0.get(&other).copied() == Some(faction_id)
                    {
                        found_father = Some(other);
                        break 'search;
                    }
                }
            }
        }

        if let Some(father_entity) = found_father {
            found_pairs.push((entity, father_entity, *transform, faction_id, mother_net.cloned()));
        }
    }

    for (mother_ent, father_ent, transform, faction_id, mother_net) in found_pairs {
        // Reset needs for both regardless of birth
        if let Ok([mut m_needs, mut f_needs]) = query.get_many_mut([mother_ent, father_ent]) {
            m_needs.1.reproduction = 0.0;
            f_needs.1.reproduction = 0.0;
            
            // Set cooldown on mother
            m_needs.2.birth_cooldown = BIRTH_COOLDOWN_TICKS;
        }

        // Roll for birth
        if fastrand::u32(..10_000) < BIRTH_CHANCE {
            let father_net = father_net_query.get(father_ent).ok().flatten();
            let child_net = match (mother_net, father_net) {
                (Some(m), Some(f)) => UtilityNet::from_parents(&m, f),
                (Some(p), None)    => UtilityNet::from_parent(&p),
                (None, Some(p))    => UtilityNet::from_parent(p),
                (None, None)       => UtilityNet::new_random(),
            };
            births.push((transform, faction_id, child_net));
        }
    }

    for (parent_transform, faction_id, child_net) in births {
        let new_slot = clock.population;
        clock.population += 1;
        clock.bucket_size = clock.population.min(10_000);

        let tx = (parent_transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (parent_transform.translation.y / TILE_SIZE).floor() as i32;
        let world_pos = tile_to_world(tx, ty);

        registry.add_member(faction_id);

        commands.spawn((
            (
                Person,
                Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                GlobalTransform::default(),
                Needs::new(0.0, 0.0, 0.0, 0.0, 0.0),
                Mood::default(),
                Skills::default(),
                PersonAI {
                    job_id: PersonAI::UNEMPLOYED,
                    state: AiState::Idle,
                    target_tile: (tx as i16, ty as i16),
                    dest_tile: (tx as i16, ty as i16),
                    ticks_idle: 0,
                    work_progress: 0,
                    last_plan_id: PersonAI::UNEMPLOYED,
                    last_goal_eval_tick: 0,
                    target_entity: None,
                },
                EconomicAgent::default(),
            ),
            (
                LodLevel::Full,
                BucketSlot(new_slot),
                MovementState { wander_timer: 0.0 },
                BiologicalSex::random(),
                Personality::random(),
                AgentGoal::default(),
                FactionMember { faction_id, ..Default::default() },
                Body::new_humanoid(),
                Equipment::default(),
                CombatTarget::default(),
            ),
            (
                AgentMemory::default(),
                RelationshipMemory::default(),
                child_net,
                KnownPlans::with_innate(&[0, 1]),
                PlanScoringMethod::UtilityNN,
            ),
        ));
    }
}
