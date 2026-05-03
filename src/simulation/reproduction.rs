use super::carry::Carrier;
use super::combat::{Body, CombatCooldown, CombatTarget};
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::goals::{AgentGoal, Personality};
use super::items::{Equipment, TargetItem};
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::person::{
    generate_person_name, AiState, HairColor, Person, PersonAI, Profession, SkinTone,
};
use super::plan::{KnownPlans, PlanHistory, PlanScoringMethod};
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;
use super::stats::Stats;
use crate::economy::agent::EconomicAgent;
use crate::pathfinding::path_request::PathFollow;
use crate::world::seasons::TICKS_PER_SEASON;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use bevy::prelude::*;

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BiologicalSex {
    Male = 0,
    Female = 1,
}

impl BiologicalSex {
    pub fn random() -> Self {
        if fastrand::bool() {
            BiologicalSex::Male
        } else {
            BiologicalSex::Female
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            BiologicalSex::Male => "Male",
            BiologicalSex::Female => "Female",
        }
    }
}

/// Pregnancy lasts three seasons (54,000 ticks at 20Hz / 5-day seasons).
pub const PREGNANCY_TICKS: u32 = TICKS_PER_SEASON * 3;
/// Tile radius (Chebyshev) within which a sleeping pair counts as co-sleeping.
const COSLEEP_RADIUS: i32 = 3;
/// Minimum overlapping sleep ticks before a co-sleep "counts" as a night —
/// filters out brief wake-and-resettle moments.
const MIN_COSLEEP_TICKS: u16 = 100;
/// After participating in an attempt, a male is on cooldown for this many ticks.
/// Prevents a single male from servicing every co-sleeping woman in one morning.
const MALE_ATTEMPT_COOLDOWN_TICKS: u32 = 600;

/// First-class pregnancy state. Inserted on a female on a successful conception
/// roll; removed by `pregnancy_system` when the timer reaches 0 and the child
/// is spawned. Carries snapshots of father data so the birth survives the
/// father's death or the mother's faction reassignment mid-pregnancy.
#[derive(Component, Clone)]
pub struct Pregnancy {
    pub ticks_remaining: u32,
    pub father: Option<Entity>,
    pub father_stats: Option<Stats>,
    pub faction_id: u32,
}

impl Pregnancy {
    pub fn new(father: Entity, father_stats: Option<Stats>, faction_id: u32) -> Self {
        Self {
            ticks_remaining: PREGNANCY_TICKS,
            father: Some(father),
            father_stats,
            faction_id,
        }
    }
}

/// Per-agent tracking of who they're currently co-sleeping with. Updated by
/// `cosleep_observation_system` while the agent is in `AiState::Sleeping`;
/// consumed and cleared by `wake_up_conception_system` on the
/// `Sleeping → !Sleeping` transition.
#[derive(Component, Default, Clone, Debug)]
pub struct CoSleepTracker {
    pub partner: Option<Entity>,
    pub ticks_co_slept: u16,
    /// Last observed AI state, for transition detection in
    /// `wake_up_conception_system`. Initialised to `Idle` so newly-spawned
    /// agents register their first sleep cycle naturally.
    pub prev_state: AiState,
}

/// Refractory period for a male after participating in a conception attempt.
/// Set to `MALE_ATTEMPT_COOLDOWN_TICKS` at attempt time, decremented each tick.
/// Idle for non-males (the field is always present but only consumed on the
/// male side of an attempt).
#[derive(Component, Default, Clone, Debug)]
pub struct MaleConceptionCooldown(pub u32);

/// Per-tick observation of co-sleeping pairs. Runs in
/// `SimulationSet::Sequential` after the spatial index is up to date. Also
/// decrements `MaleConceptionCooldown` for active (non-Dormant) agents — folded
/// in here to avoid an extra system pass.
pub fn cosleep_observation_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    others: Query<(&PersonAI, &BiologicalSex, &FactionMember), With<Person>>,
    mut query: Query<
        (
            Entity,
            &PersonAI,
            &Transform,
            &BiologicalSex,
            &FactionMember,
            &LodLevel,
            &BucketSlot,
            &mut CoSleepTracker,
            &mut MaleConceptionCooldown,
        ),
        With<Person>,
    >,
) {
    for (entity, ai, transform, sex, member, lod, slot, mut tracker, mut male_cd) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }

        // Decrement male cooldown every active tick.
        if *sex == BiologicalSex::Male && male_cd.0 > 0 && clock.is_active(slot.0) {
            male_cd.0 = male_cd.0.saturating_sub(1);
        }

        if ai.state != AiState::Sleeping {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut closest: Option<(Entity, i32)> = None;
        for dy in -COSLEEP_RADIUS..=COSLEEP_RADIUS {
            for dx in -COSLEEP_RADIUS..=COSLEEP_RADIUS {
                if dx * dx + dy * dy > COSLEEP_RADIUS * COSLEEP_RADIUS {
                    continue;
                }
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if candidate == entity {
                        continue;
                    }
                    let Ok((other_ai, other_sex, other_fm)) = others.get(candidate) else {
                        continue;
                    };
                    if other_ai.state != AiState::Sleeping
                        || *other_sex == *sex
                        || other_fm.faction_id != member.faction_id
                    {
                        continue;
                    }
                    let d = dx * dx + dy * dy;
                    if closest.map(|(_, bd)| d < bd).unwrap_or(true) {
                        closest = Some((candidate, d));
                    }
                }
            }
        }

        if let Some((partner, _)) = closest {
            tracker.partner = Some(partner);
            tracker.ticks_co_slept = tracker.ticks_co_slept.saturating_add(1);
        }
    }
}

