use super::goals::Personality;
use super::items::{spawn_or_merge_ground_item, GroundItem};
use super::jobs::{
    record_progress, record_progress_filtered, JobBoard, JobClaim, JobCompletedEvent, JobKind,
    RecipeId,
};
use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::{AiState, Person, PersonAI, Profession};
use super::plants::PlantKind;
use super::schedule::{BucketSlot, SimClock};
use super::social_contact::{is_social_contact, SecondarySocial};
use super::skills::{SkillKind, Skills};
use super::tasks::TaskKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::simulation::technology::{
    tech_def, ActivityKind, Era, TechId, ACTIVITY_COUNT, CROP_CULTIVATION, HUNTING_SPEAR,
    TECH_COUNT, TECH_TREE,
};
use crate::world::chunk::ChunkMap;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use ahash::AHashMap;
use bevy::prelude::*;

pub const SOLO: u32 = 0;
pub const BOND_THRESHOLD: u8 = 180;
const CAMP_KEEP: u32 = 0;
pub const SOCIAL_RADIUS: i32 = 3;

// ── Chief-assigned hunting ───────────────────────────────────────────────────

/// Floor proportion of adults assigned as `Profession::Hunter` whenever the
/// faction has unlocked `HUNTING_SPEAR`. Scaled up by martial culture and
/// local prey density (see `faction_hunter_assignment_system`).
pub const HUNTER_MIN_RATIO: f32 = 0.20;

/// Tiles around `home_tile` the chief considers when picking a target species.
pub const HUNT_SCAN_RADIUS: i32 = 40;

/// Maximum age of a `HuntOrder` in ticks before the chief abandons a stalled
/// muster and waiters fall through. `TICKS_PER_DAY / 4` ≈ 15 game-hours / 45 s
/// real-time at 20 Hz — enough for stragglers without holding through the next
/// chief decision cycle.
pub const HUNT_PARTY_TIMEOUT: u64 = (TICKS_PER_DAY / 4) as u64;

/// Cadence at which `chief_hunt_order_system` re-decides each faction's
/// hunting target. Anchored at one game-day per faction so hunting reads as a
/// daily expedition, not a per-second reflex. Factions stagger across the day
/// via `tick % TICKS_PER_DAY == faction_id_offset`.
pub const HUNT_DECISION_CADENCE: u64 = TICKS_PER_DAY as u64;

/// Cadence for the cheap mid-day invalidation sweep. Cleared orders re-decide
/// at the next `HUNT_DECISION_CADENCE` boundary; this only catches the case
/// where a party finished or the prey emptied between full decision cycles.
pub const HUNT_INVALIDATE_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Cadence at which `faction_hunter_assignment_system` reconciles profession
/// counts. ~Once per quarter game-day; re-rolling every tick churns plans.
pub const HUNTER_ASSIGNMENT_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Phase 4b asymmetric hysteresis: number of hunters above target the
/// system tolerates before demoting. Promotion stays eager (any
/// shortfall promotes immediately); demotion only fires when the
/// excess exceeds this buffer. Stops single-tick flapping when prey
/// density rounds the target up and down across cadence cycles.
pub const HUNTER_DEMOTE_BUFFER: usize = 1;

// ── Pluralist Economy R5: Bureaucrat constants ─────────────────────────

/// Floor proportion of adults the chief tries to maintain as
/// `Profession::Bureaucrat` whenever `state_funds_public_works` is true
/// AND the settlement treasury can fund the wage. Min 1 promoted as
/// long as there's an eligible None adult.
pub const BUREAUCRAT_MIN_RATIO: f32 = 0.05;

/// Per-day wage paid to each bureaucrat from their settlement's
/// treasury. Anchored at 1.0/day; tune later as economy balance lands.
pub const BUREAUCRAT_DAILY_WAGE: f32 = 1.0;

/// Salary tick cadence — every `TICKS_PER_DAY/24` ticks (~hourly).
/// Each tick pays `BUREAUCRAT_DAILY_WAGE / 24` per bureaucrat.
pub const BUREAUCRAT_SALARY_INTERVAL: u64 = (TICKS_PER_DAY / 24) as u64;

/// Number of consecutive game-days the settlement treasury must be
/// unable to fund full wages before bureaucrats are forcibly demoted.
pub const BUREAUCRAT_QUIT_DAYS: u32 = 3;

/// Cadence at which `chief_bureaucrat_appointment_system` reconciles
/// bureaucrat headcount. Mirrors `HUNTER_ASSIGNMENT_CADENCE`.
pub const BUREAUCRAT_ASSIGNMENT_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Phase 4b asymmetric hysteresis (matches `HUNTER_DEMOTE_BUFFER`):
/// tolerate one bureaucrat above target before demoting. Promotion
/// stays eager; demotion fires only on excess > buffer.
pub const BUREAUCRAT_DEMOTE_BUFFER: usize = 1;

// ── Phase 5a (wage-aware-labor-market-v2): Crafter constants ──────────

/// Floor proportion of adults the chief tries to maintain as
/// `Profession::Crafter` whenever the faction's `wage_signal` shows
/// active paid craft work. Capped at adults/3 so crafters don't
/// crowd out subsistence labor.
pub const CRAFTER_MIN_RATIO: f32 = 0.25;

/// Per-faction cap for crafters. Mirrors hunter `adults/2`; crafters
/// are slightly tighter (adults/3) because their work depends on
/// upstream material flow.
pub const CRAFTER_MAX_DIVISOR: usize = 3;

/// Cadence at which `chief_craft_assignment_system` reconciles crafter
/// headcount. Mirrors `HUNTER_ASSIGNMENT_CADENCE`.
pub const CRAFTER_ASSIGNMENT_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Cadence at which `chief_architect_appointment_system` reconciles the
/// per-settlement architect (sleepy-dove Phase 3). Mirrors the other
/// specialised-labour assignment systems.
pub const ARCHITECT_ASSIGNMENT_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Demotion hysteresis for architects: tolerate this many over target
/// before demoting (consistent with Hunter/Bureaucrat patterns).
pub const ARCHITECT_DEMOTE_BUFFER: usize = 1;

/// Phase 4b/5a hysteresis deadband: promote crafters only when the
/// faction's Craft `wage_signal` has accumulated past this floor
/// (~one day of sustained paid craft work). A single first-day payout
/// folds in at `α ≈ 0.129 × reward`, so a ~5-currency contract
/// produces `ema ≈ 0.65` after one day — below the promote floor.
/// Two days of similar contracts push the EMA above 1.0 and trigger
/// promotion. Demotion uses the lower ceiling — once the signal has
/// genuinely decayed below 0.3, target → 0. Between the thresholds,
/// target = current crafter count (no churn).
pub const CRAFTER_WAGE_PROMOTE_FLOOR: f32 = 1.0;
pub const CRAFTER_WAGE_DEMOTE_CEILING: f32 = 0.3;

/// Pure helper for `chief_craft_assignment_system`'s target headcount.
/// Returns the post-cap target given the faction's max Craft EMA, its
/// current crafter count, and its member count. Exposed as a free
/// function so the deadband logic stays unit-testable without a `World`.
pub fn crafter_target_with_hysteresis(
    craft_ema: f32,
    current_crafters: usize,
    member_count: u32,
) -> usize {
    let mut target = if craft_ema >= CRAFTER_WAGE_PROMOTE_FLOOR && member_count > 0 {
        (member_count as f32 * CRAFTER_MIN_RATIO).round().max(1.0) as usize
    } else if craft_ema <= CRAFTER_WAGE_DEMOTE_CEILING {
        0
    } else {
        // Deadband: hold steady.
        current_crafters
    };
    target = target.min((member_count as usize) / CRAFTER_MAX_DIVISOR);
    target
}

// ── Pluralist Economy R11: Tribute constants ──────────────────────────

/// Per-subordinate-per-day tribute amount transferred from the
/// subordinate's faction treasury to the overlord's faction treasury.
/// Anchored at 5.0/day; tune as economy balance lands. The transfer
/// is capped at the subordinate's available treasury — a destitute
/// vassal pays nothing rather than going into debt.
pub const TRIBUTE_PER_DAY: f32 = 5.0;

/// Cadence at which `tribute_payment_system` runs. Once per game-day,
/// staggered per faction to spread workload.
pub const TRIBUTE_CADENCE: u64 = TICKS_PER_DAY as u64;

// ── Pluralist Economy R6 follow-on: household-poster constants ────────

/// Per-household-per-day craft-contract reward when a household
/// commissions a Tools job from its treasury. Anchored at 5.0 — same
/// scale as `TRIBUTE_PER_DAY`. Tune as economy balance lands.
pub const HOUSEHOLD_CONTRACT_REWARD: f32 = 5.0;

/// Minimum household treasury required for the periodic posting
/// system to commission a contract. Households with less than this
/// stay quiet rather than posting micro-contracts.
pub const HOUSEHOLD_MIN_TREASURY_FOR_POSTING: f32 = 10.0;

/// Initial treasury seeded into a household when it's spawned at game-start
/// under the `EconomyPreset::Market` path (`spawn_population` in
/// `person.rs`). Set above `HOUSEHOLD_MIN_TREASURY_FOR_POSTING` so the
/// household can post its first paid contract on the next
/// `HOUSEHOLD_POSTING_CADENCE` cycle without having to first earn money —
/// otherwise capitalist factions sit dormant for hours of game time.
/// Cosleep-bond households (`household_formation_system`) keep starting at
/// 0 — they accumulate via market earnings instead.
pub const HOUSEHOLD_SEED_TREASURY: f32 = 15.0;

/// Cadence at which `household_contract_posting_system` runs. Once
/// per game-day; each qualifying household posts one contract per
/// firing.
pub const HOUSEHOLD_POSTING_CADENCE: u64 = TICKS_PER_DAY as u64;

/// A chief-issued hunting directive. Lives on `FactionData::hunt_order` and is
/// either a concrete `Hunt` (with mustering bookkeeping) or a fallback
/// `Scout` order to find new game.
#[derive(Clone, Debug)]
pub enum HuntOrder {
    Hunt {
        species: super::corpse::CorpseSpecies,
        area_tile: (i32, i32),
        target_party_size: u8,
        mustered: Vec<Entity>,
        deployed_tick: Option<u64>,
        posted_tick: u64,
    },
    Scout {
        posted_tick: u64,
    },
}

impl HuntOrder {
    pub fn posted_tick(&self) -> u64 {
        match self {
            HuntOrder::Hunt { posted_tick, .. } => *posted_tick,
            HuntOrder::Scout { posted_tick } => *posted_tick,
        }
    }
}

/// Chief-driven hunter assignment. Runs in Economy after
/// `compute_faction_storage_system` once per `HUNTER_ASSIGNMENT_CADENCE`. For
/// every faction:
///
/// - Compute a target headcount from `HUNTER_MIN_RATIO * adults`, scaled up
///   by `culture.martial` and local prey density. `HUNTING_SPEAR` is a hard
///   tech gate; without it, target = 0.
/// - Under target → promote the highest-Combat-skill `Profession::None` adult.
///   Skips Farmers (don't poach an established role).
/// - Over target → demote the lowest-Combat-skill `Hunter`, tear down their
///   in-flight chain, release any storage reservation, drop any carried corpse.
///
/// Density is read off `FactionData::nearby_prey_count`, which `chief_hunt_order_system`
/// refreshes alongside its decision cycle. We don't re-scan the spatial index
/// here — assignment runs more often than the chief and the density signal
/// only needs to be roughly current.
pub fn faction_hunter_assignment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    reservations: Res<StorageReservations>,
    ownership: Res<crate::simulation::capital::WorkshopOwnership>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plots: Query<&crate::simulation::land::Plot>,
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut Profession,
        &FactionMember,
        &Skills,
        &EconomicAgent,
        &crate::simulation::carry::Carrier,
        &Transform,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        Option<&mut PersonAI>,
        Option<&mut crate::simulation::typed_task::ActionQueue>,
        Option<&crate::simulation::knowledge::PersonKnowledge>,
    )>,
) {
    if clock.tick % HUNTER_ASSIGNMENT_CADENCE != 0 {
        return;
    }

    // Snapshot per-faction target headcounts so we don't borrow registry
    // across the mutable query iteration.
    struct FactionTarget {
        adult_count: u32,
        hunter_target: usize,
    }
    let mut targets: AHashMap<u32, FactionTarget> = AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        let has_tech = faction.techs.has(HUNTING_SPEAR);
        let adults = faction.member_count;
        let nearby = faction.nearby_prey_count as f32;
        let martial_scale = 0.5 + (faction.culture.martial as f32 / 255.0);
        let density_scale = if adults > 0 {
            (nearby / adults as f32).clamp(1.0, 2.0)
        } else {
            1.0
        };
        // Phase 4b survival override: starving factions zero hunter
        // target so labor surrenders to the Farmer ramp. Don't gate on
        // `nearby_prey_count > 0` here — even with prey available, if
        // food per head is critical we want everyone farming first.
        let per_head = if adults > 0 {
            faction.storage.food_total() / adults as f32
        } else {
            f32::INFINITY
        };
        let survival = per_head < FARMER_SURVIVAL_FLOOR;
        let mut target = if has_tech && adults > 0 && !survival {
            let floor = (adults as f32 * HUNTER_MIN_RATIO).round().max(1.0);
            (floor * martial_scale * density_scale).round() as usize
        } else {
            0
        };
        // Don't let hunters consume more than half the workforce.
        target = target.min((adults as usize) / 2);
        targets.insert(
            fid,
            FactionTarget {
                adult_count: adults,
                hunter_target: target,
            },
        );
    }

    // Per-faction snapshot of (entity, combat_skill) for current hunters and
    // None candidates. None candidates are pre-filtered to those who have
    // personally Learned HUNTING_SPEAR — the chief can post hunter slots
    // (faction-aware) but only members who actually know the technique are
    // promotable. Existing hunters are left alone; demotion will catch any
    // who lost the tech via LRU eviction.
    // Phase 4b: rank candidates by `(EV, skill)` — EV via
    // `profession_choice::expected_wage(faction, Hunter, skills, capital)`
    // dominates when the faction's `wage_signal` has accumulated samples
    // for Hunter's job kinds; falls back to raw Combat skill when wages
    // are zero. `capital_factor` averages tool / workshop / land affinity
    // so a weapon-bearing agent ranks ahead of an unarmed equal-skill
    // peer when wages are non-zero.
    let mut by_faction_hunters: AHashMap<u32, Vec<(Entity, f32, u32)>> = AHashMap::default();
    let mut by_faction_none: AHashMap<u32, Vec<(Entity, f32, u32)>> = AHashMap::default();
    for (entity, prof, member, skills, agent, carrier, xf, household_opt, _, _, knowledge_opt) in
        query.iter()
    {
        if member.faction_id == SOLO {
            continue;
        }
        let combat = skills.0[SkillKind::Combat as usize];
        let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
        let cap = crate::simulation::capital::capital_factor(
            agent,
            carrier,
            tile,
            member.faction_id,
            household_opt,
            Profession::Hunter,
            &ownership,
            &plots,
            &plot_index,
        );
        let ev = registry
            .factions
            .get(&member.faction_id)
            .map(|f| {
                crate::simulation::profession_choice::expected_wage(
                    f,
                    Profession::Hunter,
                    skills,
                    cap,
                )
            })
            .unwrap_or(0.0);
        match *prof {
            Profession::Hunter => by_faction_hunters
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, combat)),
            Profession::None => {
                let knows_hunting = knowledge_opt
                    .map(|k| k.has_learned(HUNTING_SPEAR))
                    .unwrap_or(false);
                if knows_hunting {
                    by_faction_none
                        .entry(member.faction_id)
                        .or_default()
                        .push((entity, ev, combat));
                }
            }
            _ => {}
        }
    }

    let mut promote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut demote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for (&fid, target) in &targets {
        let mut hunters = by_faction_hunters.remove(&fid).unwrap_or_default();
        let mut none = by_faction_none.remove(&fid).unwrap_or_default();
        let want = target.hunter_target;
        let _ = target.adult_count; // populated for inspector/logging
        if hunters.len() < want {
            // Highest (EV, skill) first.
            none.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.2.cmp(&a.2))
            });
            let need = want - hunters.len();
            for (e, _, _) in none.into_iter().take(need) {
                promote.insert(e);
            }
        } else if hunters.len() > want.saturating_add(HUNTER_DEMOTE_BUFFER) || want == 0 {
            // Asymmetric hysteresis: tolerate `HUNTER_DEMOTE_BUFFER`
            // hunters above target. The `want == 0` arm forces full
            // demotion when the survival floor or HUNTING_SPEAR gate
            // collapses the target — the buffer applies only to
            // non-zero target oscillation.
            // Lowest (EV, skill) first.
            hunters.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.2.cmp(&b.2))
            });
            let extra = hunters.len() - want;
            for (e, _, _) in hunters.into_iter().take(extra) {
                demote.insert(e);
            }
        }
    }

    if promote.is_empty() && demote.is_empty() {
        return;
    }

    for (
        entity,
        mut prof,
        _member,
        _skills,
        _agent,
        _carrier,
        _xf,
        _household,
        ai_opt,
        aq_opt,
        _knowledge,
    ) in query.iter_mut()
    {
        if promote.contains(&entity) {
            *prof = Profession::Hunter;
        } else if demote.contains(&entity) {
            *prof = Profession::None;
            crate::simulation::profession_choice::demote_profession_state(
                entity,
                ai_opt.map(|x| x.into_inner()),
                aq_opt.map(|x| x.into_inner()),
                &reservations,
                &mut commands,
            );
        }
    }
}

