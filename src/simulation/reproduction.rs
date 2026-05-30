use super::carry::Carrier;
use super::combat::{Body, CombatCooldown, CombatTarget};
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::goals::{AgentGoal, Personality};
use super::htn::MethodHistory;
use super::items::{Equipment, TargetItem};
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::person::{
    generate_person_name, AiState, HairColor, Person, PersonAI, Profession, SkinTone,
};
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillPeaks, SkillUseTicks, Skills, SkillsLastSeen};
use super::stats::Stats;
use crate::economy::agent::EconomicAgent;
use crate::pathfinding::path_request::PathFollow;
use crate::ui::activity_log::{ActivityEntryKind, ActivityLogEvent};
use crate::world::chunk::Z_MIN;
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

    pub fn opposite(self) -> Self {
        match self {
            BiologicalSex::Male => BiologicalSex::Female,
            BiologicalSex::Female => BiologicalSex::Male,
        }
    }
}

/// Pregnancy lasts three seasons (108,000 ticks at TICKS_PER_DAY=7200,
/// DAYS_PER_SEASON=5 → 15 game days). Ticked every FixedUpdate regardless of
/// the mother's LOD or bucket slot so gestation matches calendar time.
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
    /// Snapshot of the father's `PersonKnowledge.aware | learned` at conception,
    /// so the child still inherits paternal awareness if the father dies or
    /// wanders out of range before birth.
    pub father_known: crate::simulation::knowledge_bits::KnowledgeBits,
    pub faction_id: u32,
}

impl Pregnancy {
    pub fn new(
        father: Entity,
        father_stats: Option<Stats>,
        father_known: crate::simulation::knowledge_bits::KnowledgeBits,
        faction_id: u32,
    ) -> Self {
        Self {
            ticks_remaining: PREGNANCY_TICKS,
            father: Some(father),
            father_stats,
            father_known,
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
    /// Pluralist Economy R3 follow-on: cumulative cosleep ticks
    /// across all sleep cycles with the current partner. Unlike
    /// `ticks_co_slept` (which resets per partner-switch via
    /// `wake_up_conception_system`), this counter accumulates as long
    /// as the partner is the same. Crossing
    /// `HOUSEHOLD_BOND_THRESHOLD` triggers
    /// `household_formation_system` to spawn a sub-faction.
    pub bond_strength: u16,
}

/// Marker that an agent has founded or joined a household sub-faction.
/// Prevents `household_formation_system` from re-spawning a sub-faction
/// for the same pair every tick. Cleared on death (via `Indexed`'s
/// despawn cascade) but otherwise persistent.
#[derive(Component, Clone, Copy, Debug)]
pub struct HouseholdMember {
    pub household_id: u32,
}

/// Pluralist Economy R3 follow-on: ticks of cumulative cosleep with
/// the same partner needed to trigger household formation. Anchored
/// at one game-week (`TICKS_PER_DAY * 7 = 25200`) — a long enough
/// signal that the bond is genuinely stable, not just a one-off.
pub const HOUSEHOLD_BOND_THRESHOLD: u16 = 25200;

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
            // Pluralist Economy R3 follow-on: bond_strength
            // accumulates across sleep cycles with the same
            // partner. Reset when the partner changes (different
            // entity).
            let same_partner = tracker.partner == Some(partner);
            tracker.partner = Some(partner);
            tracker.ticks_co_slept = tracker.ticks_co_slept.saturating_add(1);
            if same_partner {
                tracker.bond_strength = tracker.bond_strength.saturating_add(1);
            } else {
                tracker.bond_strength = 1;
            }
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
                Option<&crate::simulation::knowledge::PersonKnowledge>,
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
        father_known: crate::simulation::knowledge_bits::KnowledgeBits,
        female_pregnant: bool,
        success: bool,
    }