/// Detects `Sleeping → !Sleeping` transitions and runs the conception attempt
/// logic on females. Mirrors the transition reset on males so their tracker
/// state stays clean. The female side is the canonical "attempt" site; the
/// male's cooldown is consumed only there.
///
/// `ParamSet` is used so the female (mut Needs / CoSleepTracker) loop and the
/// male-side mutations don't claim &mut Needs concurrently.
pub fn wake_up_conception_system(
    mut commands: Commands,
    name_query: Query<&Name>,
    pregnancy_query: Query<(), With<Pregnancy>>,
    mut params: ParamSet<(
        Query<
            (
                Entity,
                &PersonAI,
                &BiologicalSex,
                &FactionMember,
                &Transform,
                &LodLevel,
                &mut Needs,
                &mut CoSleepTracker,
            ),
            With<Person>,
        >,
        Query<&mut Needs, With<Person>>,
        Query<&mut MaleConceptionCooldown, With<Person>>,
        Query<
            (
                Entity,
                Option<&Stats>,
                &MaleConceptionCooldown,
                &BiologicalSex,
                &FactionMember,
            ),
            With<Person>,
        >,
    )>,
) {
    struct Attempt {
        mother: Entity,
        male: Entity,
        faction_id: u32,
        mother_transform: Transform,
        father_stats: Option<Stats>,
        female_pregnant: bool,
        success: bool,
    }

    // ── Pass 0: snapshot read-only male data so the ParamSet can later move
    //            on to mutating Needs / MaleConceptionCooldown without
    //            aliasing this read-only borrow.
    struct MaleSnapshot {
        father_stats: Option<Stats>,
        cooldown: u32,
        sex: BiologicalSex,
        faction_id: u32,
    }
    let male_snapshot: ahash::AHashMap<Entity, MaleSnapshot> = {
        let q = params.p3();
        q.iter()
            .map(|(e, stats, cd, sex, fm)| {
                (
                    e,
                    MaleSnapshot {
                        father_stats: stats.copied(),
                        cooldown: cd.0,
                        sex: *sex,
                        faction_id: fm.faction_id,
                    },
                )
            })
            .collect()
    };

    let mut attempts: Vec<Attempt> = Vec::new();

    // ── Pass 1: iterate every Person, detect transitions, mutate tracker
    //            (and female Needs on attempts). Collect male-side work.
    {
        let mut q = params.p0();
        for (entity, ai, sex, member, transform, lod, mut needs, mut tracker) in q.iter_mut() {
            if *lod == LodLevel::Dormant {
                tracker.prev_state = ai.state;
                continue;
            }

            let was_sleeping = tracker.prev_state == AiState::Sleeping;
            let now_sleeping = ai.state == AiState::Sleeping;
            tracker.prev_state = ai.state;

            if !(was_sleeping && !now_sleeping) {
                continue;
            }

            let partner = tracker.partner.take();
            let ticks_co_slept = tracker.ticks_co_slept;
            tracker.ticks_co_slept = 0;

            if *sex != BiologicalSex::Female {
                continue;
            }
            if ticks_co_slept < MIN_COSLEEP_TICKS {
                continue;
            }
            let Some(male) = partner else { continue };

            // Validate male via the snapshot taken in Pass 0.
            let Some(snap) = male_snapshot.get(&male) else {
                continue;
            };
            if snap.sex == *sex || snap.faction_id != member.faction_id || snap.cooldown > 0 {
                continue;
            }

            let female_pregnant = pregnancy_query.get(entity).is_ok();
            let success = !female_pregnant && fastrand::u32(..2) == 0;

            // Reset female's reproduction need on every attempt (success or fail,
            // pregnant or not).
            needs.reproduction = 0.0;

            attempts.push(Attempt {
                mother: entity,
                male,
                faction_id: member.faction_id,
                mother_transform: *transform,
                father_stats: snap.father_stats,
                female_pregnant,
                success,
            });
        }
    }

    // ── Pass 2: reset the male's reproduction need.
    {
        let mut needs_q = params.p1();
        for a in &attempts {
            if let Ok(mut n) = needs_q.get_mut(a.male) {
                n.reproduction = 0.0;
            }
        }
    }

    // ── Pass 3: set the male's cooldown.
    {
        let mut cd_q = params.p2();
        for a in &attempts {
            if let Ok(mut cd) = cd_q.get_mut(a.male) {
                cd.0 = MALE_ATTEMPT_COOLDOWN_TICKS;
            }
        }
    }

    // ── Pass 4: insert pregnancies via Commands.
    for a in attempts {
        let mother_name = name_query
            .get(a.mother)
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|_| format!("{:?}", a.mother));
        let father_name = name_query
            .get(a.male)
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|_| format!("{:?}", a.male));

        if a.female_pregnant {
            info!(
                "Co-sleep attempt (already pregnant): {} + {} — needs reset only",
                mother_name, father_name
            );
            continue;
        }

        info!(
            "Conception attempt: {} + {} — success={}",
            mother_name, father_name, a.success
        );

        if a.success {
            let preg = Pregnancy::new(a.male, a.father_stats, a.faction_id);
            commands.entity(a.mother).insert(preg);
            let _ = a.mother_transform; // recorded for future use; transform read at birth
        }
    }
}