/// Pluralist Economy R5: chief-driven bureaucrat assignment. Runs in
/// Economy on `BUREAUCRAT_ASSIGNMENT_CADENCE`. Mirrors the hunter
/// pattern (promote highest-skill None, demote lowest-skill on
/// over-target / treasury bust).
///
/// Gating:
/// - Faction must have `state_funds_public_works = true`.
/// - Treasury empty-streak below the quit threshold (otherwise forces
///   full demotion regardless of target).
///
/// Target headcount: `max(1, adults * BUREAUCRAT_MIN_RATIO)` rounded.
/// Skill: ranked by Social skill (bureaucracy is paperwork). Existing
/// Hunters / Farmers are skipped — bureaucracy doesn't poach
/// established roles.
pub fn chief_bureaucrat_appointment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    reservations: Res<StorageReservations>,
    ownership: Res<crate::simulation::capital::WorkshopOwnership>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plots: Query<&crate::simulation::land::Plot>,
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut Profession,
        &FactionMember,
        &Skills,
        &EconomicAgent,
        &crate::simulation::carry::Carrier,
        &Transform,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        Option<&mut PersonAI>,
        Option<&mut crate::simulation::typed_task::ActionQueue>,
    )>,
) {
    if clock.tick % BUREAUCRAT_ASSIGNMENT_CADENCE != 0 {
        return;
    }

    // Per-faction target headcount, snapshotted to avoid borrowing the
    // registry during the mutable query iteration. Treasury bust forces
    // target=0 so all bureaucrats demote.
    let quit_threshold = BUREAUCRAT_QUIT_DAYS.saturating_mul(TICKS_PER_DAY);
    let mut targets: AHashMap<u32, usize> = AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO || !faction.state_funds_public_works {
            continue;
        }
        // Phase 4b survival override: starving factions zero
        // bureaucrat target. Administration is the first thing to
        // surrender when food runs out.
        let per_head = if faction.member_count > 0 {
            faction.storage.food_total() / faction.member_count as f32
        } else {
            f32::INFINITY
        };
        let survival = per_head < FARMER_SURVIVAL_FLOOR;
        let mut target = if faction.member_count > 0 && !survival {
            (faction.member_count as f32 * BUREAUCRAT_MIN_RATIO)
                .round()
                .max(1.0) as usize
        } else {
            0
        };
        if faction.bureaucrat_treasury_empty_streak >= quit_threshold {
            target = 0;
        }
        target = target.min((faction.member_count as usize) / 2);
        targets.insert(fid, target);
    }

    if targets.is_empty() {
        return;
    }

    // Phase 4b: EV-aware ranking. See hunter equivalent above. Capital
    // factor here is dominated by Market workshop ownership (Bureaucrat
    // affine) within `WORKSHOP_AFFINITY_RADIUS` of the agent's tile.
    let mut by_faction_bureaucrats: AHashMap<u32, Vec<(Entity, f32, u32)>> = AHashMap::default();
    let mut by_faction_none: AHashMap<u32, Vec<(Entity, f32, u32)>> = AHashMap::default();
    for (entity, prof, member, skills, agent, carrier, xf, household_opt, _, _) in query.iter() {
        if member.faction_id == SOLO || !targets.contains_key(&member.faction_id) {
            continue;
        }
        let social = skills.0[SkillKind::Social as usize];
        let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
        let cap = crate::simulation::capital::capital_factor(
            agent,
            carrier,
            tile,
            member.faction_id,
            household_opt,
            Profession::Bureaucrat,
            &ownership,
            &plots,
            &plot_index,
        );
        let ev = registry
            .factions
            .get(&member.faction_id)
            .map(|f| {
                crate::simulation::profession_choice::expected_wage(
                    f,
                    Profession::Bureaucrat,
                    skills,
                    cap,
                )
            })
            .unwrap_or(0.0);
        match *prof {
            Profession::Bureaucrat => by_faction_bureaucrats
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, social)),
            Profession::None => by_faction_none
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, social)),
            _ => {}
        }
    }

    let mut promote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut demote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for (&fid, &want) in &targets {
        let mut bureaucrats = by_faction_bureaucrats.remove(&fid).unwrap_or_default();
        let mut none = by_faction_none.remove(&fid).unwrap_or_default();
        if bureaucrats.len() < want {
            none.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.2.cmp(&a.2))
            });
            for (e, _, _) in none.into_iter().take(want - bureaucrats.len()) {
                promote.insert(e);
            }
        } else if bureaucrats.len() > want.saturating_add(BUREAUCRAT_DEMOTE_BUFFER) || want == 0 {
            // Asymmetric hysteresis: tolerate `BUREAUCRAT_DEMOTE_BUFFER`
            // bureaucrats above target. `want == 0` (treasury-quit or
            // survival override) forces full demotion bypassing the
            // buffer so destitute factions actually shed administration.
            bureaucrats.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.2.cmp(&b.2))
            });
            let extra = bureaucrats.len() - want;
            for (e, _, _) in bureaucrats.into_iter().take(extra) {
                demote.insert(e);
            }
        }
    }

    if promote.is_empty() && demote.is_empty() {
        return;
    }

    for (entity, mut prof, _member, _skills, _agent, _carrier, _xf, _household, ai_opt, aq_opt) in
        query.iter_mut()
    {
        if promote.contains(&entity) {
            *prof = Profession::Bureaucrat;
        } else if demote.contains(&entity) {
            *prof = Profession::None;
            crate::simulation::profession_choice::demote_profession_state(
                entity,
                ai_opt.map(|x| x.into_inner()),
                aq_opt.map(|x| x.into_inner()),
                &reservations,
                &mut commands,
            );
        }
    }
}

/// sleepy-dove Phase 3: chief-driven, per-settlement architect
/// appointment. An architect is a settlement-scoped construction
/// authority — they author build/haul postings whose blueprint the
/// chief couldn't gate because the chief hasn't personally **Learned**
/// the construction tech.
///
/// Per-settlement target:
/// - **0** unless the chief is *Aware* of at least one construction tech
///   they haven't personally **Learned** (the chief can't cover
///   everything). Paleolithic bands — whose construction is all no-tech
///   (Crude beds / Open hearths / Wood doors) — naturally get 0.
/// - **1** otherwise. (Per user direction: one architect per settlement,
///   not population-scaled.)
///
/// Candidate filter: residents of that settlement, `Profession::None`,
/// who have personally **Learned** at least one construction tech the
/// chief lacks. Ranked by coverage gain → Building → Social → entity id.
/// Demotion uses `ARCHITECT_DEMOTE_BUFFER` hysteresis and the shared
/// `demote_profession_state` helper; in-flight blueprints keep their
/// snapshotted `posted_by` + `design_techs`, so demotion is a no-op for
/// work already authored.
///
/// Scheduled after `chief_bureaucrat_appointment_system`, before
/// `chief_craft_assignment_system`, so it gets first pick of
/// Building-skilled `None` candidates without starving the wage-economy
/// roles. Real contention is rare (one per settlement, tech-gated).
pub fn chief_architect_appointment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    reservations: Res<StorageReservations>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    settlement_q: Query<&crate::simulation::settlement::Settlement>,
    chief_knowledge_q: Query<&crate::simulation::knowledge::PersonKnowledge, With<FactionChief>>,
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &mut Profession,
            &FactionMember,
            &Skills,
            &Transform,
            &crate::simulation::knowledge::PersonKnowledge,
            Option<&mut PersonAI>,
            Option<&mut crate::simulation::typed_task::ActionQueue>,
        ),
        Without<FactionChief>,
    >,
) {
    if clock.tick % ARCHITECT_ASSIGNMENT_CADENCE != 0 {
        return;
    }

    let constr_techs = crate::simulation::construction::construction_relevant_techs();

    // Per-faction: the construction techs the chief is Aware of but has
    // NOT personally Learned. Non-empty → settlements want an architect.
    // Also stash the chief's Learned set for the coverage-gain rank.
    let mut chief_gap: AHashMap<u32, Vec<crate::simulation::technology::TechId>> =
        AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO || faction.member_count == 0 {
            continue;
        }
        // Camps / no-posting archetypes don't run chief construction.
        if faction.caps.posting.is_disabled() {
            continue;
        }
        if matches!(faction.camp_state, CampState::Packed { .. }) {
            continue;
        }
        let Some(chief) = faction.chief_entity else {
            continue;
        };
        let Ok(ck) = chief_knowledge_q.get(chief) else {
            continue;
        };
        let gap: Vec<_> = constr_techs
            .iter()
            .copied()
            .filter(|&t| ck.is_aware(t) && !ck.has_learned(t))
            .collect();
        if !gap.is_empty() {
            chief_gap.insert(fid, gap);
        }
    }

    if chief_gap.is_empty() {
        // Still need to demote any orphaned architects (faction lost the
        // gap, or chief learned everything). Fall through with empty gap.
    }

    // Bucket current architects + None candidates by (faction, resident
    // settlement). Coverage gain = how many of the chief's missing
    // construction techs this member has personally Learned.
    type Bucket = AHashMap<
        (u32, crate::simulation::settlement::SettlementId),
        Vec<(
            Entity,
            u32, /*coverage*/
            u32, /*building*/
            u32, /*social*/
        )>,
    >;
    let mut architects: Bucket = AHashMap::default();
    let mut nones: Bucket = AHashMap::default();

    for (entity, prof, member, skills, xf, knowledge, _, _) in query.iter() {
        let fid = member.faction_id;
        if fid == SOLO {
            continue;
        }
        let Some(gap) = chief_gap.get(&fid) else {
            // No architect demand for this faction. Still bucket existing
            // architects so they get demoted.
            if *prof == Profession::Architect {
                if let Some(sid) = settlement_map.first_for_faction(fid) {
                    architects
                        .entry((fid, sid))
                        .or_default()
                        .push((entity, 0, 0, 0));
                }
            }
            continue;
        };
        let coverage = gap.iter().filter(|&&t| knowledge.has_learned(t)).count() as u32;
        let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
        // Resident settlement = nearest same-faction settlement.
        let mut best: Option<(crate::simulation::settlement::SettlementId, i32)> = None;
        for &sid in settlement_map.for_faction(fid) {
            let Some(&se) = settlement_map.by_id.get(&sid) else {
                continue;
            };
            let Ok(s) = settlement_q.get(se) else {
                continue;
            };
            let d = (s.market_tile.0 - tile.0)
                .abs()
                .max((s.market_tile.1 - tile.1).abs());
            if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((sid, d));
            }
        }
        let Some((sid, _)) = best else {
            continue;
        };
        let building = skills.0[SkillKind::Building as usize];
        let social = skills.0[SkillKind::Social as usize];
        match *prof {
            Profession::Architect => architects
                .entry((fid, sid))
                .or_default()
                .push((entity, coverage, building, social)),
            Profession::None if coverage > 0 => nones
                .entry((fid, sid))
                .or_default()
                .push((entity, coverage, building, social)),
            _ => {}
        }
    }

    let mut promote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut demote: ahash::AHashSet<Entity> = ahash::AHashSet::default();

    // Every settlement that has demand (gap non-empty) targets exactly 1.
    let mut settlement_keys: ahash::AHashSet<(u32, crate::simulation::settlement::SettlementId)> =
        ahash::AHashSet::default();
    for k in architects.keys() {
        settlement_keys.insert(*k);
    }
    for k in nones.keys() {
        settlement_keys.insert(*k);
    }

    for key in settlement_keys {
        let (fid, _sid) = key;
        let want: usize = if chief_gap.contains_key(&fid) { 1 } else { 0 };
        let mut cur = architects.remove(&key).unwrap_or_default();
        let mut cands = nones.remove(&key).unwrap_or_default();

        // Demote architects who no longer cover any chief gap (lost the
        // tech via LRU eviction, or faction lost demand entirely).
        cur.retain(|&(e, cov, _, _)| {
            if want == 0 || cov == 0 {
                demote.insert(e);
                false
            } else {
                true
            }
        });

        if cur.len() < want {
            cands.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then(b.2.cmp(&a.2))
                    .then(b.3.cmp(&a.3))
                    .then(a.0.cmp(&b.0))
            });
            for (e, _, _, _) in cands.into_iter().take(want - cur.len()) {
                promote.insert(e);
            }
        } else if cur.len() > want + ARCHITECT_DEMOTE_BUFFER {
            cur.sort_by(|a, b| {
                a.1.cmp(&b.1)
                    .then(a.2.cmp(&b.2))
                    .then(a.3.cmp(&b.3))
                    .then(a.0.cmp(&b.0))
            });
            let extra = cur.len() - want;
            for (e, _, _, _) in cur.into_iter().take(extra) {
                demote.insert(e);
            }
        }
    }

    if promote.is_empty() && demote.is_empty() {
        return;
    }

    for (entity, mut prof, _member, _skills, _xf, _knowledge, ai_opt, aq_opt) in query.iter_mut() {
        if promote.contains(&entity) {
            *prof = Profession::Architect;
        } else if demote.contains(&entity) {
            *prof = Profession::None;
            crate::simulation::profession_choice::demote_profession_state(
                entity,
                ai_opt.map(|x| x.into_inner()),
                aq_opt.map(|x| x.into_inner()),
                &reservations,
                &mut commands,
            );
        }
    }
}

/// Phase 5a (wage-aware-labor-market-v2): chief-driven crafter
/// assignment. Mirrors `chief_bureaucrat_appointment_system` and
/// `faction_hunter_assignment_system`.
///
/// Trigger: faction has a positive `wage_signal[(Craft, _)].ema_per_day`
/// — there's actually paid craft work flowing. When the signal
/// collapses (no Craft postings for ~5 days), target → 0 and
/// existing crafters demote.
///
/// Target headcount: `max(1, member_count * CRAFTER_MIN_RATIO)`,
/// capped at `member_count / CRAFTER_MAX_DIVISOR`. Skips Farmers /
/// Hunters / Bureaucrats / Traders — crafter assignment doesn't
/// poach established roles.
pub fn chief_craft_assignment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    reservations: Res<StorageReservations>,
    ownership: Res<crate::simulation::capital::WorkshopOwnership>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plots: Query<&crate::simulation::land::Plot>,
    mentors_q: Query<&crate::simulation::apprenticeship::MentorOf>,
    mut commands: Commands,
    mut activity: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut query: Query<(
        Entity,
        &mut Profession,
        &FactionMember,
        &Skills,
        &EconomicAgent,
        &crate::simulation::carry::Carrier,
        &Transform,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        Option<&mut PersonAI>,
        Option<&mut crate::simulation::typed_task::ActionQueue>,
    )>,
) {
    if clock.tick % CRAFTER_ASSIGNMENT_CADENCE != 0 {
        return;
    }

    // Pre-pass: count current crafters per faction so the deadband
    // can hold steady when ema sits between the promote / demote
    // thresholds. Apprentices count toward the headcount target — they
    // are committed Crafters-in-training. Pre-passing also lets us
    // short-circuit the candidate walk for factions that have neither
    // crafters nor a live signal.
    // Also collect available mentors (Crafter, Skills[Crafting] >=
    // MASTER_THRESHOLD, not already mentoring) per faction — Phase 5b
    // routes sub-`APPRENTICE_THRESHOLD` promotions through apprenticeship.
    let mut current_crafters: AHashMap<u32, usize> = AHashMap::default();
    let mut available_mentors: AHashMap<u32, Vec<Entity>> = AHashMap::default();
    for (entity, prof, member, skills, _, _, _, _, _, _) in query.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        match *prof {
            Profession::Crafter => {
                *current_crafters.entry(member.faction_id).or_insert(0) += 1;
                let crafting = skills.0[SkillKind::Crafting as usize];
                if crafting >= crate::simulation::apprenticeship::MASTER_THRESHOLD
                    && mentors_q.get(entity).is_err()
                {
                    available_mentors
                        .entry(member.faction_id)
                        .or_default()
                        .push(entity);
                }
            }
            Profession::Apprentice => {
                *current_crafters.entry(member.faction_id).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    // Per-faction target headcounts. Skipped factions: SOLO. Crafter
    // assignment honors a hysteresis deadband on the Craft wage signal:
    //   ema >  CRAFTER_WAGE_PROMOTE_FLOOR  → target = ratio-driven
    //   ema <  CRAFTER_WAGE_DEMOTE_CEILING → target = 0
    //   otherwise (deadband)               → target = current count
    // The deadband prevents flapping when a single one-shot Craft
    // payout's EMA fold ripples around zero between paid contracts.
    let mut targets: AHashMap<u32, usize> = AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        // Phase 4b survival override: starving factions zero crafter
        // target. Specialized labor surrenders before bureaucracy when
        // food is critical; crafters are slightly more affordable
        // than hunters, but both yield to a Farmer ramp.
        let per_head = if faction.member_count > 0 {
            faction.storage.food_total() / faction.member_count as f32
        } else {
            f32::INFINITY
        };
        if per_head < FARMER_SURVIVAL_FLOOR {
            targets.insert(fid, 0);
            continue;
        }
        // Highest active Craft EMA across all resource variants.
        let craft_ema = faction
            .wage_signal
            .iter()
            .filter_map(|((kind, _), ema)| {
                (*kind == crate::simulation::jobs::JobKind::Craft).then_some(ema.ema_per_day)
            })
            .fold(0.0_f32, f32::max);
        let current = current_crafters.get(&fid).copied().unwrap_or(0);
        let target = crafter_target_with_hysteresis(craft_ema, current, faction.member_count);
        targets.insert(fid, target);
    }

    if targets.is_empty() {
        return;
    }

    // EV-aware ranking. Capital factor here is dominated by
    // Workbench / Loom ownership within `WORKSHOP_AFFINITY_RADIUS`
    // and any `tools` resource in inventory or hands.
    let mut by_faction_crafters: AHashMap<u32, Vec<(Entity, f32, u32)>> = AHashMap::default();
    let mut by_faction_none: AHashMap<u32, Vec<(Entity, f32, u32)>> = AHashMap::default();
    for (entity, prof, member, skills, agent, carrier, xf, household_opt, _, _) in query.iter() {
        if member.faction_id == SOLO || !targets.contains_key(&member.faction_id) {
            continue;
        }
        let crafting = skills.0[SkillKind::Crafting as usize];
        let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
        let cap = crate::simulation::capital::capital_factor(
            agent,
            carrier,
            tile,
            member.faction_id,
            household_opt,
            Profession::Crafter,
            &ownership,
            &plots,
            &plot_index,
        );
        let ev = registry
            .factions
            .get(&member.faction_id)
            .map(|f| {
                crate::simulation::profession_choice::expected_wage(
                    f,
                    Profession::Crafter,
                    skills,
                    cap,
                )
            })
            .unwrap_or(0.0);
        match *prof {
            Profession::Crafter => by_faction_crafters
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, crafting)),
            Profession::None => by_faction_none
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, crafting)),
            _ => {}
        }
    }

    let mut promote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut demote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for (&fid, &want) in &targets {
        let mut crafters = by_faction_crafters.remove(&fid).unwrap_or_default();
        let mut none = by_faction_none.remove(&fid).unwrap_or_default();
        if crafters.len() < want {
            none.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.2.cmp(&a.2))
            });
            for (e, _, _) in none.into_iter().take(want - crafters.len()) {
                promote.insert(e);
            }
        } else if crafters.len() > want {
            crafters.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.2.cmp(&b.2))
            });
            let extra = crafters.len() - want;
            for (e, _, _) in crafters.into_iter().take(extra) {
                demote.insert(e);
            }
        }
    }

    if promote.is_empty() && demote.is_empty() {
        return;
    }

    for (entity, mut prof, member, skills, _agent, _carrier, _xf, _household, ai_opt, aq_opt) in
        query.iter_mut()
    {
        if promote.contains(&entity) {
            // Phase 5b: candidates below APPRENTICE_THRESHOLD route through
            // a master mentor when one is available; otherwise fall back to
            // direct Crafter promotion so a faction without any masters
            // can still bootstrap.
            let crafting = skills.0[SkillKind::Crafting as usize];
            if crafting < crate::simulation::apprenticeship::APPRENTICE_THRESHOLD {
                if let Some(pool) = available_mentors.get_mut(&member.faction_id) {
                    if let Some(mentor) = pool.pop() {
                        *prof = Profession::Apprentice;
                        commands
                            .entity(entity)
                            .insert(crate::simulation::apprenticeship::ApprenticeOf { mentor })
                            .insert(
                                crate::simulation::apprenticeship::ApprenticeProgress::default(),
                            );
                        commands.entity(mentor).insert(
                            crate::simulation::apprenticeship::MentorOf { apprentice: entity },
                        );
                        activity.send(crate::ui::activity_log::ActivityLogEvent {
                            tick: clock.tick,
                            actor: entity,
                            faction_id: member.faction_id,
                            kind:
                                crate::ui::activity_log::ActivityEntryKind::ApprenticeshipStarted {
                                    mentor,
                                },
                        });
                        continue;
                    }
                }
            }
            *prof = Profession::Crafter;
        } else if demote.contains(&entity) {
            *prof = Profession::None;
            crate::simulation::profession_choice::demote_profession_state(
                entity,
                ai_opt.map(|x| x.into_inner()),
                aq_opt.map(|x| x.into_inner()),
                &reservations,
                &mut commands,
            );
        }
    }
}