    // ── Pass 0: snapshot read-only male data so the ParamSet can later move
    //            on to mutating Needs / MaleConceptionCooldown without
    //            aliasing this read-only borrow.
    struct MaleSnapshot {
        father_stats: Option<Stats>,
        father_known: crate::simulation::knowledge_bits::KnowledgeBits,
        cooldown: u32,
        sex: BiologicalSex,
        faction_id: u32,
    }
    let male_snapshot: crate::collections::AHashMap<Entity, MaleSnapshot> = {
        let q = params.p3();
        q.iter()
            .map(|(e, stats, cd, sex, fm, knowledge)| {
                (
                    e,
                    MaleSnapshot {
                        father_stats: stats.copied(),
                        father_known: knowledge
                            .map(|k| k.awareness_snapshot())
                            .unwrap_or_default(),
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
                father_known: snap.father_known,
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
            let preg = Pregnancy::new(a.male, a.father_stats, a.father_known, a.faction_id);
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
    mut activity_log: EventWriter<ActivityLogEvent>,
    mut query: Query<(
        Entity,
        &mut Pregnancy,
        &Transform,
        &LodLevel,
        &PersonAI,
        Option<&Stats>,
        Option<&crate::simulation::knowledge::PersonKnowledge>,
        // Pluralist Economy R3 follow-on b: pass the mother's
        // HouseholdMember through to the birth-spawn site so the
        // newborn inherits.
        Option<&HouseholdMember>,
    )>,
) {
    let mut births: Vec<(
        Entity,
        Transform,
        LodLevel,
        i8,
        u32,
        Stats,
        crate::simulation::knowledge_bits::KnowledgeBits,
        Option<HouseholdMember>,
    )> = Vec::new();

    for (
        mother,
        mut preg,
        transform,
        lod,
        person_ai,
        mother_stats,
        mother_knowledge,
        mother_household,
    ) in query.iter_mut()
    {
        // Pregnancy must tick every FixedUpdate so gestation matches calendar
        // time: a Dormant mother far from the camera, or one whose BucketSlot
        // is outside the active window, must still birth on schedule.
        preg.ticks_remaining = preg.ticks_remaining.saturating_sub(1);
        if preg.ticks_remaining > 0 {
            continue;
        }
        let child_stats = match (mother_stats, preg.father_stats.as_ref()) {
            (Some(m), Some(f)) => Stats::inherit(m, f),
            (Some(p), None) | (None, Some(p)) => Stats::inherit(p, p),
            (None, None) => Stats::roll_3d6(),
        };
        let mother_known = mother_knowledge
            .map(|k| k.awareness_snapshot())
            .unwrap_or_default();
        let inherited_aware = mother_known.union(&preg.father_known);
        births.push((
            mother,
            *transform,
            *lod,
            person_ai.current_z,
            preg.faction_id,
            child_stats,
            inherited_aware,
            mother_household.copied(),
        ));
    }

    for (
        mother,
        parent_transform,
        mother_lod,
        mother_z,
        faction_id,
        child_stats,
        inherited_aware,
        mother_household,
    ) in births
    {
        commands.entity(mother).remove::<Pregnancy>();

        let new_slot = clock.population;
        clock.population += 1;
        clock.bucket_size = clock.population.min(10_000);

        let tx = (parent_transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (parent_transform.translation.y / TILE_SIZE).floor() as i32;
        let world_pos = tile_to_world(tx, ty);
        // `surface_z_at` returns `Z_MIN - 1` for unloaded chunks; Dormant
        // mothers may now birth off-screen, so fall back to the mother's
        // current Z when the chunk hasn't been streamed in.
        let surface_z = chunk_map.surface_z_at(tx, ty);
        let child_z = if surface_z >= Z_MIN {
            surface_z as i8
        } else {
            mother_z
        };

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

        let child_entity = commands
            .spawn((
                (
                    Person,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    Needs::new(0.0, 0.0, 0.0, 0.0, 0.0, 255.0),
                    Mood::default(),
                    Skills::default(),
                    SkillPeaks::default(),
                    SkillUseTicks::default(),
                    SkillsLastSeen::default(),
                    child_stats,
                    PersonAI {
                        state: AiState::Idle,
                        target_tile: (tx as i32, ty as i32),
                        dest_tile: (tx as i32, ty as i32),
                        current_z: child_z,
                        target_z: child_z,
                        ..PersonAI::default()
                    },
                    EconomicAgent::default(),
                ),
                (
                    mother_lod,
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
                    MethodHistory::default(),
                    crate::simulation::memory::CurrentVision::default(),
                    crate::simulation::memory::AgentVisionCache::default(),
                    Name::new(child_name.clone()),
                    PathFollow::default(),
                    Carrier::default(),
                ),
                (
                    CoSleepTracker::default(),
                    MaleConceptionCooldown::default(),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Person),
                    crate::simulation::knowledge::PersonKnowledge {
                        aware: inherited_aware,
                        learned: crate::simulation::knowledge_bits::KnowledgeBits::EMPTY,
                        learned_at: crate::collections::AHashMap::default(),
                        study_progress: crate::collections::AHashMap::default(),
                        mastery: crate::collections::AHashMap::default(),
                        belief: crate::collections::AHashMap::default(),
                    },
                    crate::simulation::typed_task::ActionQueue::idle(),
                    crate::simulation::goal_scorers::AgentDecisionState::default(),
                    crate::simulation::goal_scorers::Disposition::default(),
                    crate::simulation::social_contact::SecondarySocial::inactive(),
                    crate::simulation::energy::Energy::default(),
                    crate::simulation::tools::ToolKit::default(),
                ),
            ))
            .id();

        // Pluralist Economy R3 follow-on b: inherit
        // `HouseholdMember` from the mother. The child of two
        // pair-bonded parents is born into the household, not just
        // the village. This is what lets households grow over
        // generations.
        if let Some(mother_hh) = mother_household {
            commands.entity(child_entity).insert(mother_hh);
        }

        activity_log.send(ActivityLogEvent {
            tick: clock.tick,
            actor: mother,
            faction_id,
            kind: ActivityEntryKind::ChildBorn {
                child: child_entity,
                child_name,
            },
        });
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

/// Pluralist Economy R3 follow-on: spawn a household sub-faction
/// when a stable cosleep bond crosses `HOUSEHOLD_BOND_THRESHOLD`.
///
/// Trigger: an agent's `CoSleepTracker.bond_strength` crosses the
/// threshold AND neither the agent nor their partner is already a
/// `HouseholdMember`. The agent who crosses first is the household
/// head; the partner becomes a household member alongside them.
///
/// Side-effects:
/// - Calls `FactionRegistry::spawn_household` (parent = the shared
///   village faction) — stamps capitalist policy on every catalog
///   resource for the household.
/// - Inserts `HouseholdMember { household_id }` on **both** parents
///   so the system doesn't re-trigger.
/// - Does **not** modify `FactionMember.faction_id` on either
///   parent. They remain village members; the household is a
///   container for private storage / treasury that R6's
///   household-poster path will consume.
///
/// Future extensions: when a child is born to a household pair,
/// inherit `HouseholdMember` from the mother. (Wired alongside
/// `pregnancy_system` when the household-poster path lands.)
pub fn household_formation_system(
    mut commands: Commands,
    catalog: Res<crate::economy::resource_catalog::ResourceCatalog>,
    mut registry: ResMut<crate::simulation::faction::FactionRegistry>,
    transforms: Query<&Transform>,
    members: Query<&FactionMember>,
    households: Query<&HouseholdMember>,
    candidates: Query<(Entity, &CoSleepTracker, &FactionMember), With<Person>>,
) {
    use crate::world::terrain::TILE_SIZE;
    // Dedupe pairs: when iterating candidates, (a, b) and (b, a)
    // represent the same household. Track `seen` so we only emit
    // one spawn intent per pair.
    let mut spawn_intents: Vec<(Entity, Entity, u32, (i32, i32))> = Vec::new();
    let mut seen: crate::collections::AHashSet<Entity> = crate::collections::AHashSet::default();
    for (entity, tracker, member) in candidates.iter() {
        if tracker.bond_strength < HOUSEHOLD_BOND_THRESHOLD {
            continue;
        }
        let Some(partner) = tracker.partner else {
            continue;
        };
        if member.faction_id == crate::simulation::faction::SOLO {
            continue;
        }
        // Both must already not be in a household.
        if households.get(entity).is_ok() || households.get(partner).is_ok() {
            continue;
        }
        // Skip if this pair was already queued this tick.
        if seen.contains(&entity) || seen.contains(&partner) {
            continue;
        }
        // Both must share a village. The partner must still exist.
        let Ok(partner_member) = members.get(partner) else {
            continue;
        };
        if partner_member.faction_id != member.faction_id {
            continue;
        }
        // Pick a home tile near the head's current position.
        let home = match transforms.get(entity) {
            Ok(t) => (
                (t.translation.x / TILE_SIZE).floor() as i32,
                (t.translation.y / TILE_SIZE).floor() as i32,
            ),
            Err(_) => continue,
        };
        seen.insert(entity);
        seen.insert(partner);
        spawn_intents.push((entity, partner, member.faction_id, home));
    }
    for (head, partner, village, home) in spawn_intents {
        let household_id = registry.spawn_household(village, home, head, &catalog);
        commands
            .entity(head)
            .insert(HouseholdMember { household_id });
        commands
            .entity(partner)
            .insert(HouseholdMember { household_id });
        info!(
            "Household {household_id} formed from village {village}: head={head:?}, partner={partner:?}",
        );
    }
}