/// Ticks `Pregnancy` down each active frame and spawns the child when the
/// timer reaches 0. Runs in `SimulationSet::Economy`, replacing the old
/// `reproduction_system`.
pub fn pregnancy_system(
    mut commands: Commands,
    mut clock: ResMut<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    name_query: Query<&Name>,
    mut query: Query<(
        Entity,
        &mut Pregnancy,
        &Transform,
        &BucketSlot,
        &LodLevel,
        Option<&Stats>,
    )>,
) {
    let mut births: Vec<(Entity, Transform, u32, Stats)> = Vec::new();

    for (mother, mut preg, transform, slot, lod, mother_stats) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        preg.ticks_remaining = preg.ticks_remaining.saturating_sub(1);
        if preg.ticks_remaining > 0 {
            continue;
        }
        let child_stats = match (mother_stats, preg.father_stats.as_ref()) {
            (Some(m), Some(f)) => Stats::inherit(m, f),
            (Some(p), None) | (None, Some(p)) => Stats::inherit(p, p),
            (None, None) => Stats::roll_3d6(),
        };
        births.push((mother, *transform, preg.faction_id, child_stats));
    }

    for (mother, parent_transform, faction_id, child_stats) in births {
        commands.entity(mother).remove::<Pregnancy>();

        let new_slot = clock.population;
        clock.population += 1;
        clock.bucket_size = clock.population.min(10_000);

        let tx = (parent_transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (parent_transform.translation.y / TILE_SIZE).floor() as i32;
        let world_pos = tile_to_world(tx, ty);

        registry.add_member(faction_id);
        let sex = BiologicalSex::random();
        let child_name = child_name_for(faction_id, sex, &registry);
        let mother_name = name_query
            .get(mother)
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|_| format!("{:?}", mother));
        info!(
            "Birth: mother={} → child={} (faction {})",
            mother_name, child_name, faction_id
        );

        commands.spawn((
            (
                Person,
                Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
                Needs::new(0.0, 0.0, 0.0, 0.0, 0.0, 255.0),
                Mood::default(),
                Skills::default(),
                child_stats,
                PersonAI {
                    task_id: PersonAI::UNEMPLOYED,
                    state: AiState::Idle,
                    target_tile: (tx as i32, ty as i32),
                    dest_tile: (tx as i32, ty as i32),
                    last_plan_id: PersonAI::UNEMPLOYED,
                    current_z: chunk_map.surface_z_at(tx, ty) as i8,
                    target_z: chunk_map.surface_z_at(tx, ty) as i8,
                    ..PersonAI::default()
                },
                EconomicAgent::default(),
            ),
            (
                LodLevel::Full,
                BucketSlot(new_slot),
                MovementState::default(),
                sex,
                SkinTone::random(),
                HairColor::random(),
                Personality::random(),
                AgentGoal::default(),
                Profession::None,
                FactionMember {
                    faction_id,
                    ..Default::default()
                },
                Body::new_humanoid(),
                Equipment::default(),
            ),
            (
                TargetItem::default(),
                CombatTarget::default(),
                CombatCooldown::default(),
                AgentMemory::default(),
                RelationshipMemory::default(),
                KnownPlans::with_innate(&[
    0, 1, 2, 3, 5, 6, 7, 13, 14, 15, 16, 23, 25, 26, 27, 29, 30, 31, 32, 33, 34, 35, 36, 37,
    38, 39, 60, 61, 62, 63,
]),
                PlanHistory::default(),
                PlanScoringMethod::Weighted,
                Name::new(child_name),
                PathFollow::default(),
                Carrier::default(),
            ),
            (
                CoSleepTracker::default(),
                MaleConceptionCooldown::default(),
                crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Person),
            ),
        ));
    }
}

/// Build a name for a freshly-born child. Children of bonded factions take
/// "<First> of <LineageRoot>" so the dynasty is visible in the inspector;
/// SOLO births fall back to the generic name pool.
fn child_name_for(faction_id: u32, sex: BiologicalSex, registry: &FactionRegistry) -> String {
    let base = generate_person_name(sex);
    let root = registry
        .factions
        .get(&faction_id)
        .map(|f| f.lineage.root.as_str())
        .unwrap_or("");
    if root.is_empty() {
        base.to_string()
    } else {
        format!("{base} of {root}")
    }
}