/// Pluralist Economy R5: per-bureaucrat salary tick. Every
/// `BUREAUCRAT_SALARY_INTERVAL` ticks, find each bureaucrat's
/// faction's first settlement, debit `BUREAUCRAT_DAILY_WAGE / 24`
/// from `Settlement.treasury`, and credit it to the bureaucrat's
/// `EconomicAgent.currency`.
///
/// Empty-treasury accounting: when a settlement can't fully pay all
/// its bureaucrats, the parent faction's
/// `bureaucrat_treasury_empty_streak` advances by
/// `BUREAUCRAT_SALARY_INTERVAL` ticks. A successful full pay resets
/// the streak. The appointment system reads this streak and demotes
/// once it crosses `BUREAUCRAT_QUIT_DAYS * TICKS_PER_DAY`.
pub fn bureaucrat_salary_tick_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    mut settlements: Query<&mut crate::simulation::settlement::Settlement>,
    mut bureaucrats: Query<(
        &Profession,
        &FactionMember,
        &mut crate::economy::agent::EconomicAgent,
    )>,
) {
    if clock.tick % BUREAUCRAT_SALARY_INTERVAL != 0 {
        return;
    }
    let wage_per_tick = BUREAUCRAT_DAILY_WAGE / 24.0;

    // Build per-faction bureaucrat counts so we can compute the total
    // wage demand per settlement before mutating treasuries. Two-pass
    // pattern to keep borrow-checker happy: pass 1 = read-only counts,
    // pass 2 = mutate treasuries + agent currencies based on each
    // faction's pay-rate (full or shorted).
    let mut bureaucrats_per_faction: AHashMap<u32, u32> = AHashMap::default();
    for (prof, member, _) in bureaucrats.iter() {
        if *prof != Profession::Bureaucrat || member.faction_id == SOLO {
            continue;
        }
        *bureaucrats_per_faction
            .entry(member.faction_id)
            .or_insert(0) += 1;
    }
    if bureaucrats_per_faction.is_empty() {
        // Still need to run the streak-update loop for factions that
        // *should* have bureaucrats but don't yet (treasury empty
        // before any promote). Iterate registry separately below.
    }

    // For each faction with `state_funds_public_works=true`, decide
    // whether the settlement treasury covers the full per-tick wage
    // for its bureaucrats. Output: (faction_id, pay_per_bureaucrat,
    // treasury_healthy). `treasury_healthy` is decoupled from
    // bureaucrat count — it tracks whether the settlement treasury
    // could fund the appointed-or-target headcount this tick.
    // Decoupling matters: after a demote-to-zero, count drops to 0
    // but the underlying treasury is still empty, so the streak must
    // keep advancing to block immediate re-promotion.
    let mut pay_decision: AHashMap<u32, (f32, bool)> = AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if !faction.state_funds_public_works || fid == SOLO {
            continue;
        }
        let count = bureaucrats_per_faction.get(&fid).copied().unwrap_or(0);

        // Find the faction's first settlement and read its treasury.
        let settlement_entity = settlement_map
            .by_faction
            .get(&fid)
            .and_then(|ids| ids.first().copied())
            .and_then(|sid| settlement_map.by_id.get(&sid).copied());
        let Some(settlement_entity) = settlement_entity else {
            // No settlement → can't fund. Treasury vacuously empty.
            pay_decision.insert(fid, (0.0, false));
            continue;
        };
        let treasury = settlements
            .get(settlement_entity)
            .map(|s| s.treasury)
            .unwrap_or(0.0);

        // Treasury health: enough to fund a hypothetical headcount of
        // at least 1 bureaucrat for one salary interval. Independent
        // of current `count`, so a zero-bureaucrat empty-treasury
        // faction keeps its streak advancing.
        let healthy = treasury >= wage_per_tick;

        let pay = if count == 0 {
            0.0
        } else if treasury >= wage_per_tick * count as f32 {
            wage_per_tick
        } else {
            // Partial pay: split whatever's there evenly.
            (treasury / count as f32).max(0.0)
        };
        pay_decision.insert(fid, (pay, healthy));
    }

    // Apply: debit settlements, credit bureaucrats, advance / reset
    // empty-streaks.
    for (&fid, &(pay, full)) in pay_decision.iter() {
        if pay > 0.0 {
            // Debit settlement.
            let count = bureaucrats_per_faction.get(&fid).copied().unwrap_or(0);
            let debit = pay * count as f32;
            if let Some(settlement_entity) = settlement_map
                .by_faction
                .get(&fid)
                .and_then(|ids| ids.first().copied())
                .and_then(|sid| settlement_map.by_id.get(&sid).copied())
            {
                if let Ok(mut settlement) = settlements.get_mut(settlement_entity) {
                    settlement.treasury -= debit;
                    if settlement.treasury < 0.0 {
                        settlement.treasury = 0.0;
                    }
                }
            }

            // Credit each bureaucrat.
            for (prof, member, mut econ) in bureaucrats.iter_mut() {
                if *prof != Profession::Bureaucrat || member.faction_id != fid {
                    continue;
                }
                econ.currency += pay;
            }
        }

        // Update empty-streak.
        if let Some(faction) = registry.factions.get_mut(&fid) {
            if full {
                faction.bureaucrat_treasury_empty_streak = 0;
            } else {
                faction.bureaucrat_treasury_empty_streak = faction
                    .bureaucrat_treasury_empty_streak
                    .saturating_add(BUREAUCRAT_SALARY_INTERVAL as u32);
            }
        }
    }
}

/// Pluralist Economy R11: per-day tribute payment from each
/// subordinate faction's treasury to its overlord's treasury.
/// Treasury-to-treasury transfer; agent currency untouched. The
/// amount is capped at the subordinate's available treasury (no
/// debt). Total system currency is conserved.
pub fn tribute_payment_system(clock: Res<SimClock>, mut registry: ResMut<FactionRegistry>) {
    if clock.tick % TRIBUTE_CADENCE != 0 {
        return;
    }

    // Collect (subordinate, dominant) pairs from `subordinate_to` so
    // we can mutate both treasuries safely. Snapshot first to avoid
    // double-borrow during the transfer pass.
    let pairs: Vec<(u32, u32)> = registry
        .factions
        .iter()
        .filter_map(|(id, data)| data.subordinate_to.map(|d| (*id, d)))
        .collect();

    for (subordinate, dominant) in pairs {
        // Decide the transfer amount: min(TRIBUTE_PER_DAY,
        // subordinate's treasury). Read first.
        let avail = match registry.factions.get(&subordinate) {
            Some(s) => s.treasury,
            None => continue,
        };
        if avail <= 0.0 {
            continue;
        }
        let amount = TRIBUTE_PER_DAY.min(avail);

        // Debit subordinate.
        if let Some(s) = registry.factions.get_mut(&subordinate) {
            s.treasury -= amount;
            if s.treasury < 0.0 {
                s.treasury = 0.0;
            }
        }
        // Credit dominant.
        if let Some(d) = registry.factions.get_mut(&dominant) {
            d.treasury += amount;
        } else {
            // Overlord vanished; refund subordinate to keep invariant.
            if let Some(s) = registry.factions.get_mut(&subordinate) {
                s.treasury += amount;
            }
        }
    }
}

/// Pluralist Economy R6 follow-on: each household with sufficient
/// treasury posts a Tools craft contract once per game-day. The
/// contract is funded from the household's treasury via
/// `post_craft_contract_from_treasury` (escrowed to the household
/// head as proxy beneficiary). The posting carries
/// `poster_class=HouseholdHead` + `reward > 0` so R9's `U_bid`
/// scorer routes through the paid branch when smiths claim it.
///
/// This is the load-bearing system that makes capitalist factions
/// observably economically active. Without it, capitalist
/// households exist but post nothing — workers stay idle.
///
/// **Cadence**: once per `HOUSEHOLD_POSTING_CADENCE` (one game-day).
/// All qualifying households post on the same tick — adequate for
/// today's small populations; per-household stagger is a future
/// optimisation.
///
/// **Posting target**: M3 — the head's `MaslowTier::next_unmet`
/// picks the recipe. Tier-3 Belonging households commission Woven
/// Cloth (recipe 4); Tier-4 Esteem households commission Tools
/// (recipe 0); other tiers fall back to Tools as the always-buildable
/// safety net so the household isn't a dead poster while higher tiers
/// are dormant. Recipes whose `tech_gate` the *village* hasn't
/// learned are skipped; if the picked recipe is gated and unavailable
/// the system falls through to Tools (always recipe 0, ungated by
/// FLINT_KNAPPING in fresh paleolithic factions). Food/Bed/Wall
/// contracts will land alongside `post_stockpile_contract_from_treasury`
/// / `post_build_contract_from_treasury` (M3 follow-on) — until those
/// helpers exist, food demand routes through M4's chief buy-orders +
/// M5's market-buy path instead of household contracts.
pub fn household_contract_posting_system(world: &mut World) {
    use crate::simulation::crafting::craft_recipes;
    use crate::simulation::goals::MaslowTier;
    use crate::simulation::needs::Needs;

    let now = match world.get_resource::<SimClock>() {
        Some(c) => c.tick,
        None => return,
    };
    if now % HOUSEHOLD_POSTING_CADENCE != 0 {
        return;
    }

    // Snapshot eligible households so we can later mutate the
    // registry + JobBoard without long-lived borrows. Each entry:
    // (household_id, parent_village_id, head_entity, recipe_id).
    let intents: Vec<(u32, u32, Entity, RecipeId)> = {
        let registry = world.resource::<FactionRegistry>();
        let mut out = Vec::new();
        for (&hid, data) in registry.factions.iter() {
            // Only sub-faction households (those with a parent) are
            // eligible. Top-level village factions don't post via
            // this path — they'd post via the bureaucrat system if
            // they had `state_funds_public_works=true` (R10+).
            let Some(parent) = data.parent_faction else {
                continue;
            };
            if data.treasury < HOUSEHOLD_MIN_TREASURY_FOR_POSTING {
                continue;
            }
            let Some(head) = data.household_head else {
                continue;
            };
            // Use community-adoption (not chief-Aware) so a village
            // that only *heard* of weaving doesn't issue Cloth
            // contracts the chief can't actually fulfil.
            let village_techs = match registry.factions.get(&parent) {
                Some(v) => crate::simulation::technology_adoption::community_adoption_bitset(v),
                None => continue,
            };
            // Pick the recipe driven by the head's Maslow tier. Read
            // `Needs` directly from the world; if missing (e.g. head
            // entity already despawned) the household sits idle this
            // cycle.
            let needs = match world.get::<Needs>(head) {
                Some(n) => n,
                None => continue,
            };
            let tier = MaslowTier::next_unmet(needs);
            let recipe = pick_household_recipe(tier, &village_techs);
            out.push((hid, parent, head, recipe));
        }
        out
    };

    for (household_id, village_id, head, recipe) in intents {
        // Validate the recipe one last time before posting (defence
        // in depth: pick_household_recipe should never return an
        // invalid id, but the recipe table is decoupled).
        if craft_recipes().get(recipe as usize).is_none() {
            continue;
        }
        let _escrow = crate::simulation::jobs::post_craft_contract_from_treasury(
            world,
            household_id,
            village_id,
            head,
            recipe,
            1,
            HOUSEHOLD_CONTRACT_REWARD,
            None,
        );
        // post_craft_contract_from_treasury returns None on
        // insufficient treasury / invalid recipe. Either way we
        // proceed — if a contract didn't post this tick, the next
        // cadence cycle will retry once funds permit.
    }
}

/// Maslow-tier → craft-recipe selection for `household_contract_posting_system`.
/// Tier-3 Belonging → Woven Cloth (recipe 4) for clothing/social
/// signalling. Tier-4 Esteem → Stone Tools (recipe 0). Lower tiers
/// (Physiological / Safety) currently have no household-craftable
/// remedy — they fall through to Tools too so the household isn't a
/// dead poster while M4 buy-orders + M5 market-buy paths handle the
/// food/build supply side. Recipes whose `tech_gate` isn't met by the
/// village fall back to Tools (recipe 0 — gated on FLINT_KNAPPING which
/// every starting faction learns).
pub(crate) fn pick_household_recipe(
    tier: Option<crate::simulation::goals::MaslowTier>,
    village_techs: &FactionTechs,
) -> RecipeId {
    use crate::simulation::crafting::craft_recipes;
    use crate::simulation::goals::MaslowTier;

    let preferred: RecipeId = match tier {
        Some(MaslowTier::Belonging) => 4, // Woven Cloth
        Some(MaslowTier::Esteem) => 0,    // Stone Tools
        _ => 0,                           // Stone Tools default
    };
    // Tech-gate check against the village's `FactionTechs`. A village
    // without LOOM_WEAVING can't fulfil a Cloth contract → fall back
    // to Tools. Tools is recipe 0 — gated on FLINT_KNAPPING which
    // every faction learns at spawn (paleolithic seed).
    if let Some(recipe) = craft_recipes().get(preferred as usize) {
        if let Some(tech) = recipe.tech_gate {
            if !village_techs.has(tech) {
                return 0;
            }
        }
        preferred
    } else {
        0
    }
}

/// Per-faction chief decision: scan a `HUNT_SCAN_RADIUS` window around
/// `home_tile` for living Wolves/Deer, pick the species with highest count,
/// and post a `HuntOrder::Hunt` (or `HuntOrder::Scout` if nothing's nearby).
/// Runs once per `HUNT_DECISION_CADENCE` per faction; factions stagger
/// across the cadence by `faction_id` so the workload spreads. Also writes
/// `nearby_prey_count` to drive `faction_hunter_assignment_system`.
pub fn chief_hunt_order_system(
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    mut registry: ResMut<FactionRegistry>,
    prey_query: Query<
        (&Transform, &super::combat::Health),
        Or<(With<super::animals::Wolf>, With<super::animals::Deer>)>,
    >,
    wolf_q: Query<(), With<super::animals::Wolf>>,
    deer_q: Query<(), With<super::animals::Deer>>,
) {
    // Each faction's decision phase is `fid % HUNT_DECISION_CADENCE`, so
    // factions fire on different ticks throughout the day. Same for the
    // mid-day invalidation sweep, offset by half a cadence so it doesn't
    // collide with the decision tick. Modulo + branch is cheap; real work
    // only happens once per faction per cadence.
    let factions: Vec<u32> = registry.factions.keys().copied().collect();
    for fid in factions {
        if fid == SOLO {
            continue;
        }
        let phase_decide = (fid as u64) % HUNT_DECISION_CADENCE;
        let phase_invalidate =
            ((fid as u64).wrapping_add(HUNT_INVALIDATE_CADENCE / 2)) % HUNT_INVALIDATE_CADENCE;
        let do_decide = clock.tick % HUNT_DECISION_CADENCE == phase_decide;
        let do_invalidate = !do_decide && clock.tick % HUNT_INVALIDATE_CADENCE == phase_invalidate;
        if do_decide {
            decide_for_faction(
                fid,
                &mut registry,
                &spatial,
                &prey_query,
                &wolf_q,
                &deer_q,
                clock.tick,
            );
        } else if do_invalidate {
            invalidate_for_faction(
                fid,
                &mut registry,
                &spatial,
                &prey_query,
                &wolf_q,
                &deer_q,
                clock.tick,
            );
        }
    }
}

fn invalidate_for_faction(
    fid: u32,
    registry: &mut FactionRegistry,
    spatial: &SpatialIndex,
    prey_query: &Query<
        (&Transform, &super::combat::Health),
        Or<(With<super::animals::Wolf>, With<super::animals::Deer>)>,
    >,
    wolf_q: &Query<(), With<super::animals::Wolf>>,
    deer_q: &Query<(), With<super::animals::Deer>>,
    tick: u64,
) {
    let Some(faction) = registry.factions.get_mut(&fid) else {
        return;
    };
    let Some(order) = faction.hunt_order.as_ref() else {
        return;
    };
    // Stale-muster timeout: the order has been live longer than agents
    // should patiently wait for stragglers, and the party never deployed.
    if let HuntOrder::Hunt { deployed_tick, .. } = order {
        if deployed_tick.is_none() && tick.saturating_sub(order.posted_tick()) > HUNT_PARTY_TIMEOUT
        {
            faction.hunt_order = None;
            return;
        }
    }
    // Target-area-empty: prey has moved on or been butchered.
    if let HuntOrder::Hunt {
        area_tile, species, ..
    } = order
    {
        let mut count = 0u32;
        let cx = area_tile.0 as i32;
        let cy = area_tile.1 as i32;
        for dy in -8..=8 {
            for dx in -8..=8 {
                let tx = cx + dx;
                let ty = cy + dy;
                for &e in spatial.get(tx, ty) {
                    let matches_species = match species {
                        super::corpse::CorpseSpecies::Wolf => wolf_q.get(e).is_ok(),
                        super::corpse::CorpseSpecies::Deer => deer_q.get(e).is_ok(),
                    };
                    if matches_species {
                        if let Ok((_, h)) = prey_query.get(e) {
                            if !h.is_dead() {
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
        if count == 0 {
            faction.hunt_order = None;
        }
    }
}

fn decide_for_faction(
    fid: u32,
    registry: &mut FactionRegistry,
    spatial: &SpatialIndex,
    prey_query: &Query<
        (&Transform, &super::combat::Health),
        Or<(With<super::animals::Wolf>, With<super::animals::Deer>)>,
    >,
    wolf_q: &Query<(), With<super::animals::Wolf>>,
    deer_q: &Query<(), With<super::animals::Deer>>,
    tick: u64,
) {
    let Some(faction) = registry.factions.get_mut(&fid) else {
        return;
    };
    if !faction.techs.has(HUNTING_SPEAR) {
        faction.hunt_order = None;
        faction.nearby_prey_count = 0;
        return;
    }
    let (htx, hty) = faction.home_tile;
    let mut wolf_count = 0u32;
    let mut deer_count = 0u32;
    let mut wolf_centroid = (0i64, 0i64);
    let mut deer_centroid = (0i64, 0i64);
    for dy in -HUNT_SCAN_RADIUS..=HUNT_SCAN_RADIUS {
        for dx in -HUNT_SCAN_RADIUS..=HUNT_SCAN_RADIUS {
            if dx * dx + dy * dy > HUNT_SCAN_RADIUS * HUNT_SCAN_RADIUS {
                continue;
            }
            let tx = htx as i32 + dx;
            let ty = hty as i32 + dy;
            for &e in spatial.get(tx, ty) {
                let Ok((_, health)) = prey_query.get(e) else {
                    continue;
                };
                if health.is_dead() {
                    continue;
                }
                if wolf_q.get(e).is_ok() {
                    wolf_count += 1;
                    wolf_centroid.0 += tx as i64;
                    wolf_centroid.1 += ty as i64;
                } else if deer_q.get(e).is_ok() {
                    deer_count += 1;
                    deer_centroid.0 += tx as i64;
                    deer_centroid.1 += ty as i64;
                }
            }
        }
    }
    faction.nearby_prey_count = wolf_count + deer_count;
    if wolf_count == 0 && deer_count == 0 {
        faction.hunt_order = Some(HuntOrder::Scout { posted_tick: tick });
        return;
    }
    let (species, count, centroid) = if wolf_count >= deer_count {
        (
            super::corpse::CorpseSpecies::Wolf,
            wolf_count,
            wolf_centroid,
        )
    } else {
        (
            super::corpse::CorpseSpecies::Deer,
            deer_count,
            deer_centroid,
        )
    };
    let area_tile = (
        (centroid.0 / count as i64) as i32,
        (centroid.1 / count as i64) as i32,
    );
    let target_party_size = match species {
        super::corpse::CorpseSpecies::Wolf => 4,
        super::corpse::CorpseSpecies::Deer => 2,
    };
    faction.hunt_order = Some(HuntOrder::Hunt {
        species,
        area_tile,
        target_party_size,
        mustered: Vec::new(),
        deployed_tick: None,
        posted_tick: tick,
    });
}

/// Per-head food reserve threshold for farmer recruitment. Below this we ramp
/// up farmers; above `FARMER_DEMOTE_RATIO * threshold` we ramp them down.
/// Hysteresis prevents the role from flapping while reserves hover at target.
const FARMER_PROMOTE_PER_HEAD: f32 = 32.0;
const FARMER_DEMOTE_RATIO: f32 = 1.6;

/// Phase 4b (wage-aware-labor-market-v2) survival override. When per-head
/// food reserves drop below this floor, the Hunter / Bureaucrat / Crafter
/// assignment systems zero their target headcount for the affected
/// faction — labor surrenders back to `Profession::None`, which the
/// Farmer ramp then poaches into food production. The floor is set
/// well below the `FARMER_PROMOTE_PER_HEAD = 32` ramp trigger so it
/// only fires in genuine emergencies; bouncing between the two thresholds
/// (e.g. a single bad season) leaves the Farmer ramp in charge while
/// hunters keep working.
pub const FARMER_SURVIVAL_FLOOR: f32 = 16.0;
/// Anchor farmer reassignment to game time, not every tick. Six game-hours
/// (~`TICKS_PER_DAY/4`) is enough to react to harvest spikes without flapping.
pub const FARMER_ASSIGNMENT_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

pub fn faction_profession_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    ownership: Res<crate::simulation::capital::WorkshopOwnership>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plots: Query<&crate::simulation::land::Plot>,
    mut query: Query<(
        Entity,
        &mut Profession,
        &FactionMember,
        &Skills,
        &EconomicAgent,
        &crate::simulation::carry::Carrier,
        &Transform,
        Option<&crate::simulation::reproduction::HouseholdMember>,
    )>,
) {
    if clock.tick % FARMER_ASSIGNMENT_CADENCE != 0 {
        return;
    }
    for (&faction_id, faction) in registry.factions.iter_mut() {
        if !faction.techs.has(CROP_CULTIVATION) {
            continue;
        }

        // Anticipatory recruitment: ramp up farmers when per-head food reserves
        // fall below FARMER_PROMOTE_PER_HEAD; ramp down when reserves climb past
        // FARMER_PROMOTE_PER_HEAD * FARMER_DEMOTE_RATIO. Target = ~20% of adults
        // when low. The previous threshold (food_total < 100) only fired during
        // outright starvation for any tribe larger than ~5.
        let members = faction.member_count.max(1);
        let per_head = faction.storage.food_total() / members as f32;
        let promote_threshold = FARMER_PROMOTE_PER_HEAD;
        let demote_threshold = FARMER_PROMOTE_PER_HEAD * FARMER_DEMOTE_RATIO;
        // Phase 4b: rank candidates by `(expected_wage(Farmer), Farming skill)`
        // so that wage-signal-driven promotion picks competent agents over
        // arbitrary scan order. `capital_factor` averages tool / workshop /
        // land affinity — a household with an Agricultural plot under
        // non-StateOwned tenure scores 1.5 on land, lifting EV over a
        // landless peer with identical Farming skill.
        let mut current_farmer_set: Vec<(Entity, f32, u32)> = Vec::new();
        let mut none_set: Vec<(Entity, f32, u32)> = Vec::new();
        for (entity, prof, member, skills, agent, carrier, xf, household_opt) in query.iter() {
            if member.faction_id != faction_id {
                continue;
            }
            let farming = skills.0[SkillKind::Farming as usize];
            let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
            let cap = crate::simulation::capital::capital_factor(
                agent,
                carrier,
                tile,
                faction_id,
                household_opt,
                Profession::Farmer,
                &ownership,
                &plots,
                &plot_index,
            );
            let ev = crate::simulation::profession_choice::expected_wage(
                faction,
                Profession::Farmer,
                skills,
                cap,
            );
            match *prof {
                Profession::Farmer => current_farmer_set.push((entity, ev, farming)),
                Profession::None => none_set.push((entity, ev, farming)),
                _ => {}
            }
        }
        let current_farmers_for_target = current_farmer_set.len() as u32;
        // §7: plot-demand pressure. Even when per-head food is comfortably
        // above the demote threshold, communal plots that exist but have
        // nobody assigned should still recruit a Farmer — otherwise a
        // village sitting on `per_head = 25` with three unworked plots
        // promotes zero farmers and the chief postings go unclaimed.
        let plot_demand = crate::simulation::farm::state_owned_ag_plots_for_faction(
            faction_id,
            &plot_index,
            &plots,
        )
        .len() as u32;
        let plot_target = plot_demand.min((faction.member_count / 3).max(1));
        let food_target_when_low = (faction.member_count / 5).max(1);
        let target_farmers = if per_head < promote_threshold {
            // Below promote threshold: emergency food ramp PLUS coverage
            // for any unworked plots.
            food_target_when_low.max(plot_target)
        } else if per_head > demote_threshold {
            // Reserves abundant: keep enough farmers to cover plots, but
            // shed the rest. Without this floor, plots stay unworked when
            // the village happens to be flush.
            plot_target
        } else {
            current_farmers_for_target.max(plot_target)
        };
        let current_farmers = current_farmers_for_target;

        if current_farmers < target_farmers {
            let to_assign = (target_farmers - current_farmers) as usize;
            // Highest (EV, skill) first.
            none_set.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.2.cmp(&a.2))
            });
            let promote: ahash::AHashSet<Entity> = none_set
                .into_iter()
                .take(to_assign)
                .map(|(e, _, _)| e)
                .collect();
            for (entity, mut prof, _, _, _, _, _, _) in query.iter_mut() {
                if promote.contains(&entity) {
                    *prof = Profession::Farmer;
                }
            }
        } else if current_farmers > target_farmers {
            let to_unassign = (current_farmers - target_farmers) as usize;
            // Lowest (EV, skill) first.
            current_farmer_set.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.2.cmp(&b.2))
            });
            let demote: ahash::AHashSet<Entity> = current_farmer_set
                .into_iter()
                .take(to_unassign)
                .map(|(e, _, _)| e)
                .collect();
            for (entity, mut prof, _, _, _, _, _, _) in query.iter_mut() {
                if demote.contains(&entity) {
                    *prof = Profession::None;
                }
            }
        }
    }
}

#[derive(Component, Clone, Copy)]
pub struct FactionMember {
    pub faction_id: u32,
    pub bond_target: Option<Entity>,
    pub bond_timer: u8,
}

#[derive(Component)]
pub struct FactionCenter;

/// Marks the designated tribal chief of a faction.
/// Inserted on the faction founder at formation; re-elected by `chief_selection_system`
/// if the current chief leaves or dies.
#[derive(Component)]
pub struct FactionChief;

#[derive(Component)]
pub struct PlayerFactionMarker;

/// Marks an entity as a storage drop-off point for a faction.
/// Spawned at the faction's home tile on creation; additional tiles can be added later.
#[derive(Component, Clone, Copy)]
pub struct FactionStorageTile {
    pub faction_id: u32,
}

/// Fast lookup from tile coords to faction_id for all storage tiles.
#[derive(Resource, Default)]
pub struct StorageTileMap {
    pub tiles: AHashMap<(i32, i32), u32>,
    pub by_faction: AHashMap<u32, Vec<(i32, i32)>>,
}

impl StorageTileMap {
    /// Plain Manhattan-nearest storage tile. Kept as the cheap accessor
    /// for non-routing / bookkeeping callers (SOLO fallback, landlord
    /// sharecrop deposit, storage backend, tests) where the worker isn't
    /// about to walk a river-detour to it. Routing-relevant deposit picks
    /// (the gather → DepositToFactionStorage chain) go through
    /// `nearest_for_faction_reachable`, which is detour-aware.
    pub fn nearest_for_faction(&self, faction_id: u32, from: (i32, i32)) -> Option<(i32, i32)> {
        self.by_faction
            .get(&faction_id)?
            .iter()
            .min_by_key(|&&(tx, ty)| (tx as i32 - from.0).abs() + (ty as i32 - from.1).abs())
            .copied()
    }

    /// Like `nearest_for_faction` but skips storage tiles that aren't
    /// reachable from the source tile via component-exact `tile_reachable`.
    /// Closes the gap where the Manhattan-closest storage tile sits in a
    /// disconnected component (across a wall, in a separated cave, on a
    /// different megachunk surface) and the dispatcher would happily pick
    /// it, then fail at `assign_task_with_routing` and burn a tick before
    /// re-evaluating.
    ///
    /// Each storage tile's z is resolved with `nearest_standable_z` so a
    /// raised but-reachable platform beats a same-z tile in a sealed
    /// chamber. The `source_tile` is typically the future pickup / gather
    /// tile (not the agent's current tile) so deposit chains gate on the
    /// post-pickup reachability rather than the worker's pre-walk state.
    ///
    /// Falls back to the connectivity-blind result when the reachability
    /// filter rejects every tile — better to attempt a (likely-failing)
    /// route than return `None` and have the dispatcher emit nothing at
    /// all.
    ///
    /// Ranking is **detour-aware**: among reachable tiles the one cheapest
    /// to actually *walk* to from `source_tile` wins, not the
    /// straight-line-nearest. A storage tile across a river scores its
    /// walk-around cost, so the gather → deposit chain doesn't bake in a
    /// huge return leg. `from` is retained as the chebyshev tiebreak the
    /// detour estimate folds in (`max(chebyshev, hop-cost)`).
    pub fn nearest_for_faction_reachable(
        &self,
        faction_id: u32,
        from: (i32, i32),
        source_tile: (i32, i32, i8),
        chunk_map: &crate::world::chunk::ChunkMap,
        chunk_graph: &crate::pathfinding::chunk_graph::ChunkGraph,
        chunk_router: &crate::pathfinding::chunk_router::ChunkRouter,
        connectivity: &crate::pathfinding::connectivity::ChunkConnectivity,
    ) -> Option<(i32, i32)> {
        let _ = from;
        let tiles = self.by_faction.get(&faction_id)?;
        let est =
            crate::pathfinding::detour::DetourEstimator::new(chunk_router, chunk_graph);
        let pick = |reachable_only: bool| {
            tiles
                .iter()
                .filter(|&&(tx, ty)| {
                    if !reachable_only {
                        return true;
                    }
                    let tz = chunk_map.nearest_standable_z(tx, ty, source_tile.2 as i32) as i8;
                    connectivity.tile_reachable(chunk_graph, source_tile, (tx, ty, tz))
                })
                .min_by_key(|&&(tx, ty)| {
                    let tz = chunk_map.nearest_standable_z(tx, ty, source_tile.2 as i32) as i8;
                    est.tiles((source_tile.0, source_tile.1), source_tile.2, (tx, ty), tz)
                })
                .copied()
        };
        pick(true).or_else(|| pick(false))
    }
}

/// Tile-scoped reservations on storage stocks. Two agents committing to the
/// same one-unit stack used to be possible because the resolver only saw raw
/// `GroundItem.qty`; now the resolver subtracts entries here from the
/// effective stock. Each `WithdrawMaterial` dispatch increments the entry,
/// and every task-teardown path (success, race-loss, plan abort) decrements
/// it via `release_reservation` so the map stays consistent under churn.
///
/// Wrapped in a `Mutex` because `plan_execution_system` runs over agents in
/// parallel via `par_iter_mut`. Both reads (resolver) and writes (dispatch +
/// release) take the lock; critical sections are a single hashmap op each.
#[derive(Resource, Default)]
pub struct StorageReservations {
    inner: std::sync::Mutex<AHashMap<((i32, i32), ResourceId), u32>>,
}

impl StorageReservations {
    pub fn add(&self, tile: (i32, i32), resource_id: ResourceId, qty: u32) {
        if qty == 0 {
            return;
        }
        let mut m = self.inner.lock().unwrap();
        *m.entry((tile, resource_id)).or_insert(0) += qty;
    }

    pub fn sub(&self, tile: (i32, i32), resource_id: ResourceId, qty: u32) {
        if qty == 0 {
            return;
        }
        let mut m = self.inner.lock().unwrap();
        if let Some(slot) = m.get_mut(&(tile, resource_id)) {
            *slot = slot.saturating_sub(qty);
            if *slot == 0 {
                m.remove(&(tile, resource_id));
            }
        }
    }

    pub fn get(&self, tile: (i32, i32), resource_id: ResourceId) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .get(&(tile, resource_id))
            .copied()
            .unwrap_or(0)
    }

    /// Snapshot total reserved qty across all (tile, resource) pairs. Used by
    /// the inspector for debugging.
    pub fn total(&self) -> u32 {
        self.inner.lock().unwrap().values().sum()
    }
}

/// Decrement and clear the reservation tracked on a `PersonAI`. Safe to call
/// from any teardown path; no-ops when the agent has no live reservation.
pub fn release_reservation(
    reservations: &StorageReservations,
    ai: &mut crate::simulation::person::PersonAI,
) {
    if let Some(resource_id) = ai.reserved_resource {
        reservations.sub(ai.reserved_tile, resource_id, ai.reserved_qty as u32);
    }
    ai.reserved_resource = None;
    ai.reserved_qty = 0;
}

/// Computed cache of resources stored on all storage tiles for a
/// faction. Updated each Economy tick by `compute_faction_storage_system`.
/// Phase 2d: keyed on `ResourceId` instead of `Good`.
#[derive(Default, Clone)]
pub struct FactionStorage {
    pub totals: AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
}

impl FactionStorage {
    /// Look up `totals` by `ResourceId`. Returns 0 for resources that
    /// have never been deposited. Legacy `Good`-typed callers pass
    /// `good.into()` at the boundary.
    pub fn stock_of(&self, resource_id: crate::economy::resource_catalog::ResourceId) -> u32 {
        self.totals.get(&resource_id).copied().unwrap_or(0)
    }

    pub fn food_total(&self) -> f32 {
        // Sum every catalog edible — adding a new food to
        // `assets/data/resources/*.ron` is automatically counted here.
        crate::economy::core_ids::edibles()
            .iter()
            .map(|id| self.totals.get(id).copied().unwrap_or(0) as f32)
            .sum()
    }
    pub fn grain_seed_total(&self) -> u32 {
        crate::economy::core_ids::GrainSeed
            .get()
            .and_then(|id| self.totals.get(id).copied())
            .unwrap_or(0)
    }
    pub fn berry_seed_total(&self) -> u32 {
        crate::economy::core_ids::BerrySeed
            .get()
            .and_then(|id| self.totals.get(id).copied())
            .unwrap_or(0)
    }
    pub fn seed_total(&self) -> u32 {
        self.grain_seed_total() + self.berry_seed_total()
    }
}

impl Default for FactionMember {
    fn default() -> Self {
        Self {
            faction_id: SOLO,
            bond_target: None,
            bond_timer: 0,
        }
    }
}

// ── Faction culture & lineage ─────────────────────────────────────────────────

/// Architectural / strategic style a faction grows into. Picked at faction
/// creation; drives the settlement planner's zone-placement strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutStyle {
    /// Tight, overlapping zones around a small core.
    Compact,
    /// Wide gaps between zones; large outward footprint.
    Sprawling,
    /// Single E-W axis with branches.
    Linear,
    /// Concentric rings around the center (closest to current Neolithic).
    Radial,
    /// Inner residential core walled tightly; agriculture outside.
    Citadel,
}

impl LayoutStyle {
    pub const ALL: [LayoutStyle; 5] = [
        LayoutStyle::Compact,
        LayoutStyle::Sprawling,
        LayoutStyle::Linear,
        LayoutStyle::Radial,
        LayoutStyle::Citadel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayoutStyle::Compact => "Compact",
            LayoutStyle::Sprawling => "Sprawling",
            LayoutStyle::Linear => "Linear",
            LayoutStyle::Radial => "Radial",
            LayoutStyle::Citadel => "Citadel",
        }
    }
}

/// Faction-wide lifestyle archetype. Settled factions found Settlements,
/// carve plots, and build permanent structures. Nomadic factions skip
/// `Settlement` creation, store goods in pooled member/pack-animal/PackBundle
/// inventories instead of `FactionStorageTile`, and migrate when local
/// resources thin out. See `~/.claude/plans/i-want-to-add-snappy-manatee.md`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Lifestyle {
    #[default]
    Settled,
    Nomadic,
}

impl Lifestyle {
    pub fn is_nomadic(self) -> bool {
        matches!(self, Lifestyle::Nomadic)
    }
    pub fn name(self) -> &'static str {
        match self {
            Lifestyle::Settled => "Settled",
            Lifestyle::Nomadic => "Nomadic",
        }
    }
}

/// Per-faction architectural and behavioural personality. Rolled once at
/// faction creation from a deterministic seed. Drives the settlement planner,
/// build-candidate scoring, raid frequency, and ritual cadence.
#[derive(Clone, Debug)]
pub struct FactionCulture {
    pub style: LayoutStyle,
    /// 0..=255 — low = wide spacing, high = packed footprints.
    pub density: u8,
    /// 0..=255 — biases wall priority and ring count.
    pub defensive: u8,
    /// 0..=255 — biases shrines, monuments, ritual cadence.
    pub ceremonial: u8,
    /// 0..=255 — biases markets and storage.
    pub mercantile: u8,
    /// 0..=255 — biases barracks, raid frequency.
    pub martial: u8,
    pub seed: u32,
}

impl FactionCulture {
    /// Roll a deterministic culture from a seed (typically `home_tile + faction_id`).
    pub fn roll(seed: u32) -> Self {
        // Cheap deterministic hash steps — splitmix-ish.
        let mut s = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0xDEAD_BEEF);
        let mut next = || {
            s ^= s >> 16;
            s = s.wrapping_mul(0x85EB_CA6B);
            s ^= s >> 13;
            s = s.wrapping_mul(0xC2B2_AE35);
            s ^= s >> 16;
            s
        };
        let style = LayoutStyle::ALL[(next() as usize) % LayoutStyle::ALL.len()];
        // Style template + per-trait jitter ±40.
        let (mut den, mut def, mut cer, mut mer, mut mar) = match style {
            LayoutStyle::Compact => (200u8, 140, 110, 110, 110),
            LayoutStyle::Sprawling => (60, 90, 110, 130, 90),
            LayoutStyle::Linear => (130, 110, 100, 160, 110),
            LayoutStyle::Radial => (140, 130, 130, 110, 110),
            LayoutStyle::Citadel => (180, 220, 100, 90, 170),
        };
        let jitter = |base: u8, raw: u32| -> u8 {
            let delta = (raw % 81) as i32 - 40; // -40..=+40
            (base as i32 + delta).clamp(0, 255) as u8
        };
        den = jitter(den, next());
        def = jitter(def, next());
        cer = jitter(cer, next());
        mer = jitter(mer, next());
        mar = jitter(mar, next());
        Self {
            style,
            density: den,
            defensive: def,
            ceremonial: cer,
            mercantile: mer,
            martial: mar,
            seed,
        }
    }
}

/// Dynastic lineage information for a faction. Successor chiefs inherit a
/// modulated culture (small drift per generation); child agent names are
/// derived from `root`.
#[derive(Clone, Debug, Default)]
pub struct FactionLineage {
    /// Naming root (e.g., "Aren") used to generate descendant names.
    pub root: String,
    /// Founder's full name. Stable across the faction's lifetime.
    pub founder: String,
    /// Number of chief successions since founding.
    pub generation: u32,
}

impl FactionLineage {
    pub fn from_seed(seed: u32) -> Self {
        const ROOTS: &[&str] = &[
            "Aren", "Bryn", "Cael", "Doran", "Elin", "Faro", "Garen", "Hela", "Irek", "Joran",
            "Kael", "Lyr", "Maren", "Nyx", "Oran", "Pyra", "Quinn", "Rhea", "Sora", "Talin", "Uma",
            "Vale", "Wren", "Yara",
        ];
        const SUFFIX: &[&str] = &["", "-tha", "-mir", "-ros", "-vyn", "-dor", "-an", "-eth"];
        let r = ROOTS[(seed as usize) % ROOTS.len()];
        let s = SUFFIX[((seed >> 8) as usize) % SUFFIX.len()];
        Self {
            root: r.to_string(),
            founder: format!("{r}{s}"),
            generation: 0,
        }
    }
}

/// u64 bitset storing which technologies are unlocked (bits 0-42).
#[derive(Clone, Copy, Debug, Default)]
pub struct FactionTechs(pub u64);

impl FactionTechs {
    #[inline]
    pub fn has(&self, id: TechId) -> bool {
        self.0 & (1u64 << id) != 0
    }
    #[inline]
    pub fn unlock(&mut self, id: TechId) {
        self.0 |= 1u64 << id;
    }
    /// Bitwise OR of two tech sets. Used by the construction poster pool
    /// to fold resident chief + architect Learned snapshots into the
    /// settlement's buildable surface.
    #[inline]
    pub fn union(&self, other: &FactionTechs) -> FactionTechs {
        FactionTechs(self.0 | other.0)
    }
}

/// Per-season activity counters, reset after each tech discovery pass.
#[derive(Clone, Debug, Default)]
pub struct ActivityLog(pub [u32; ACTIVITY_COUNT]);

impl ActivityLog {
    #[inline]
    pub fn increment(&mut self, kind: ActivityKind) {
        self.0[kind as usize] = self.0[kind as usize].saturating_add(1);
    }
    #[inline]
    pub fn get(&self, kind: ActivityKind) -> u32 {
        self.0[kind as usize]
    }
    pub fn reset(&mut self) {
        self.0 = [0; ACTIVITY_COUNT];
    }
}

/// Whether the band's shelters are currently pitched at `home_tile`
/// or fully packed for travel. AI factions atomically pack-and-pitch
/// in a single Sequential tick (`nomad_migration_commit_system`); a
/// player-driven band stays in `Packed` between explicit `PackCamp`
/// and `PitchCamp` commands and is free to forage / sleep / fight
/// while mobile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CampState {
    Pitched,
    Packed { since_tick: u32 },
}

impl Default for CampState {
    fn default() -> Self {
        CampState::Pitched
    }
}

/// Migration coarse phase. `Surveying` is the AI autopilot survey
/// window (player factions never enter it — `nomad_autopilot` gates).
/// `PendingCommit` validates the final target before the camp is touched.
/// AI factions then physically pack, travel, and pitch at the final
/// destination; player flow uses `CampState::{Pitched, Packed}` only.
/// Free-form player migration relies on Pack → wander → Pitch without
/// going through this state machine.
#[derive(Clone, Debug, Default)]
pub enum MigrationPhase {
    #[default]
    Idle,
    Surveying {
        started_tick: u32,
        scouts: Vec<Entity>,
        quadrants: [bool; 4],
    },
    PendingCommit {
        target: (i32, i32),
        chosen_tick: u32,
    },
    PackingCamp {
        target: (i32, i32),
        old_home: (i32, i32),
        started_tick: u32,
        radius: i32,
    },
    Traveling {
        target: (i32, i32),
        old_home: (i32, i32),
        departed_tick: u32,
        caravan_tile: (i32, i32),
    },
    PitchingCamp {
        target: (i32, i32),
        old_home: (i32, i32),
        started_tick: u32,
        pitch_started_tick: u32,
        repair_unlocked: bool,
    },
}

/// Phase 3: player-facing knob that biases `pick_migration_target`'s
/// component-score weights. AI factions default to `FreeRoute`
/// (uniform weights, matching the pre-Phase 3 baseline). Players
/// change this from the migration panel before scouting / committing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MigrationIntent {
    #[default]
    FreeRoute,
    FollowWater,
    FollowHerds,
    SeekWinterShelter,
    SeekSummerPasture,
    AvoidDanger,
}

/// Player-locked autonomy gate for packed nomads. `Hold` (default)
/// freezes autonomous workers and surfaces them as "Awaiting Orders" so
/// the player can issue explicit moves between Pack Camp and Pitch
/// Camp. `Forage` releases the gate so members may forage / sleep /
/// socialize / etc. like the pre-existing behaviour. Every `PackCamp`
/// resets this to `Hold`. Only consulted for player-driven nomadic
/// factions (`nomad_autopilot == false`); AI nomads ignore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PackedMigrationAutonomy {
    /// Strict: workers wait at their current tile and only execute
    /// explicit `PlayerCommand` orders / `Pack Camp` labor / manual
    /// `SendScout`.
    #[default]
    Hold,
    /// Permissive: settled-life work is still blocked, but the
    /// `allowed_while_packed` set (food / sleep / social / defence /
    /// care / play / scout) may dispatch autonomously.
    Forage,
}

impl PackedMigrationAutonomy {
    pub fn label(self) -> &'static str {
        match self {
            PackedMigrationAutonomy::Hold => "Hold",
            PackedMigrationAutonomy::Forage => "Forage",
        }
    }
}

impl MigrationIntent {
    /// Returns `[food, herd, water, biome, danger, distance_penalty]`
    /// multipliers applied per-component inside `pick_migration_target`.
    pub fn weights(self) -> [f32; 6] {
        match self {
            MigrationIntent::FreeRoute => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            MigrationIntent::FollowWater => [1.0, 0.7, 2.0, 1.0, 1.0, 1.0],
            MigrationIntent::FollowHerds => [0.8, 2.0, 1.0, 1.0, 1.0, 1.0],
            MigrationIntent::SeekWinterShelter => [1.0, 0.6, 1.0, 2.5, 1.0, 0.8],
            MigrationIntent::SeekSummerPasture => [1.5, 1.0, 1.0, 2.0, 1.0, 1.0],
            MigrationIntent::AvoidDanger => [0.8, 0.6, 1.0, 1.0, 3.0, 1.0],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MigrationIntent::FreeRoute => "Free Route",
            MigrationIntent::FollowWater => "Follow Water",
            MigrationIntent::FollowHerds => "Follow Herds",
            MigrationIntent::SeekWinterShelter => "Seek Winter Shelter",
            MigrationIntent::SeekSummerPasture => "Seek Summer Pasture",
            MigrationIntent::AvoidDanger => "Avoid Danger",
        }
    }
}

/// Phase 2: persisted camp-site candidate discovered by a scout or by
/// `pick_migration_target`. Stored in `FactionData.candidate_sites`
/// (cap `MAX_CANDIDATE_SITES`). The migration panel renders these as
/// map pins; the player picks one when setting a route. `validated`
/// flips to true once a manual scout has personally reached the tile.
#[derive(Clone, Debug)]
pub struct CampSiteCandidate {
    pub anchor: (i32, i32),
    pub z: i8,
    pub score: f32,
    pub reasons: Vec<CandidateReason>,
    pub discovered_tick: u32,
    pub validated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateReason {
    FreshWater,
    Pasture,
    Herd,
    Wolves,
    PoorShelter,
    LongCarry,
    SnowRisk,
    EnemyFaction,
    Sanitation,
}

impl CandidateReason {
    pub fn label(self) -> &'static str {
        match self {
            CandidateReason::FreshWater => "Fresh Water",
            CandidateReason::Pasture => "Pasture",
            CandidateReason::Herd => "Herd",
            CandidateReason::Wolves => "Wolves",
            CandidateReason::PoorShelter => "Poor Shelter",
            CandidateReason::LongCarry => "Long Carry",
            CandidateReason::SnowRisk => "Snow Risk",
            CandidateReason::EnemyFaction => "Enemy Faction",
            CandidateReason::Sanitation => "Sanitation",
        }
    }
}

pub const MAX_CANDIDATE_SITES: usize = 16;

/// Phase 4: per-faction cargo manifest for an in-flight migration.
/// `required` records "this is what the band needs to carry"
/// (computed at Pack time from `pack_camp_assets`); `loaded` records
/// where it ended up (which member or pack animal holds what);
/// `abandoned` lists overflow that exceeded carry capacity;
/// `deployed` accumulates redeposits as `Unpitch` runs at the new
/// site. Cleared on `MigrationPhase` returning to `Idle`.
#[derive(Clone, Debug, Default)]
pub struct CampCargoManifest {
    pub required: ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
    pub loaded: ahash::AHashMap<(Entity, crate::economy::resource_catalog::ResourceId), u32>,
    pub abandoned: Vec<(crate::economy::resource_catalog::ResourceId, u32)>,
    pub deployed: ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
    pub pitching_started_tick: Option<u32>,
    pub repair_unlocked: bool,
}

impl CampCargoManifest {
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
            && self.loaded.is_empty()
            && self.abandoned.is_empty()
            && self.deployed.is_empty()
            && self.pitching_started_tick.is_none()
            && !self.repair_unlocked
    }

    pub fn total_loaded(&self) -> u32 {
        self.loaded.values().copied().sum()
    }
}

pub struct FactionData {
    pub storage: FactionStorage,
    pub home_tile: (i32, i32),
    pub member_count: u32,
    pub raid_target: Option<u32>,
    pub under_raid: bool,
    pub techs: FactionTechs,
    /// **The single construction-tech surface.** Union of every resident
    /// chief + architect `PersonKnowledge.learned` across the faction's
    /// settlements (+ chief fallback), rewritten read-only every tick by
    /// `construction::refresh_construction_poster_pool_system`. Every
    /// build/civic/tier/shelter gate (`community_has`,
    /// `community_adoption_bitset`) reads this — construction no longer
    /// gates on community *adoption* anywhere. Empty until the first pool
    /// refresh; seeding drives tiers from `SeedConstructionProfile`
    /// instead, so the tick-0 emptiness is never observed by a gate.
    pub buildable_techs: FactionTechs,
    /// Community adoption stage per tech, derived every
    /// `ADOPTION_DERIVE_CADENCE` ticks by
    /// `technology_adoption::derive_tech_adoption_system`. **Analytics /
    /// UI only** — no construction/civic/tier/shelter gate reads this.
    /// The single build-tech surface is `buildable_techs`.
    pub tech_adoption: [crate::simulation::technology_adoption::AdoptionStage;
        crate::simulation::technology::TECH_COUNT],
    /// Tick at which each tech's `tech_adoption[i]` last *changed* (in
    /// either direction). Drives the "Adopted ≥ 1 game-year" boost into
    /// `Institutionalized` and the Phase 4 decay cooldown.
    pub stage_changed_at_tick: [u32; crate::simulation::technology::TECH_COUNT],
    /// Sparse ring buffer of recent successful uses per tech. Populated
    /// by craft / hunt / build executors; consumed by `derive_stage` for
    /// the "≥N successful uses in last K days" Adopted threshold.
    pub recent_tech_use: ahash::AHashMap<
        crate::simulation::technology::TechId,
        crate::simulation::technology_adoption::RecentTechUse,
    >,
    pub activity_log: ActivityLog,
    /// Phase 2d: keyed on `ResourceId` so consumers (recipe pipelines,
    /// HTN methods) can look up by catalog id without reverse-resolving
    /// to a legacy `Good` enum variant.
    pub resource_supply: ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
    pub resource_demand: ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
    /// The current tribal chief of this faction, if one has been designated.
    pub chief_entity: Option<Entity>,
    /// Architectural / behavioural personality. Drives planner, selector,
    /// raids, rituals.
    pub culture: FactionCulture,
    /// Dynastic naming + generation count. Updated at chief succession.
    pub lineage: FactionLineage,
    /// Tile of the structure currently being torn down for upgrade-replacement.
    /// `Some` while a deconstruct→rebuild cycle is in flight (one per faction).
    pub active_upgrade: Option<(i32, i32)>,
    /// Per-tick allocation across job kinds (gather/farm/build/craft/free).
    /// Recomputed every chief tick from the same pressure model that drives
    /// `compute_priority`. Consumed by `job_claim_system` as the per-kind cap
    /// instead of the old flat 50%-of-population rule.
    pub workforce_budget: crate::simulation::projects::WorkforceBudget,
    /// EMA per resource of how long material gather has been stagnating for
    /// this faction. Stage 3 reads this in `generate_candidates` to avoid
    /// picking blueprints that need a chronically-deficient input. Range
    /// 0..=255. Phase 2-residual: keyed on `ResourceId`; legacy callers go
    /// through `material_deficit_ema_of(good)`.
    pub material_deficit_ema: ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u8>,
    /// Anticipatory stockpile reserves: target storage levels per resource
    /// that the chief asks workers to maintain even before any blueprint
    /// demands them. Computed each chief tick from member count, culture
    /// traits, and tech foresight. Consumed by `chief_job_posting_system` to
    /// size Stockpile postings, and by `goal_update_system` to pick a
    /// fallback gather goal for unclaimed workers. Range 0..=u32::MAX.
    /// Phase 2-residual: keyed on `ResourceId`; legacy callers go through
    /// `material_target_of(good)`.
    pub material_targets: ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
    /// Active hunting directive (`Hunt` or `Scout`) issued by the chief.
    /// Refreshed by `chief_hunt_order_system` once per game-day, with a
    /// mid-day invalidation sweep that clears spent / empty targets.
    pub hunt_order: Option<HuntOrder>,
    /// Count of living Wolf+Deer entities scanned within `HUNT_SCAN_RADIUS`
    /// of `home_tile` on the most recent chief decision. Drives
    /// `faction_hunter_assignment_system`'s density scaling so factions in
    /// game-rich areas grow more hunters above the 20% floor.
    pub nearby_prey_count: u32,
    /// Faction-level wealth pool. Distinct from per-settlement
    /// treasuries (which fund settlement bureaucrats). Pluralist
    /// Economy R2: defaults to 0; later phases credit/debit during
    /// tribute (R11), public-works funding (R5+), and inter-faction
    /// transfers.
    pub treasury: f32,
    /// Construction-procurement plan, rebuilt every chief-posting cadence by
    /// `classify_construction_procurement_system`. Maps a construction input
    /// `ResourceId` to the `HaulSource` Phase 3c should stamp on its Haul
    /// posting: `Market { max_unit_price }` when the resource is scarce-but-
    /// affordably-procurable (absent / `Storage` = legacy withdraw-from-storage).
    pub procurement_plan: ahash::AHashMap<
        crate::economy::resource_catalog::ResourceId,
        crate::simulation::jobs::HaulSource,
    >,
    /// Resolved economic node for procurement: `(node_entity, market_tile)`,
    /// refreshed alongside `procurement_plan` by
    /// `classify_construction_procurement_system`. The Market-haul dispatcher
    /// reads this to route a worker to the market without needing
    /// `SettlementMap`/`CampMap` params (16-param ceiling). `None` when the
    /// faction has no settlement/camp node.
    pub procurement_market: Option<(Entity, (i32, i32))>,
    /// Full per-input scarcity snapshot, refreshed alongside
    /// `procurement_plan` by `classify_construction_procurement_system`.
    /// `generate_candidates` reads it (runtime only) so the era-aware
    /// `select_wall_material` can return `EmergencyShelter` and emit
    /// era-appropriate emergency bedding when every wall rung is
    /// unobtainable. Empty in seed mode (selector passed `None`).
    pub material_view: crate::simulation::construction::MaterialAvailabilityView,
    /// Per-resource economic policy. Pluralist Economy R4: each entry
    /// is `ResourceId → ResourceControlPolicy` (composable flags
    /// describing whether the chief allocates labor, private actors
    /// are allowed, the state sells at market, etc.). Resources
    /// **not** in the map fall through to
    /// `ResourceControlPolicy::default()` — the all-communist preset
    /// matching today's behaviour. So an empty map is observationally
    /// identical to a pre-R4 faction.
    pub economic_policy: ahash::AHashMap<
        crate::economy::resource_catalog::ResourceId,
        crate::economy::policy::ResourceControlPolicy,
    >,
    /// Faction-level land tenure governance — distinct from the
    /// per-resource `economic_policy` map because land is positional
    /// rather than commoditised. Default (all flags false) preserves
    /// the all-`StateOwned` behaviour; `land_policy_for(preset)` flips
    /// the right combination for Mixed / Market.
    pub land_policy: crate::economy::policy::LandPolicy,
    /// Pluralist Economy R3: parent faction id, set on household
    /// sub-factions. `None` = top-level (village). Households nest
    /// under villages and reuse the entire faction infrastructure.
    pub parent_faction: Option<u32>,
    /// R3: head of the household sub-faction (analogous to `chief_entity`
    /// for villages). `None` for villages or sub-factions whose head
    /// has died / not yet been appointed.
    pub household_head: Option<Entity>,
    /// R3: reverse pointer for villages — every household whose
    /// `parent_faction == Some(self_id)`. Maintained alongside
    /// `parent_faction` by `FactionRegistry::spawn_household`.
    pub children_factions: Vec<u32>,
    /// Pluralist Economy R5: when true, the chief appoints
    /// bureaucrats and the settlement treasury pays their wage.
    /// Faction-wide governance flag (not per-resource). Defaults to
    /// false — today's communist factions don't have bureaucrats.
    pub state_funds_public_works: bool,
    /// R5: how many ticks the faction's settlement treasury has been
    /// empty (i.e. unable to pay full salaries). When this crosses
    /// `BUREAUCRAT_QUIT_DAYS * TICKS_PER_DAY`, all bureaucrats
    /// demote. Reset to 0 whenever a salary tick succeeds.
    pub bureaucrat_treasury_empty_streak: u32,
    /// Pluralist Economy R11: faction-relationship primitive for
    /// tribute. Every entry is the id of a subordinate faction that
    /// pays tribute to this faction. Maintained in tandem with the
    /// subordinate's `subordinate_to`.
    pub dominance_over: Vec<u32>,
    /// R11: id of this faction's overlord (who receives tribute), or
    /// None if independent. The relationship is one-to-many:
    /// dominant→subordinates, but each subordinate has exactly one
    /// overlord.
    pub subordinate_to: Option<u32>,
    /// Faction archetype — Settled (default) or Nomadic. Set at faction
    /// creation; for nomads `home_tile` is mutable (current camp anchor),
    /// no `Settlement` is auto-founded, no plots are carved, and storage
    /// pools across member/pack-animal/PackBundle inventories instead of
    /// using a `FactionStorageTile`. Households inherit from their parent
    /// village via `spawn_household`.
    pub lifestyle: Lifestyle,
    /// Tick of the most recent migration commit (Phase 8). Zero on a band
    /// that has never moved. `nomad_migration_system` reads this against
    /// `TICKS_PER_SEASON` to enforce a minimum stay per camp.
    pub last_migration_tick: u32,
    /// Phase 8: target tile for an in-flight migration. `nomad_migration_system`
    /// (trigger) writes this when the band decides to move. The trailing
    /// `nomad_migration_commit_system` reads it, tears down the old camp's
    /// deployable structures, then mutates `home_tile = target` and clears
    /// this field. `None` outside of an active migration.
    pub pending_migration: Option<(i32, i32)>,
    /// P3 (smarter targeting): ring buffer of the band's last
    /// `RECENT_CAMP_RING_CAP` camp tiles + the tick they were vacated.
    /// `pick_migration_target` reads this for the recency penalty so a
    /// band doesn't oscillate between two known-good clusters when both
    /// dry up. Pushed when an AI caravan starts pitching at the final
    /// camp or when the player pitches manually.
    pub recent_camps: std::collections::VecDeque<((i32, i32), u32)>,
    /// Camp lifecycle state: shelters Pitched at `home_tile`, or fully
    /// Packed (mobile band). Settled factions remain `Pitched`. AI
    /// nomadic factions progress through pack/travel/pitch phases in
    /// `nomad_migration_commit_system`; player-flow nomadic factions
    /// transition via `PlayerCommand::PackCamp` / `PitchCamp`.
    pub camp_state: CampState,
    /// Unified AI migration state machine (Surveying → PendingCommit →
    /// PackingCamp → Traveling → PitchingCamp → Idle). Player-driven
    /// nomads stay in `Idle` and use explicit Pack/Pitch commands. The
    /// legacy `pending_migration` field mirrors the final target while
    /// an AI migration is active.
    pub migration_phase: MigrationPhase,
    /// Phase 1: when true, the AI Surveying / commit pipeline drives
    /// the band autonomously. AI nomadic factions default to true;
    /// player-controlled nomadic factions are flipped to false by
    /// `spawn_population` so the band only moves on explicit player
    /// command.
    pub nomad_autopilot: bool,
    /// Phase 3: player-facing scoring bias for the next
    /// `pick_migration_target` evaluation. AI defaults to `FreeRoute`.
    pub migration_intent: MigrationIntent,
    /// Player-locked autonomy mode for packed nomads. Reset to
    /// `Hold` on every `PackCamp`. Only consulted when
    /// `nomad_autopilot == false` (player nomads); AI nomads ignore.
    pub packed_autonomy: PackedMigrationAutonomy,
    /// Phase 2: scouted / surveyed camp-site candidates the player or
    /// chief can pick from. Ring with cap `MAX_CANDIDATE_SITES`.
    pub candidate_sites: std::collections::VecDeque<CampSiteCandidate>,
    /// Phase 4: cargo manifest for an in-flight migration. `Default`
    /// (`is_empty()`) outside of Pack→Pitch.
    pub cargo_manifest: CampCargoManifest,
    /// Tick of the most recent `camp_state` or `migration_phase`
    /// transition. Used by HUD age display + telemetry.
    pub last_phase_change_tick: u32,
    /// P4 (reverse sedentarization): per-day streak counter of "this
    /// settlement is failing" samples. `sedentary_collapse_system`
    /// increments daily when a trigger fires (food deficit / shelter
    /// loss / population crash) and zeros it when the band recovers.
    /// At `COLLAPSE_TRIGGER_TICKS` worth of consecutive samples the
    /// faction emits `SwitchArchetype { nomadic_X }` and reverts.
    pub collapse_streak: u32,
    /// Per-faction capability bundle (P1a). Computed at faction
    /// creation by `derive_from_legacy(...)` once a `(lifestyle,
    /// preset)` pair is known; mirrors the legacy fields above
    /// observationally. Every site that previously branched on
    /// `is_nomadic()` or `match preset` should consult this instead.
    pub caps: crate::simulation::archetype::FactionCapabilities,
    /// Phase 3 (wage-aware-labor-market-v2): per-`(JobKind, target_rid)`
    /// exponential moving average of recent payouts on this faction's
    /// postings. Folded daily by `faction_wage_signal_system` from
    /// member-side `Earnings` rings. Phase 4's EV-driven profession
    /// choice reads this to score `expected_wage(profession)`.
    /// `target_rid = None` covers Build / Calories postings.
    pub wage_signal: ahash::AHashMap<
        (
            crate::simulation::jobs::JobKind,
            Option<crate::economy::resource_catalog::ResourceId>,
        ),
        crate::simulation::jobs::WageEMA,
    >,
}

impl FactionData {
    /// Can the faction *build with* `tech`? Reads `buildable_techs` — the
    /// poster-pool union of resident chief + architect Learned. There is
    /// no longer a community-*adoption* gate anywhere in the construction
    /// path; this is the one consistent surface. Chief-Aware for planning
    /// authority lives on `self.techs.has(tech)`; per-person execution on
    /// `PersonKnowledge::has_learned`.
    #[inline]
    pub fn community_has(&self, tech: crate::simulation::technology::TechId) -> bool {
        self.buildable_techs.has(tech)
    }

    /// Look up the policy for `resource_id`, falling back to the
    /// all-communist default for unmapped resources. Hot-path call
    /// site; never allocates.
    pub fn policy_for(
        &self,
        resource_id: crate::economy::resource_catalog::ResourceId,
    ) -> crate::economy::policy::ResourceControlPolicy {
        self.economic_policy
            .get(&resource_id)
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Resource, Default)]
pub struct FactionRegistry {
    pub factions: AHashMap<u32, FactionData>,
    pub next_id: u32,
}

#[derive(Resource, Default)]
pub struct PlayerFaction {
    pub faction_id: u32,
}

impl FactionRegistry {
    pub fn create_faction(&mut self, home_tile: (i32, i32)) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        // FactionTechs starts empty; `sync_faction_techs_from_chief_system`
        // populates it from the chief's PersonKnowledge.aware bitset every
        // Economy tick. The founding chief already carries Paleolithic
        // awareness from `PersonKnowledge::paleolithic_seed` at spawn.
        let techs = FactionTechs::default();
        // Deterministic per-faction seed: home tile + faction id, packed.
        let seed = ((home_tile.0 as i32 as u32) << 16)
            ^ (home_tile.1 as i32 as u32)
            ^ id.wrapping_mul(0x9E37_79B9);
        let culture = FactionCulture::roll(seed);
        let lineage = FactionLineage::from_seed(seed);
        self.factions.insert(
            id,
            FactionData {
                storage: FactionStorage::default(),
                home_tile,
                member_count: 0,
                raid_target: None,
                under_raid: false,
                techs,
                buildable_techs: FactionTechs::default(),
                tech_adoption: [crate::simulation::technology_adoption::AdoptionStage::Unknown;
                    crate::simulation::technology::TECH_COUNT],
                stage_changed_at_tick: [0; crate::simulation::technology::TECH_COUNT],
                recent_tech_use: ahash::AHashMap::default(),
                activity_log: ActivityLog::default(),
                resource_supply: ahash::AHashMap::default(),
                resource_demand: ahash::AHashMap::default(),
                chief_entity: None,
                culture,
                lineage,
                active_upgrade: None,
                workforce_budget: crate::simulation::projects::WorkforceBudget::default(),
                material_deficit_ema: ahash::AHashMap::default(),
                material_targets: ahash::AHashMap::default(),
                hunt_order: None,
                nearby_prey_count: 0,
                treasury: 0.0,
                procurement_plan: ahash::AHashMap::default(),
                procurement_market: None,
                material_view: crate::simulation::construction::MaterialAvailabilityView::default(),
                economic_policy: ahash::AHashMap::default(),
                land_policy: crate::economy::policy::LandPolicy::default(),
                parent_faction: None,
                household_head: None,
                children_factions: Vec::new(),
                state_funds_public_works: false,
                bureaucrat_treasury_empty_streak: 0,
                dominance_over: Vec::new(),
                subordinate_to: None,
                lifestyle: Lifestyle::default(),
                last_migration_tick: 0,
                pending_migration: None,
                recent_camps: std::collections::VecDeque::new(),
                camp_state: CampState::default(),
                migration_phase: MigrationPhase::default(),
                nomad_autopilot: true,
                migration_intent: MigrationIntent::default(),
                packed_autonomy: PackedMigrationAutonomy::default(),
                candidate_sites: std::collections::VecDeque::new(),
                cargo_manifest: CampCargoManifest::default(),
                last_phase_change_tick: 0,
                collapse_streak: 0,
                // P1a: default = settled-Subsistence capabilities.
                // `spawn_population` overwrites this with the
                // archetype derived from the faction's
                // `(lifestyle, preset)` pair as soon as it knows
                // both. Other create_faction callers (tests,
                // bonding-formed factions) keep the default until
                // they apply their own preset/lifestyle.
                caps: crate::simulation::archetype::FactionCapabilities::default(),
                wage_signal: ahash::AHashMap::default(),
            },
        );
        id
    }

    /// Pluralist Economy R11: configure `subordinate` as paying
    /// tribute to `dominant`. Maintains both ends of the
    /// relationship (overlord's `dominance_over` list + subordinate's
    /// `subordinate_to` slot). Idempotent on re-call.
    pub fn set_dominance(&mut self, dominant: u32, subordinate: u32) {
        if dominant == subordinate {
            return;
        }
        if let Some(d) = self.factions.get_mut(&dominant) {
            if !d.dominance_over.contains(&subordinate) {
                d.dominance_over.push(subordinate);
            }
        }
        if let Some(s) = self.factions.get_mut(&subordinate) {
            s.subordinate_to = Some(dominant);
        }
    }

    /// Pluralist Economy R3: spawn a household sub-faction nested
    /// under `parent_faction_id`. Returns the new sub-faction id.
    ///
    /// The sub-faction reuses every faction primitive (storage,
    /// treasury, member count, chief-equivalent via `household_head`).
    /// Its `economic_policy` is stamped with `ResourceControlPolicy::
    /// capitalist()` for every catalog resource so households default
    /// to private-actor-friendly behaviour even when the parent
    /// village runs all-communist defaults. The parent faction's
    /// `children_factions` vector is updated reciprocally.
    ///
    /// Caller is responsible for:
    /// - moving the head + initial members' `FactionMember.faction_id`
    ///   to the new id,
    /// - calling `add_member` for each migrated member,
    /// - spawning a `FactionStorageTile` for the household at its
    ///   chosen home tile.
    ///
    /// R3 ships only this primitive; the actual *trigger* (cosleep
    /// duration, marriage rite, player command) lands in a follow-on
    /// sub-PR once a system needs households to exist.
    pub fn spawn_household(
        &mut self,
        parent_faction_id: u32,
        home_tile: (i32, i32),
        head: Entity,
        catalog: &crate::economy::resource_catalog::ResourceCatalog,
    ) -> u32 {
        // Inherit the parent village's policy stance: a default-communist
        // village (empty economic_policy map) spawns households that are
        // structurally distinct (storage / treasury / household_head) but
        // remain communist — they don't auto-post Tools contracts. Villages
        // that have flipped any resource toward capitalism stamp the full
        // capitalist preset on the household so private behaviour is
        // observationally consistent across the catalog.
        let parent_is_capitalist = self
            .factions
            .get(&parent_faction_id)
            .map(|p| !p.economic_policy.is_empty())
            .unwrap_or(false);
        let parent_land_policy = self
            .factions
            .get(&parent_faction_id)
            .map(|p| p.land_policy)
            .unwrap_or_default();
        let parent_lifestyle = self
            .factions
            .get(&parent_faction_id)
            .map(|p| p.lifestyle)
            .unwrap_or_default();
        let parent_caps = self
            .factions
            .get(&parent_faction_id)
            .map(|p| p.caps.clone())
            .unwrap_or_default();
        let new_id = self.create_faction(home_tile);
        if let Some(data) = self.factions.get_mut(&new_id) {
            data.parent_faction = Some(parent_faction_id);
            data.household_head = Some(head);
            // Inherit the parent village's land policy so household
            // demand systems (Phase 4+) can read "the state rents
            // land" consistently from either tier.
            data.land_policy = parent_land_policy;
            // Inherit the parent village's lifestyle — a nomadic village's
            // households are also nomadic (no plots, no FactionStorageTile,
            // travel with the band on migration).
            data.lifestyle = parent_lifestyle;
            if parent_is_capitalist {
                let cap_policy = crate::economy::policy::ResourceControlPolicy::capitalist();
                for (rid, _def) in catalog.iter() {
                    data.economic_policy.insert(rid, cap_policy);
                }
            }
            // P1a: caps inherits parent, but the divergent
            // legacy-driven fields (lifestyle, land_policy,
            // economic_policy) are mirrored from the legacy fields
            // we just populated above so the bundle stays in
            // lock-step with observable behaviour. Mixed parents
            // produce capitalist households per the legacy branch,
            // so we clone `data.economic_policy` rather than
            // `parent_caps.economic_policy`.
            data.caps = parent_caps;
            data.caps.economic_policy = data.economic_policy.clone();
            data.caps.land.policy = data.land_policy;
        }
        if let Some(parent) = self.factions.get_mut(&parent_faction_id) {
            parent.children_factions.push(new_id);
        }
        new_id
    }

    /// Pluralist Economy R3: walk the parent chain and return the
    /// top-level (root) village faction id. Returns `faction_id`
    /// itself if it has no parent.
    pub fn root_faction(&self, mut faction_id: u32) -> u32 {
        while let Some(data) = self.factions.get(&faction_id) {
            match data.parent_faction {
                Some(parent) => faction_id = parent,
                None => return faction_id,
            }
        }
        faction_id
    }

    pub fn add_member(&mut self, faction_id: u32) {
        if let Some(f) = self.factions.get_mut(&faction_id) {
            f.member_count += 1;
        }
    }

    pub fn remove_member(&mut self, faction_id: u32) {
        if let Some(f) = self.factions.get_mut(&faction_id) {
            f.member_count = f.member_count.saturating_sub(1);
        }
    }
}

// ── Bonding system ────────────────────────────────────────────────────────────

pub fn bonding_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    mut registry: ResMut<FactionRegistry>,
    options: Res<crate::game_state::GameStartOptions>,
    catalog: Res<crate::economy::resource_catalog::ResourceCatalog>,
    archetype_registry: Res<crate::simulation::archetype::FactionArchetypeRegistry>,
    mut query: Query<(Entity, &mut FactionMember, &Personality, &Transform)>,
    mut rel_query: Query<Option<&mut RelationshipMemory>>,
) {
    // Collect solo agents so we can iterate without borrow conflicts
    let solo_agents: Vec<(Entity, (i32, i32))> = query
        .iter()
        .filter_map(|(e, fm, _, transform)| {
            if fm.faction_id == SOLO {
                let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                Some((e, (tx, ty)))
            } else {
                None
            }
        })
        .collect();

    for (entity, (tx, ty)) in &solo_agents {
        // Find any adjacent entity
        let mut found_neighbor: Option<(Entity, u32)> = None;
        'outer: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                for &nb_entity in spatial.get(tx + dx, ty + dy) {
                    if nb_entity == *entity {
                        continue;
                    }
                    // Get neighbor's faction_id
                    if let Ok((_, nb_fm, _, _)) = query.get(nb_entity) {
                        found_neighbor = Some((nb_entity, nb_fm.faction_id));
                        break 'outer;
                    }
                }
            }
        }

        let Some((nb_entity, nb_faction)) = found_neighbor else {
            continue;
        };

        // Use get_many_mut to safely borrow both entities at once
        let Ok([(_, mut fm, personality, transform), (_, mut nb_fm, _, _)]) =
            query.get_many_mut([*entity, nb_entity])
        else {
            continue;
        };

        // Reset bond timer if target changed
        if fm.bond_target != Some(nb_entity) {
            fm.bond_target = Some(nb_entity);
            fm.bond_timer = 0;
        }

        let threshold = if *personality == Personality::Socialite {
            BOND_THRESHOLD.saturating_sub(60)
        } else {
            BOND_THRESHOLD
        };

        fm.bond_timer = fm.bond_timer.saturating_add(1);

        if fm.bond_timer >= threshold {
            fm.bond_timer = 0;
            fm.bond_target = None;

            let pos = transform.translation.truncate();
            let home_tx = (pos.x / TILE_SIZE).floor() as i32;
            let home_ty = (pos.y / TILE_SIZE).floor() as i32;

            let faction_id = if nb_faction == SOLO {
                let new_id = registry.create_faction((home_tx, home_ty));
                // Apply the world's economy preset so bonding-spawned
                // factions don't silently fall back to all-communist
                // defaults — without this, every new faction's chief
                // would post Stockpile/Farm/Craft regardless of the
                // player's `EconomyPreset` selection.
                if let Some(fd) = registry.factions.get_mut(&new_id) {
                    crate::economy::policy::apply_preset(
                        &mut fd.economic_policy,
                        options.economy,
                        &catalog,
                    );
                    // P5: route through the registry. Bonding-spawned
                    // factions inherit the world's preset and stay
                    // Settled (lifestyle defaults to Settled at
                    // create_faction). Legacy fallback covers any
                    // unauthored key.
                    let key = crate::simulation::archetype::legacy_archetype_key(
                        fd.lifestyle,
                        options.economy,
                    );
                    fd.caps = crate::simulation::archetype::derive_from_archetype_key(
                        &archetype_registry,
                        key,
                        Some((fd.lifestyle, options.economy, &catalog)),
                    )
                    .expect("derive_from_archetype_key with legacy fallback always returns Some");
                }
                nb_fm.faction_id = new_id;
                nb_fm.bond_timer = 0;
                nb_fm.bond_target = None;
                registry.add_member(new_id); // for the neighbor
                                             // Spawn a storage tile at the new faction's home position
                let world_pos = tile_to_world(home_tx as i32, home_ty as i32);
                commands.spawn((
                    FactionStorageTile { faction_id: new_id },
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
                    GlobalTransform::default(),
                    Visibility::Hidden,
                    InheritedVisibility::default(),
                ));
                // The initiating agent (outer-loop entity) becomes the founding chief.
                if let Some(fd) = registry.factions.get_mut(&new_id) {
                    fd.chief_entity = Some(*entity);
                }
                commands.entity(*entity).insert(FactionChief);
                new_id
            } else {
                nb_faction
            };

            fm.faction_id = faction_id;
            registry.add_member(faction_id);

            // Bonding builds affinity between the two agents
            if let Ok([rel1, rel2]) = rel_query.get_many_mut([*entity, nb_entity]) {
                if let Some(mut r) = rel1 {
                    r.update(nb_entity, 30);
                }
                if let Some(mut r) = rel2 {
                    r.update(*entity, 30);
                }
            }
        }
    }
}

// ── Social fill system ────────────────────────────────────────────────────────

pub fn social_fill_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut discovery_events: EventWriter<crate::simulation::knowledge::DiscoveryActionEvent>,
    mut query: Query<(
        Entity,
        &mut Needs,
        &FactionMember,
        &Transform,
        &BucketSlot,
        &LodLevel,
        &AgentGoal,
        Option<&SecondarySocial>,
    )>,
    member_q: Query<&FactionMember, With<Person>>,
) {
    // Tightened (was: relief from ANY nearby indexed entity, ungated by
    // goal). Social relief now requires the agent itself to be socially
    // active — dedicated `AgentGoal::Socialize` OR a live ambient
    // work-pairing (`SecondarySocial`) — and counts only same-root-faction
    // `Person` neighbors. No more spurious relief from standing near
    // animals / enemies / strangers / items, nor relief to socially-inactive
    // lone workers. The ambient path is what keeps work-paired coworkers
    // satisfied so `SocialScorer` doesn't fire a work-abandoning detour.
    let now = clock.tick as u32;
    for (entity, mut needs, member, transform, slot, lod, goal, sec) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        if !is_social_contact(*goal, *lod, sec, now) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let self_root = registry.root_faction(member.faction_id);

        let mut nearby = 0u8;
        for dy in -SOCIAL_RADIUS..=SOCIAL_RADIUS {
            for dx in -SOCIAL_RADIUS..=SOCIAL_RADIUS {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    // Same-root-faction Person only.
                    if let Ok(om) = member_q.get(other) {
                        if registry.root_faction(om.faction_id) == self_root {
                            nearby = nearby.saturating_add(1);
                        }
                    }
                }
            }
        }

        if nearby > 0 {
            needs.social = (needs.social - (nearby.min(10) * 3) as f32).max(0.0);
            if let Some(fd) = registry.factions.get_mut(&member.faction_id) {
                fd.activity_log.increment(ActivityKind::Socializing);
            }
            discovery_events.send(crate::simulation::knowledge::DiscoveryActionEvent {
                actor: entity,
                activity: ActivityKind::Socializing,
            });
        }
    }
}

// ── Drop items at destination system ─────────────────────────────────────────

pub fn drop_items_at_destination_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    registry: Res<FactionRegistry>,
    mut board: ResMut<JobBoard>,
    mut job_completed: EventWriter<JobCompletedEvent>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &FactionMember,
        &Profession,
        &LodLevel,
        Option<&JobClaim>,
    )>,
) {
    for (worker, mut ai, mut aq, mut agent, mut carrier, member, profession, lod, claim_opt) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Working
            || aq.current_task_kind() != TaskKind::DepositResource as u16
        {
            continue;
        }

        let deposit_tx = ai.dest_tile.0 as i32;
        let deposit_ty = ai.dest_tile.1 as i32;

        // First: dump everything in hands. Hauling loads (Wood, Stone, Iron, ...) are
        // exactly what storage wants; food/tools that ended up in hands also go here.
        let wood_id = crate::economy::core_ids::wood();
        let stone_id = crate::economy::core_ids::stone();
        let mut hand_wood: u32 = 0;
        let mut hand_stone: u32 = 0;
        for stack in carrier.drop_all() {
            let rid = stack.item.resource_id;
            if rid == wood_id {
                hand_wood = hand_wood.saturating_add(stack.qty);
            } else if rid == stone_id {
                hand_stone = hand_stone.saturating_add(stack.qty);
            }
            spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                deposit_tx,
                deposit_ty,
                rid,
                stack.qty,
            );
        }
        // Credit any Material Gather posting this worker holds for the
        // dropped wood/stone.
        if let Some(claim) = claim_opt {
            if hand_wood > 0 {
                record_progress_filtered(
                    &mut commands,
                    &mut board,
                    &mut job_completed,
                    claim,
                    JobKind::Stockpile,
                    Some(wood_id),
                    hand_wood,
                );
            }
            if hand_stone > 0 {
                record_progress_filtered(
                    &mut commands,
                    &mut board,
                    &mut job_completed,
                    claim,
                    JobKind::Stockpile,
                    Some(stone_id),
                    hand_stone,
                );
            }
        }

        let food_qty = agent.total_food();
        if food_qty > CAMP_KEEP {
            let mut deposit = food_qty - CAMP_KEEP;
            let mut drops: Vec<(crate::economy::resource_catalog::ResourceId, u32)> = Vec::new();
            for (it, q) in agent.inventory.iter_mut() {
                if it.resource_id.is_edible() && *q > 0 {
                    let to_remove = (*q).min(deposit);
                    *q -= to_remove;
                    deposit -= to_remove;
                    drops.push((it.resource_id, to_remove));
                }
                if deposit == 0 {
                    break;
                }
            }
            // Sum calories of food deposited at faction storage so a Gather
            // job posting (if this worker holds one) can be credited.
            let mut deposited_calories: u32 = 0;
            for (rid, qty) in drops {
                spawn_or_merge_ground_item(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    deposit_tx,
                    deposit_ty,
                    rid,
                    qty,
                );
                deposited_calories =
                    deposited_calories.saturating_add(qty * rid.nutrition() as u32);
            }
            if deposited_calories > 0 {
                if let Some(claim) = claim_opt {
                    record_progress(
                        &mut commands,
                        &mut board,
                        &mut job_completed,
                        claim,
                        JobKind::Stockpile,
                        deposited_calories,
                    );
                }
            }
        }
        let _ = worker; // silence unused if no further use

        // Deposit any seeds the agent is carrying in inventory. Hand seeds
        // were already dumped via `carrier.drop_all()` above. Iterating
        // `PlantKind::ALL` keeps this loop in sync with the seed↔plant table
        // — adding a new seed only needs an arm in `PlantKind::seed_good()`.
        // Farmers deposit too: PlantFromStorage withdraws seeds back as
        // needed, which keeps the `SI_STORAGE_*_SEED` state slots meaningful.
        let has_cultivation = registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.techs.has(CROP_CULTIVATION))
            .unwrap_or(false);
        if has_cultivation {
            for seed_id in PlantKind::ALL.iter().filter_map(|k| k.seed_resource()) {
                let mut qty: u32 = 0;
                for (it, q) in agent.inventory.iter_mut() {
                    if it.resource_id == seed_id && *q > 0 {
                        qty += *q;
                        *q = 0;
                    }
                }
                if qty > 0 {
                    spawn_or_merge_ground_item(
                        &mut commands,
                        &spatial,
                        &mut ground_items,
                        deposit_tx,
                        deposit_ty,
                        seed_id,
                        qty,
                    );
                }
            }
        }

        // Deposit all crafted goods (Tools, Weapon, Armor, Shield, Cloth, Luxury).
        // Preserve the full `Item` (material + quality + stats) through storage
        // so an equipped Iron Spear keeps its damage_bonus after a withdraw.
        let mut crafted_drops: Vec<(crate::economy::item::Item, u32)> = Vec::new();
        for (it, q) in agent.inventory.iter_mut() {
            if *q > 0
                && matches!(
                    it.resource_id.class(),
                    Some(
                        crate::economy::resource_catalog::ResourceClass::Tool
                            | crate::economy::resource_catalog::ResourceClass::Weapon
                            | crate::economy::resource_catalog::ResourceClass::Armor
                            | crate::economy::resource_catalog::ResourceClass::Shield
                            | crate::economy::resource_catalog::ResourceClass::Cloth
                            | crate::economy::resource_catalog::ResourceClass::Luxury
                    )
                )
            {
                crafted_drops.push((*it, *q));
                *q = 0;
            }
        }
        for (item, qty) in crafted_drops {
            crate::simulation::items::spawn_or_merge_ground_item_full(
                &mut commands,
                &spatial,
                &mut ground_items,
                deposit_tx,
                deposit_ty,
                item,
                qty,
            );
        }

        // Deposit recovered construction materials (Wood from deconstruction).
        let wood_id = crate::economy::core_ids::wood();
        let wood_qty = agent.quantity_of_resource(wood_id);
        if wood_qty > 0 {
            agent.remove_resource(wood_id, wood_qty);
            spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                deposit_tx,
                deposit_ty,
                wood_id,
                wood_qty,
            );
        }

        ai.state = AiState::Idle;
        // Phase 5c-ii-c-ii: clear the typed `Task::DepositToFactionStorage`
        // (or any pending DepositResource variant) so the next tick's HTN
        // dispatcher sees a clean Idle slot. `advance()` promotes any further
        // queued task — today the gather chain ends here so the queue is
        // empty and `current` flips to `Task::Idle`.
        aq.advance();
    }
}

// ── Storage tile map maintenance ──────────────────────────────────────────────

pub fn update_storage_tile_map_system(
    mut map: ResMut<StorageTileMap>,
    mut hotspots: ResMut<HotspotFlowFields>,
    chunk_map: Res<ChunkMap>,
    changed_q: Query<(), Or<(Added<FactionStorageTile>, Changed<Transform>)>>,
    removed: RemovedComponents<FactionStorageTile>,
    all_q: Query<(&FactionStorageTile, &Transform)>,
) {
    if changed_q.is_empty() && removed.is_empty() {
        return;
    }
    // Snapshot the previous tile set so we can diff hotspot registrations.
    let prev: ahash::AHashSet<(i32, i32)> = map.tiles.keys().copied().collect();

    map.tiles.clear();
    map.by_faction.clear();
    for (tile, transform) in all_q.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        map.tiles.insert((tx, ty), tile.faction_id);
        map.by_faction
            .entry(tile.faction_id)
            .or_default()
            .push((tx, ty));
    }

    // Diff: register newly-added storage tiles, unregister removed ones.
    for &(tx, ty) in map.tiles.keys() {
        if !prev.contains(&(tx, ty)) {
            let z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
            hotspots.register((tx, ty, z), HotspotKind::Storage);
        }
    }
    for (tx, ty) in prev {
        if !map.tiles.contains_key(&(tx, ty)) {
            // We don't know the original Z; unregister at every plausible Z
            // by brute-force unregister at surface_z (the only Z storage
            // tiles get registered with above).
            let z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
            hotspots.unregister((tx, ty, z), HotspotKind::Storage);
        }
    }
}

// ── Faction-center hotspot sync ───────────────────────────────────────────────

/// Maintains `HotspotFlowFields` registrations for `FactionCenter` entities.
/// Each tribe's center is a high-traffic destination — caching a flow field
/// for it lets the worker skip per-agent A* on the final leg of a route.
pub fn sync_faction_center_hotspots_system(
    mut hotspots: ResMut<HotspotFlowFields>,
    chunk_map: Res<ChunkMap>,
    mut last_seen: Local<ahash::AHashMap<Entity, (i32, i32, i8)>>,
    mut removed: RemovedComponents<FactionCenter>,
    centers: Query<(Entity, &Transform), With<FactionCenter>>,
) {
    let mut current: ahash::AHashMap<Entity, (i32, i32, i8)> = ahash::AHashMap::new();
    for (entity, transform) in centers.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
        current.insert(entity, (tx, ty, z));
    }

    // Unregister centers that were destroyed entirely.
    for entity in removed.read() {
        if let Some(prev) = last_seen.remove(&entity) {
            hotspots.unregister(prev, HotspotKind::FactionCenter);
        }
    }

    // Register newly-spawned centers; re-register if a center moved.
    for (entity, &tile) in current.iter() {
        match last_seen.get(entity) {
            Some(&prev) if prev == tile => {}
            Some(&prev) => {
                hotspots.unregister(prev, HotspotKind::FactionCenter);
                hotspots.register(tile, HotspotKind::FactionCenter);
            }
            None => {
                hotspots.register(tile, HotspotKind::FactionCenter);
            }
        }
    }

    *last_seen = current;
}

// ── Faction storage totals computation ───────────────────────────────────────

/// Build `FactionStorage.totals` for every faction. Backend-aware
/// (P2a): each pass gates on `FactionData.caps.storage`:
///
/// - **`FactionTile` / `Hybrid` (tile pass)**: sum `GroundItem`s
///   sitting on tiles marked by a `FactionStorageTile` belonging to
///   that faction.
/// - **`MemberPool` / `Hybrid` (member pass)**: sum
///   `EconomicAgent.inventory` for every member of the faction.
/// - `CaravanBundles` is reserved for Phase 6+ (pack-animal `PackBundle`s).
///
/// Both passes write to the same `faction.storage.totals` map. Chief
/// posting math (`faction.storage.stock_of(rid)`) and HTN food gates
/// stay archetype-agnostic. The underlying iteration is still
/// `O(ground_items + members)` rather than `O(factions × ground_items)`
/// — it's a tight inner loop with a per-target faction lookup, not a
/// per-faction call into `storage_backend::rollup_for_kind` (that
/// helper is the off-system / test verifier).
pub fn compute_faction_storage_system(
    storage_tile_map: Res<StorageTileMap>,
    ground_items: Query<(&GroundItem, &Transform)>,
    members: Query<
        (&FactionMember, &crate::economy::agent::EconomicAgent),
        With<crate::simulation::person::Person>,
    >,
    pack_animals: Query<(
        &crate::simulation::animals::Tamed,
        &crate::simulation::animals::PackAnimalInventory,
    )>,
    mut registry: ResMut<FactionRegistry>,
) {
    use crate::simulation::archetype::StorageBackendKind;

    for faction in registry.factions.values_mut() {
        faction.storage.totals.clear();
    }

    // Tile pass: only `FactionTile` / `Hybrid` backends contribute.
    // Nomadic (`MemberPool`) factions never spawn storage tiles in the
    // first place, so the implicit lookup miss already excluded them
    // pre-P1a; the explicit gate makes the contract self-documenting
    // and survives a hypothetical Hybrid-without-tiles configuration.
    for (gi, transform) in ground_items.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let Some(&faction_id) = storage_tile_map.tiles.get(&(tx, ty)) else {
            continue;
        };
        let Some(faction) = registry.factions.get_mut(&faction_id) else {
            continue;
        };
        if !matches!(
            faction.caps.storage,
            StorageBackendKind::FactionTile | StorageBackendKind::Hybrid
        ) {
            continue;
        }
        *faction
            .storage
            .totals
            .entry(gi.item.resource_id)
            .or_insert(0) += gi.qty;
    }

    // Member pass: only `MemberPool` / `Hybrid` backends contribute.
    // FactionTile factions stay tile-only (regression invariant for the
    // 411-test baseline).
    for (member, agent) in members.iter() {
        let Some(faction) = registry.factions.get_mut(&member.faction_id) else {
            continue;
        };
        if !matches!(
            faction.caps.storage,
            StorageBackendKind::MemberPool | StorageBackendKind::Hybrid
        ) {
            continue;
        }
        for (item, qty) in agent.inventory.iter() {
            if *qty == 0 {
                continue;
            }
            *faction.storage.totals.entry(item.resource_id).or_insert(0) += qty;
        }
    }

    // P8: pack-animal pass — only mobile-home factions whose storage is
    // member-pool-flavoured contribute. Folds carrying pack animals into
    // the band's storage rollup so chief postings + UI panels see the
    // band's full inventory regardless of who's holding it.
    for (tamed, inv) in pack_animals.iter() {
        let Some(faction) = registry.factions.get_mut(&tamed.owner_faction) else {
            continue;
        };
        if !matches!(
            faction.caps.storage,
            StorageBackendKind::MemberPool | StorageBackendKind::Hybrid
        ) {
            continue;
        }
        if !faction.caps.home.is_mobile() {
            continue;
        }
        for (rid, qty) in inv.iter() {
            *faction.storage.totals.entry(rid).or_insert(0) += qty;
        }
    }
}

// ── Helpers for task dispatch ──────────────────────────────────────────────────

impl FactionRegistry {
    pub fn home_tile(&self, faction_id: u32) -> Option<(i32, i32)> {
        self.factions.get(&faction_id).map(|f| f.home_tile)
    }

    pub fn food_stock(&self, faction_id: u32) -> f32 {
        self.factions
            .get(&faction_id)
            .map(|f| f.storage.food_total())
            .unwrap_or(0.0)
    }

    pub fn raid_target(&self, faction_id: u32) -> Option<u32> {
        self.factions.get(&faction_id).and_then(|f| f.raid_target)
    }

    pub fn is_under_raid(&self, faction_id: u32) -> bool {
        self.factions
            .get(&faction_id)
            .map(|f| f.under_raid)
            .unwrap_or(false)
    }
}

impl FactionData {
    /// Read `resource_supply` by `ResourceId`. Returns 0 for missing entries.
    /// Legacy `Good`-typed callers pass `good.into()` at the boundary.
    pub fn supply_of(&self, resource_id: crate::economy::resource_catalog::ResourceId) -> u32 {
        self.resource_supply.get(&resource_id).copied().unwrap_or(0)
    }

    /// Read `resource_demand` by `ResourceId`. See `supply_of` for migration
    /// semantics.
    pub fn demand_of(&self, resource_id: crate::economy::resource_catalog::ResourceId) -> u32 {
        self.resource_demand.get(&resource_id).copied().unwrap_or(0)
    }

    /// Read `material_targets` by `ResourceId`. Returns 0 for missing entries.
    pub fn material_target_of(
        &self,
        resource_id: crate::economy::resource_catalog::ResourceId,
    ) -> u32 {
        self.material_targets
            .get(&resource_id)
            .copied()
            .unwrap_or(0)
    }

    /// Read `material_deficit_ema` by `ResourceId`.
    pub fn material_deficit_ema_of(
        &self,
        resource_id: crate::economy::resource_catalog::ResourceId,
    ) -> u8 {
        self.material_deficit_ema
            .get(&resource_id)
            .copied()
            .unwrap_or(0)
    }

    pub fn food_yield_multiplier(&self) -> f32 {
        1.0 + (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .map(|id| tech_def(id).bonus.food_yield_bonus)
            .sum::<f32>()
    }

    pub fn wood_yield_multiplier(&self) -> f32 {
        1.0 + (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .map(|id| tech_def(id).bonus.wood_yield_bonus)
            .sum::<f32>()
    }

    pub fn stone_yield_multiplier(&self) -> f32 {
        1.0 + (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .map(|id| tech_def(id).bonus.stone_yield_bonus)
            .sum::<f32>()
    }

    pub fn combat_damage_bonus(&self) -> u8 {
        (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .fold(0u8, |acc, id| {
                acc.saturating_add(tech_def(id).bonus.combat_damage_bonus)
            })
    }
}

pub fn center_camera_on_player_faction(
    player_faction: Res<PlayerFaction>,
    registry: Res<FactionRegistry>,
    mut camera: Query<&mut Transform, With<Camera>>,
) {
    let Some(data) = registry.factions.get(&player_faction.faction_id) else {
        return;
    };
    let (htx, hty) = data.home_tile;
    let world_pos = tile_to_world(htx as i32, hty as i32);
    for mut transform in camera.iter_mut() {
        transform.translation.x = world_pos.x;
        transform.translation.y = world_pos.y;
    }
}

pub fn resource_demand_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    agent_query: Query<(&FactionMember, &EconomicAgent)>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    for faction in registry.factions.values_mut() {
        faction.resource_supply.clear();
        faction.resource_demand.clear();
    }

    // 1. Tally supply (agents' inventories + faction stocks)
    for (member, agent) in agent_query.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        if let Some(faction) = registry.factions.get_mut(&member.faction_id) {
            for (item, qty) in &agent.inventory {
                if *qty > 0 {
                    *faction.resource_supply.entry(item.resource_id).or_insert(0) += *qty;
                }
            }
        }
    }

    for faction in registry.factions.values_mut() {
        for (&id, &qty) in &faction.storage.totals {
            *faction.resource_supply.entry(id).or_insert(0) += qty;
        }
    }

    // 2. Tally demand
    // Materials from Blueprints — sum unmet need per ingredient across all deposit slots.
    for &bp_entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(bp_entity) {
            if let Some(faction) = registry.factions.get_mut(&bp.faction_id) {
                for i in 0..bp.deposit_count as usize {
                    let need = bp.deposits[i];
                    let unmet = need.needed.saturating_sub(need.deposited) as u32;
                    if unmet > 0 {
                        *faction.resource_demand.entry(need.resource_id).or_insert(0) += unmet;
                    }
                }
            }
        }
    }

    // Food demand from population size. Resolve core_ids upfront so we
    // pay one OnceLock read per attribute, not per faction.
    let fruit = crate::economy::core_ids::fruit();
    let meat = crate::economy::core_ids::meat();
    let grain = crate::economy::core_ids::grain();
    let tools = crate::economy::core_ids::tools();
    let weapon = crate::economy::core_ids::weapon();
    let cloth = crate::economy::core_ids::cloth();
    let luxury = crate::economy::core_ids::luxury();
    let shield = crate::economy::core_ids::shield();
    let armor = crate::economy::core_ids::armor();
    for faction in registry.factions.values_mut() {
        let food_demand = faction.member_count * 10;
        faction.resource_demand.insert(fruit, food_demand);
        faction.resource_demand.insert(meat, food_demand);
        faction.resource_demand.insert(grain, food_demand);

        // Crafted-good demand: scales with member count. Drives
        // `chief_job_posting_system`'s recipe selection (highest output
        // deficit wins).
        faction
            .resource_demand
            .insert(tools, faction.member_count.div_ceil(2));
        faction
            .resource_demand
            .insert(weapon, faction.member_count.div_ceil(2));
        faction
            .resource_demand
            .insert(cloth, faction.member_count.div_ceil(2));
        faction
            .resource_demand
            .insert(luxury, faction.member_count.div_ceil(3));
        faction
            .resource_demand
            .insert(shield, faction.member_count.div_ceil(4));
        faction
            .resource_demand
            .insert(armor, faction.member_count.div_ceil(4));
    }
}

// ── Material stockpile target system ──────────────────────────────────────────

/// Refresh `FactionData::material_targets` for every faction. Targets are
/// anticipatory reserves the chief asks workers to keep in storage even when
/// no blueprint currently demands them. Driven by member count, culture
/// traits, and tech foresight; runs every 60 ticks in Economy.
pub fn update_material_targets_system(clock: Res<SimClock>, mut registry: ResMut<FactionRegistry>) {
    if clock.tick % 60 != 0 {
        return;
    }
    for faction in registry.factions.values_mut() {
        if faction.member_count == 0 {
            faction.material_targets.clear();
            continue;
        }
        // Baseline: scale with member count so larger tribes hold more
        // headroom for spontaneous upgrades.
        let members = faction.member_count;
        let mut wood_target = (members * 2).max(8);
        let mut stone_target = members.max(4);

        // Culture modulation. Trait values are 0..=255; clamp the multiplier
        // to a sane range so high-defense factions stockpile ~50% more stone.
        let scale_u32 = |base: u32, trait_value: u8, max_bonus: f32| -> u32 {
            let t = trait_value as f32 / 255.0;
            let mult = 1.0 + t * max_bonus;
            (base as f32 * mult).round() as u32
        };
        // Defensive cultures want more stone (walls); martial want both.
        stone_target = scale_u32(stone_target, faction.culture.defensive, 0.5);
        stone_target = scale_u32(stone_target, faction.culture.martial, 0.25);
        wood_target = scale_u32(wood_target, faction.culture.martial, 0.25);
        // Ceremonial bumps stone (shrines/monuments).
        stone_target = scale_u32(stone_target, faction.culture.ceremonial, 0.3);
        // Mercantile bumps wood (markets, granaries).
        wood_target = scale_u32(wood_target, faction.culture.mercantile, 0.25);

        // Tech foresight: once flint knapping or settlement is unlocked,
        // construction tier-ups will start consuming stone in volume.
        if faction
            .techs
            .has(crate::simulation::technology::FLINT_KNAPPING)
        {
            stone_target = stone_target.saturating_add(4);
        }
        if faction
            .techs
            .has(crate::simulation::technology::PERM_SETTLEMENT)
        {
            wood_target = wood_target.saturating_add(4);
        }

        let wood_id = crate::economy::core_ids::wood();
        let stone_id = crate::economy::core_ids::stone();
        faction.material_targets.insert(wood_id, wood_target);
        faction.material_targets.insert(stone_id, stone_target);
    }
}

// ── Chief selection system ────────────────────────────────────────────────────

/// Ensures every non-SOLO faction has a designated tribal chief.
/// Runs every 60 ticks. If the current chief has left or died, elects any
/// surviving faction member as the new chief.
pub fn chief_selection_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    member_query: Query<(Entity, &FactionMember)>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    // Build faction_id → member entities map from the current world state.
    let mut faction_members: AHashMap<u32, Vec<Entity>> = AHashMap::new();
    for (entity, member) in member_query.iter() {
        if member.faction_id != SOLO {
            faction_members
                .entry(member.faction_id)
                .or_default()
                .push(entity);
        }
    }

    for (&faction_id, faction) in registry.factions.iter_mut() {
        let members = match faction_members.get(&faction_id) {
            Some(m) if !m.is_empty() => m,
            _ => {
                faction.chief_entity = None;
                continue;
            }
        };

        let chief_valid = faction
            .chief_entity
            .map(|e| members.contains(&e))
            .unwrap_or(false);

        if !chief_valid {
            let old_chief = faction.chief_entity;
            let new_chief = members[0];
            faction.chief_entity = Some(new_chief);
            commands.entity(new_chief).insert(FactionChief);
            if let Some(old) = old_chief {
                if old != new_chief {
                    if let Some(mut ec) = commands.get_entity(old) {
                        ec.remove::<FactionChief>();
                    }
                }
                // Succession drift — only counts as a transition if there was
                // a prior chief (the founding chief sets generation 0).
                faction.lineage.generation = faction.lineage.generation.saturating_add(1);
                drift_culture(&mut faction.culture, faction.lineage.generation);
            }
        }
    }
}

/// Sole writer of `FactionData.techs`. Each Economy tick, project the chief's
/// `PersonKnowledge.aware` bitset onto the faction so existing read sites
/// (plan filters, recipe gates, building gates, era checks) reflect the
/// leader's awareness. If the faction has no chief, leave the previous value
/// untouched — `chief_selection_system` runs every 60 ticks and will refill it.
pub fn sync_faction_techs_from_chief_system(
    mut registry: ResMut<FactionRegistry>,
    chief_q: Query<&crate::simulation::knowledge::PersonKnowledge>,
) {
    for (_id, faction) in registry.factions.iter_mut() {
        let Some(chief) = faction.chief_entity else {
            continue;
        };
        let Ok(knowledge) = chief_q.get(chief) else {
            continue;
        };
        // Mask to valid tech bits (lower TECH_COUNT) to keep the bitset clean.
        let mask = if TECH_COUNT >= 64 {
            u64::MAX
        } else {
            (1u64 << TECH_COUNT) - 1
        };
        faction.techs.0 = knowledge.aware & mask;
    }
}

/// Drift the five culture traits by ±10 deterministically based on the
/// generation count. Successive chiefs gradually shift settlement personality
/// without erasing the founder's identity. Layout style is left untouched —
/// architectural identity persists across generations.
fn drift_culture(culture: &mut FactionCulture, generation: u32) {
    let mut s = culture
        .seed
        .wrapping_add(generation.wrapping_mul(0x9E37_79B9));
    let mut next = || {
        s ^= s >> 16;
        s = s.wrapping_mul(0x85EB_CA6B);
        s ^= s >> 13;
        s
    };
    let drift = |val: u8, raw: u32| -> u8 {
        let delta = (raw % 21) as i32 - 10; // -10..=+10
        (val as i32 + delta).clamp(0, 255) as u8
    };
    culture.density = drift(culture.density, next());
    culture.defensive = drift(culture.defensive, next());
    culture.ceremonial = drift(culture.ceremonial, next());
    culture.mercantile = drift(culture.mercantile, next());
    culture.martial = drift(culture.martial, next());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crafter_target_promotes_above_floor() {
        // Live signal + member room → ratio-driven target.
        let t = crafter_target_with_hysteresis(2.0, 0, 12);
        // 12 × 0.25 = 3, capped at 12/3 = 4.
        assert_eq!(t, 3);
    }

    #[test]
    fn crafter_target_demotes_below_ceiling() {
        // Stale signal → target collapses to 0 regardless of incumbents.
        let t = crafter_target_with_hysteresis(0.1, 4, 12);
        assert_eq!(t, 0);
    }

    #[test]
    fn crafter_target_holds_in_deadband() {
        // ema between 0.3 and 1.0 → keep current count (no churn).
        let t_with_two = crafter_target_with_hysteresis(0.5, 2, 12);
        assert_eq!(t_with_two, 2);
        let t_with_zero = crafter_target_with_hysteresis(0.5, 0, 12);
        assert_eq!(t_with_zero, 0);
    }

    #[test]
    fn crafter_target_capped_by_member_divisor() {
        // 24 members → cap at 24/3 = 8. Ratio 0.25 × 24 = 6, so target = 6.
        let t = crafter_target_with_hysteresis(5.0, 0, 24);
        assert_eq!(t, 6);
        // 3 members → cap at 1. Even with active signal, can't promote more.
        let t_small = crafter_target_with_hysteresis(5.0, 0, 3);
        assert_eq!(t_small, 1);
    }

    #[test]
    fn crafter_target_zero_when_no_members() {
        let t = crafter_target_with_hysteresis(5.0, 0, 0);
        assert_eq!(t, 0);
    }

    #[test]
    fn nearest_for_faction_reachable_prefers_raised_reachable_over_farther_same_z() {
        // Layout:
        // - chunk (0,0) surface z=0 — agent stands at (5, 5, 0).
        // - chunk (1,0) surface z=1 — reachable from chunk (0,0) via the
        //   cross-chunk stair-step edge (|Δz| ≤ 1). Storage tile A at
        //   (32, 5, z=1) — nearby but raised.
        // - chunk (5,5) surface z=0 — isolated, not adjacent to anything
        //   built. Storage tile B at (165, 165, z=0) — farther but at the
        //   agent's Z.
        //
        // Old `(chunk, z)` reachability checked at the agent's z=0 and
        // rejected the raised tile (chunk (1,0) has no component at z=0),
        // then fell back to the blind result by Manhattan distance — which
        // might pick *either* tile depending on chunk indexing. New
        // `tile_reachable` resolves the storage tile's actual standable z
        // and follows component identity, so it definitively picks A.
        use crate::pathfinding::chunk_graph::{rebuild_chunk_graph_sync, ChunkGraph};
        use crate::pathfinding::connectivity::{
            populate_connectivity_from_graph, ChunkConnectivity,
        };
        use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
        use crate::world::tile::TileKind;

        fn flat_chunk(surf_z: i8) -> Chunk {
            let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
            let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
            let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
            Chunk::new(surface_z, surface_kind, surface_fertility)
        }

        let mut chunk_map = ChunkMap::default();
        chunk_map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        chunk_map.0.insert(ChunkCoord(1, 0), flat_chunk(1));
        chunk_map.0.insert(ChunkCoord(5, 5), flat_chunk(0));

        let mut graph = ChunkGraph::default();
        rebuild_chunk_graph_sync(&chunk_map, &mut graph);

        let mut conn = ChunkConnectivity::default();
        populate_connectivity_from_graph(&graph, &mut conn);
        let router = crate::pathfinding::chunk_router::ChunkRouter::default();

        let agent_tile = (5, 5, 0i8);
        let raised_tile = (32, 5); // chunk (1,0) — reachable via stair-step
        let isolated_tile = (165, 165); // chunk (5,5) — isolated

        // Sanity: confirm the raised tile is reachable per the exact API.
        assert!(
            conn.tile_reachable(&graph, agent_tile, (raised_tile.0, raised_tile.1, 1)),
            "raised tile must be reachable via the stair-step cross-chunk edge"
        );
        assert!(
            !conn.tile_reachable(&graph, agent_tile, (isolated_tile.0, isolated_tile.1, 0),),
            "isolated chunk must be unreachable"
        );

        // Storage tile map carries both candidates for faction 7. Manhattan
        // distance from agent (5, 5): raised (32, 5) = 27; isolated
        // (165, 165) = 320. Both unreachable would tie-break to raised;
        // we still want to confirm the reachability filter wins.
        let mut stm = StorageTileMap::default();
        stm.tiles.insert(raised_tile, 7);
        stm.tiles.insert(isolated_tile, 7);
        stm.by_faction.insert(7, vec![raised_tile, isolated_tile]);

        let picked = stm.nearest_for_faction_reachable(
            7, (5, 5), agent_tile, &chunk_map, &graph, &router, &conn,
        );
        assert_eq!(
            picked,
            Some(raised_tile),
            "reachable raised tile must beat the isolated tile"
        );
    }
}
