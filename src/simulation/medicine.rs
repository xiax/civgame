//! Heal-pipeline data layer (Heal-1).
//!
//! Tracks combat-induced damage as an `Injury` component derived from
//! `Body.fraction()` — i.e. cumulative limb damage on humanoid agents.
//! Persons in this sim don't carry `Health`; they use a per-limb
//! `Body` component, so the watcher reacts to `Changed<Body>`. The
//! component is what `HealNeedScorer` (Heal-2) reads to gate
//! `AgentGoal::SeekCare`, and what `ProvideCareScorer` reads to find
//! treatable patients.
//!
//! Severity is `((1.0 - body.fraction()) * 255).round().clamp(0, 255)`
//! — a fully-intact body returns 0, a half-broken body returns 128,
//! a near-dead one returns near 255. When the heal pipeline (Heal-3)
//! restores limb HP, the same system clears the component.
//! No combat-system edit required: insertion is reactive on
//! `Changed<Body>`.

use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::combat::Body;
use crate::simulation::faction::FactionMember;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI, Profession, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::TILE_SIZE;

/// Per-agent illness record. Independent of `Injury` (which is *derived*
/// from `Body` damage via `injury_tracking_system`, so making it carry
/// a `cause` field would mean the body-derived path constantly clobbered
/// the illness side). A parallel component keeps the lifecycles
/// orthogonal: a sick agent can also be wounded, and the two recover
/// on their own clocks.
///
/// Decays by `SICKNESS_DECAY_PER_DAY` per game-day until severity hits
/// zero, at which point `sickness_decay_system` removes the component.
/// Phase 5 grants this component via `apply_sickness` when an agent
/// drinks raw / contaminated water; future industrial / contamination
/// vectors land on the same helper.
#[derive(Component, Clone, Copy, Debug)]
pub struct Sickness {
    /// 0..=255. Drives a mood penalty and a small work-progress
    /// slowdown read at executor sites that consult `Sickness` (Phase 5
    /// limits the slowdown to a single helper; future passes can fan
    /// out to combat/movement).
    pub severity: u8,
    pub applied_tick: u64,
}

/// Severity decrement per game-day. With `apply_sickness` writing
/// severity 80–160 typical, illnesses clear in 4–10 game-days.
pub const SICKNESS_DECAY_PER_DAY: u8 = 16;

/// Mild slowdown applied to work-progress increments while sick. Phase 5
/// limits the wiring to a single helper; executor sites that consult
/// `Sickness` can call `sickness_work_factor(severity)` to get a multiplier
/// in `[0.5, 1.0]` for the per-tick `work_progress` advance.
pub fn sickness_work_factor(severity: u8) -> f32 {
    // 255 severity → 0.5×; 0 → 1.0×.
    let s = severity as f32 / 255.0;
    (1.0 - 0.5 * s).clamp(0.5, 1.0)
}

/// Apply or merge an illness severity onto an entity. Idempotent: if
/// the entity already carries `Sickness`, severity is `max`'d with the
/// incoming value so a more serious bout dominates. Caller is
/// responsible for the world write — this helper is called from a
/// system with `&mut World` access via the wrapper below.
pub fn apply_sickness_severity(
    existing: Option<&mut Sickness>,
    severity: u8,
    now: u64,
) -> Option<Sickness> {
    match existing {
        Some(s) => {
            s.severity = s.severity.max(severity);
            None
        }
        None => Some(Sickness {
            severity,
            applied_tick: now,
        }),
    }
}

/// Severity assigned when an agent drinks raw (non-river) freshwater.
pub const SICKNESS_RAW_DRINK_SEVERITY: u8 = 60;
/// Severity assigned when the source water is `SanitationMap`-contaminated
/// above the drink threshold. Heavier so the operator sees the difference
/// in the inspector.
pub const SICKNESS_CONTAMINATED_DRINK_SEVERITY: u8 = 140;

/// Daily decay system. Iterates every entity carrying `Sickness`,
/// subtracts `SICKNESS_DECAY_PER_DAY`, despawns the component on zero.
/// Runs Economy / daily.
pub fn sickness_decay_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut query: Query<(Entity, &mut Sickness)>,
) {
    if clock.tick % crate::world::seasons::TICKS_PER_DAY as u64 != 0 {
        return;
    }
    for (entity, mut sickness) in query.iter_mut() {
        sickness.severity = sickness.severity.saturating_sub(SICKNESS_DECAY_PER_DAY);
        if sickness.severity == 0 {
            commands.entity(entity).remove::<Sickness>();
        }
    }
}

/// Per-agent injury record. Stamped by `injury_tracking_system` when
/// `Health.current < Health.max`; removed when fully healed.
///
/// `severity` is the `max - current` gap at the most recent damage
/// tick. `applied_tick` captures the first damage event of the
/// current injury (across consecutive damage windows); reset only
/// when the component is fully cleared. `last_damage_tick` is the
/// most recent damage write — triage scorers prefer fresher wounds
/// (an old scar isn't urgent).
#[derive(Component, Clone, Copy, Debug)]
pub struct Injury {
    pub severity: u8,
    pub applied_tick: u64,
    pub last_damage_tick: u64,
}

/// Convert a `Body` into a 0-255 severity value. Returns 0 for a
/// fully-intact body, scales linearly with `1.0 - body.fraction()`,
/// clamps to 255. Pulled out so the system body stays focused on
/// the insert / update / remove decision tree.
#[inline]
pub fn severity_from_body(body: &Body) -> u8 {
    let frac = body.fraction().clamp(0.0, 1.0);
    let severity = ((1.0 - frac) * 255.0).round();
    severity.clamp(0.0, 255.0) as u8
}

/// Sequential schedule, after `combat::combat_system` so the same
/// frame's damage is reflected. Reactive on `Changed<Body>` — every
/// limb-damage write reactivates the filter, fires this system once
/// per affected entity.
pub fn injury_tracking_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut query: Query<(Entity, &Body, Option<&mut Injury>), Changed<Body>>,
) {
    let now = clock.tick;
    for (entity, body, injury_opt) in query.iter_mut() {
        let severity = severity_from_body(body);
        match (severity, injury_opt) {
            (0, Some(_)) => {
                commands.entity(entity).remove::<Injury>();
            }
            (0, None) => {}
            (severity, Some(mut injury)) => {
                if severity != injury.severity {
                    injury.severity = severity;
                    injury.last_damage_tick = now;
                }
            }
            (severity, None) => {
                commands.entity(entity).insert(Injury {
                    severity,
                    applied_tick: now,
                    last_damage_tick: now,
                });
            }
        }
    }
}

/// Limb HP restored per tick while the Healer is adjacent to the
/// patient. The Healer cycles through limbs in `Body.parts` order,
/// adding 1 HP to the first damaged limb each tick. A 30-HP leg
/// brought from 0 → 30 takes ~1.5 seconds @ 20 Hz; severity falls
/// proportionally as `injury_tracking_system` re-derives it from
/// `Body.fraction()`. Future Heal-3b: scale by `Skills::Medicine`
/// competence and gate on a medicine resource consumed from
/// inventory.
pub const HEAL_LIMB_HP_PER_TICK: u8 = 1;
/// Medicine XP granted to the Healer per tick of adjacent treatment.
pub const HEAL_MEDICINE_XP_PER_TICK: u32 = 1;
/// Chebyshev radius within which the Healer can treat the patient.
pub const HEAL_ADJACENCY_RADIUS: i32 = 1;
/// Maximum chebyshev radius the `htn_provide_care_dispatch_system`
/// will scan for patients from the Healer's tile. Mirrors the
/// `PARTNER_RADIUS = 12` used by socialize/play dispatchers.
pub const HEAL_SCAN_RADIUS: i32 = 12;
/// Heal-3b: chebyshev radius at which a SeekCare patient is considered
/// "at the recovery site". When already within this distance of the
/// chosen Shrine / home_tile, `htn_seek_care_dispatch_system` skips
/// re-routing so the patient idles in place while Healers come to
/// them. Set to half `HEAL_SCAN_RADIUS` so a patient who walks to the
/// site lands comfortably inside the Healer's sweep ring even after
/// drift.
pub const SEEK_CARE_AT_SITE_RADIUS: i32 = 6;

/// Heal-5: cadence at which `chief_healer_assignment_system`
/// reconciles Healer headcount with injured-member demand. Matches the
/// `BUREAUCRAT_ASSIGNMENT_CADENCE` / `CRAFTER_ASSIGNMENT_CADENCE` so
/// chief decisions about specialized labour share a heartbeat.
pub const HEALER_ASSIGNMENT_CADENCE: u64 = (crate::world::seasons::TICKS_PER_DAY / 4) as u64;
/// Number of injured agents one Healer is expected to serve before the
/// chief promotes a second. Healing takes minutes per limb so one
/// Healer can comfortably cycle through ~4 patients before backlog.
pub const HEALER_PER_INJURY_DIVISOR: u32 = 4;
/// Asymmetric hysteresis: tolerate this many Healers above target
/// before demoting on the next cadence. Mirrors `HUNTER_DEMOTE_BUFFER`
/// / `BUREAUCRAT_DEMOTE_BUFFER` so a single-tick injured-count drop
/// doesn't churn the roster.
pub const HEALER_DEMOTE_BUFFER: usize = 1;
/// Hard cap: never let more than `member_count / HEALER_MAX_DIVISOR` of
/// the band be Healers. Matches the Crafter cap so specialized labour
/// stays a minority share of the population.
pub const HEALER_MAX_DIVISOR: usize = 3;

/// HTN dispatcher for `AgentGoal::ProvideCare`. Iterates Healers
/// (and Apprentices targeting Medicine) under the ProvideCare goal,
/// finds the nearest same-faction patient with an `Injury`, and
/// dispatches `Task::Heal { patient }` via the standard routing
/// helper. ParallelB schedule, mirroring the other goal-driven
/// dispatchers.
#[allow(clippy::too_many_arguments)]
pub fn htn_provide_care_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    injured_query: Query<(Entity, &Transform, &FactionMember), With<Injury>>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &Profession,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    for (agent, mut ai, mut aq, goal, transform, member, profession, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::ProvideCare) {
            continue;
        }
        if !matches!(*profession, Profession::Healer | Profession::Apprentice) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        let mut best: Option<(Entity, (i32, i32), i32)> = None;
        for (patient, patient_t, patient_member) in injured_query.iter() {
            if patient == agent {
                continue;
            }
            if patient_member.faction_id != member.faction_id {
                continue;
            }
            let px = (patient_t.translation.x / TILE_SIZE).floor() as i32;
            let py = (patient_t.translation.y / TILE_SIZE).floor() as i32;
            let d = (px - cur_tx).abs().max((py - cur_ty).abs());
            if d > HEAL_SCAN_RADIUS {
                continue;
            }
            if best.map_or(true, |(_, _, bd)| d < bd) {
                best = Some((patient, (px, py), d));
            }
        }

        let Some((patient, patient_tile, _)) = best else {
            continue;
        };
        let dispatched = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            patient_tile,
            TaskKind::Heal,
            Some(patient),
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !dispatched {
            continue;
        }
        aq.dispatch(Task::Heal { patient });
    }
}

/// Sequential executor for `Task::Heal { patient }`. Runs after
/// `combat_system`. Verifies the Healer is still adjacent to the
/// targeted patient; if so, restores limb HP on the patient's
/// `Body` (which `injury_tracking_system` then translates back into
/// a lower `Injury.severity` on the next frame) and grants Medicine
/// XP. If the patient moved out of range or fully recovered,
/// advances the action queue so the Healer's next goal re-evaluation
/// can pick a new patient.
#[allow(clippy::too_many_arguments)]
pub fn heal_task_system(
    mut healer_query: Query<(
        Entity,
        &mut ActionQueue,
        &mut PersonAI,
        &Transform,
        &mut Skills,
        Option<&crate::simulation::apprenticeship::ApprenticeOf>,
    )>,
    transform_query: Query<&Transform>,
    mut body_query: Query<&mut Body>,
) {
    for (_healer, mut aq, mut ai, healer_t, mut skills, apprentice_opt) in healer_query.iter_mut() {
        let Task::Heal { patient } = aq.current else {
            continue;
        };
        if ai.state != AiState::Working {
            continue;
        }
        let Ok(patient_t) = transform_query.get(patient) else {
            aq.advance();
            continue;
        };
        let hx = (healer_t.translation.x / TILE_SIZE).floor() as i32;
        let hy = (healer_t.translation.y / TILE_SIZE).floor() as i32;
        let px = (patient_t.translation.x / TILE_SIZE).floor() as i32;
        let py = (patient_t.translation.y / TILE_SIZE).floor() as i32;
        let d = (px - hx).abs().max((py - hy).abs());
        if d > HEAL_ADJACENCY_RADIUS {
            aq.advance();
            continue;
        }
        let Ok(mut body) = body_query.get_mut(patient) else {
            aq.advance();
            continue;
        };
        let mut healed_something = false;
        for limb in body.parts.iter_mut() {
            if limb.current < limb.max {
                limb.current = limb
                    .current
                    .saturating_add(HEAL_LIMB_HP_PER_TICK)
                    .min(limb.max);
                healed_something = true;
                break;
            }
        }
        if healed_something {
            let xp = crate::simulation::apprenticeship::xp_with_apprentice_bonus(
                HEAL_MEDICINE_XP_PER_TICK,
                apprentice_opt,
            );
            skills.gain_xp(SkillKind::Medicine, xp);
            // injury_tracking_system observes the body change next
            // frame and despawns Injury when fraction reaches 1.0.
        } else {
            // Body fully intact — patient recovered.
            aq.advance();
        }
    }
}

/// Heal-3b: dispatcher for `AgentGoal::SeekCare`. Routes an injured
/// agent to the nearest faction-owned `Shrine` (a known recovery
/// site that Healers `htn_provide_care_dispatch_system` will sweep)
/// or, when no Shrine exists, to the faction's `home_tile`. Patients
/// already inside `SEEK_CARE_AT_SITE_RADIUS` of the chosen site stay
/// put so Healers can converge without ping-pong. ParallelB schedule,
/// after `htn_provide_care_dispatch_system`.
#[allow(clippy::too_many_arguments)]
pub fn htn_seek_care_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<crate::simulation::faction::FactionRegistry>,
    ownership: Res<crate::simulation::capital::WorkshopOwnership>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
        ),
        (Without<Drafted>, With<Injury>),
    >,
) {
    use crate::simulation::capital::WorkshopKind;
    for (mut ai, mut aq, goal, transform, member, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::SeekCare) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Recovery site: nearest faction-owned Shrine, else faction
        // home_tile. SOLO agents have no home_tile / Shrines, so skip.
        let mut target: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for entry in ownership.workshops_for(member.faction_id) {
            if entry.kind != WorkshopKind::Shrine {
                continue;
            }
            let d = (entry.tile.0 - cur_tx)
                .abs()
                .max((entry.tile.1 - cur_ty).abs());
            if d < best_dist {
                best_dist = d;
                target = Some(entry.tile);
            }
        }
        if target.is_none() {
            target = faction_registry.home_tile(member.faction_id);
            if let Some(t) = target {
                best_dist = (t.0 - cur_tx).abs().max((t.1 - cur_ty).abs());
            }
        }
        let Some(dest) = target else {
            continue;
        };

        // Already at the site — idle in place so the Healer can sweep
        // us. Avoids per-tick re-dispatch loops.
        if best_dist <= SEEK_CARE_AT_SITE_RADIUS {
            continue;
        }

        let dispatched = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            dest,
            TaskKind::SeekCare,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !dispatched {
            continue;
        }
        let z = ai.target_z;
        aq.dispatch(Task::WalkTo {
            tile: dest,
            z,
            why: crate::simulation::typed_task::WalkReason::SeekCare,
        });
    }
}

/// Heal-5: chief-driven Healer assignment. Mirrors
/// `chief_bureaucrat_appointment_system` / `chief_craft_assignment_system`
/// in shape — EV-ranked promote, apprenticeship for sub-`APPRENTICE_THRESHOLD`
/// Medicine, asymmetric demote buffer, survival override — but the
/// target headcount is driven by the *injured-member tally* rather
/// than a wage signal. The faction needs a Healer when its people are
/// hurt, not when there's a paid heal-job EMA (no such job-kind ships
/// yet); a future `JobKind::Heal` can replace the injured-count proxy
/// without changing this system's shape.
///
/// Target per faction:
///   - `per_head_food < FARMER_SURVIVAL_FLOOR` → 0 (specialized labour
///     surrenders to the Farmer ramp during famine).
///   - no injured members → 0 (Healers demote out when the band is
///     fully healed).
///   - else → `min(max(1, injured.div_ceil(HEALER_PER_INJURY_DIVISOR)),
///     member_count / HEALER_MAX_DIVISOR)`. A faction of 12 with 6
///     injured wants `max(1, 6/4) = 2` Healers; a faction of 4 with 3
///     injured wants `min(1, 4/3) = 1`.
///
/// Apprentice path: sub-`APPRENTICE_THRESHOLD` Medicine candidates
/// route through `Profession::Apprentice` with
/// `ApprenticeProgress::target_profession = Healer`, bound to a master
/// Healer (`Skills[Medicine] >= MASTER_THRESHOLD`, no live `MentorOf`).
/// Without a master the candidate falls back to direct promotion so a
/// faction without elders can still bootstrap.
#[allow(clippy::too_many_arguments)]
pub fn chief_healer_assignment_system(
    clock: Res<SimClock>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    reservations: Res<crate::simulation::faction::StorageReservations>,
    ownership: Res<crate::simulation::capital::WorkshopOwnership>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plots: Query<&crate::simulation::land::Plot>,
    mentors_q: Query<&crate::simulation::apprenticeship::MentorOf>,
    injured_q: Query<&FactionMember, With<Injury>>,
    mut commands: Commands,
    mut activity: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut query: Query<(
        Entity,
        &mut Profession,
        &FactionMember,
        &Skills,
        &crate::economy::agent::EconomicAgent,
        &crate::simulation::carry::Carrier,
        &Transform,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        Option<&mut PersonAI>,
        Option<&mut ActionQueue>,
    )>,
) {
    use crate::simulation::apprenticeship::{
        ApprenticeOf, ApprenticeProgress, MentorOf, APPRENTICE_THRESHOLD, MASTER_THRESHOLD,
    };
    use crate::simulation::faction::{FARMER_SURVIVAL_FLOOR, SOLO};
    use crate::simulation::person::Profession;

    if clock.tick % HEALER_ASSIGNMENT_CADENCE != 0 {
        return;
    }

    // Pass 1: per-faction injured tally + Healer / Apprentice census.
    let mut injured_per_faction: ahash::AHashMap<u32, u32> = ahash::AHashMap::default();
    for member in injured_q.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        *injured_per_faction.entry(member.faction_id).or_insert(0) += 1;
    }
    let mut current_healers: ahash::AHashMap<u32, usize> = ahash::AHashMap::default();
    let mut available_mentors: ahash::AHashMap<u32, Vec<Entity>> = ahash::AHashMap::default();
    for (entity, prof, member, skills, _, _, _, _, _, _) in query.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        match *prof {
            Profession::Healer => {
                *current_healers.entry(member.faction_id).or_insert(0) += 1;
                let medicine = skills.0[SkillKind::Medicine as usize];
                if medicine >= MASTER_THRESHOLD && mentors_q.get(entity).is_err() {
                    available_mentors
                        .entry(member.faction_id)
                        .or_default()
                        .push(entity);
                }
            }
            Profession::Apprentice => {
                // Apprentice headcount counts toward Healer target only
                // when their training targets Healer. Crafter-targeted
                // apprentices are not Healer-trainees and stay invisible
                // to this system.
                // We can't read ApprenticeProgress in this query without
                // pushing the param count over Bevy's ceiling; instead,
                // the safer assumption is "Apprentice doesn't count
                // toward Healer headcount" — at worst we over-promote a
                // Healer while a Crafter-apprentice graduates, which
                // self-corrects on the next cadence.
            }
            _ => {}
        }
    }

    // Pass 2: build per-faction targets.
    let mut targets: ahash::AHashMap<u32, usize> = ahash::AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        let per_head = if faction.member_count > 0 {
            faction.storage.food_total() / faction.member_count as f32
        } else {
            f32::INFINITY
        };
        if per_head < FARMER_SURVIVAL_FLOOR {
            targets.insert(fid, 0);
            continue;
        }
        let injured = injured_per_faction.get(&fid).copied().unwrap_or(0);
        if injured == 0 {
            targets.insert(fid, 0);
            continue;
        }
        let demand =
            ((injured + HEALER_PER_INJURY_DIVISOR - 1) / HEALER_PER_INJURY_DIVISOR).max(1) as usize;
        let cap = (faction.member_count as usize) / HEALER_MAX_DIVISOR;
        let target = demand.min(cap.max(1));
        targets.insert(fid, target);
    }

    if targets.is_empty() {
        return;
    }

    // Pass 3: EV-ranked candidate buckets per faction.
    let mut by_faction_healers: ahash::AHashMap<u32, Vec<(Entity, f32, u32)>> =
        ahash::AHashMap::default();
    let mut by_faction_none: ahash::AHashMap<u32, Vec<(Entity, f32, u32)>> =
        ahash::AHashMap::default();
    for (entity, prof, member, skills, agent, carrier, xf, household_opt, _, _) in query.iter() {
        if member.faction_id == SOLO || !targets.contains_key(&member.faction_id) {
            continue;
        }
        let medicine = skills.0[SkillKind::Medicine as usize];
        let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
        let cap = crate::simulation::capital::capital_factor(
            agent,
            carrier,
            tile,
            member.faction_id,
            household_opt,
            Profession::Healer,
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
                    Profession::Healer,
                    skills,
                    cap,
                )
            })
            .unwrap_or(0.0);
        match *prof {
            Profession::Healer => by_faction_healers
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, medicine)),
            Profession::None => by_faction_none
                .entry(member.faction_id)
                .or_default()
                .push((entity, ev, medicine)),
            _ => {}
        }
    }

    let mut promote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut demote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for (&fid, &want) in &targets {
        let mut healers = by_faction_healers.remove(&fid).unwrap_or_default();
        let mut none = by_faction_none.remove(&fid).unwrap_or_default();
        if healers.len() < want {
            none.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.2.cmp(&a.2))
            });
            for (e, _, _) in none.into_iter().take(want - healers.len()) {
                promote.insert(e);
            }
        } else if healers.len() > want.saturating_add(HEALER_DEMOTE_BUFFER) || want == 0 {
            healers.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.2.cmp(&b.2))
            });
            let extra = healers.len() - want;
            for (e, _, _) in healers.into_iter().take(extra) {
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
            let medicine = skills.0[SkillKind::Medicine as usize];
            if medicine < APPRENTICE_THRESHOLD {
                if let Some(pool) = available_mentors.get_mut(&member.faction_id) {
                    if let Some(mentor) = pool.pop() {
                        *prof = Profession::Apprentice;
                        commands
                            .entity(entity)
                            .insert(ApprenticeOf { mentor })
                            .insert(ApprenticeProgress {
                                ticks: 0,
                                target_ticks: ApprenticeProgress::default().target_ticks,
                                target_profession: Profession::Healer,
                            });
                        commands
                            .entity(mentor)
                            .insert(MentorOf { apprentice: entity });
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
            *prof = Profession::Healer;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::combat::BodyPart;

    fn build_app() -> App {
        let mut app = App::new();
        app.insert_resource(SimClock::default());
        app.add_systems(Update, injury_tracking_system);
        app
    }

    fn damage_torso(app: &mut App, entity: Entity, dmg: u8) {
        let mut body = app.world_mut().get_mut::<Body>(entity).unwrap();
        let torso = body.get_mut(BodyPart::Torso);
        torso.current = torso.current.saturating_sub(dmg);
    }

    #[test]
    fn damage_inserts_injury() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        // First tick: undamaged body, no injury.
        app.update();
        assert!(app.world().get::<Injury>(entity).is_none());

        damage_torso(&mut app, entity, 20);
        app.update();
        let injury = app
            .world()
            .get::<Injury>(entity)
            .expect("Injury inserted after Body damage");
        assert!(injury.severity > 0);
    }

    #[test]
    fn additional_damage_updates_severity() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        damage_torso(&mut app, entity, 10);
        app.update();
        let first = app.world().get::<Injury>(entity).unwrap().severity;
        damage_torso(&mut app, entity, 15);
        app.update();
        let second = app.world().get::<Injury>(entity).unwrap().severity;
        assert!(second > first, "more damage must raise severity");
    }

    #[test]
    fn full_heal_removes_injury() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        damage_torso(&mut app, entity, 10);
        app.update();
        assert!(app.world().get::<Injury>(entity).is_some());

        // Restore the limb.
        let mut body = app.world_mut().get_mut::<Body>(entity).unwrap();
        let torso = body.get_mut(BodyPart::Torso);
        torso.current = torso.max;
        app.update();
        assert!(
            app.world().get::<Injury>(entity).is_none(),
            "Injury should clear when Body.fraction() == 1.0"
        );
    }

    #[test]
    fn redundant_body_write_doesnt_flap_injury() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        damage_torso(&mut app, entity, 10);
        app.update();
        let applied_at = app.world().get::<Injury>(entity).unwrap().applied_tick;
        let severity_before = app.world().get::<Injury>(entity).unwrap().severity;

        // Re-write Body without changing limb values — Changed
        // filter still fires, but severity unchanged so applied_tick
        // should not reset.
        let _ = app.world_mut().get_mut::<Body>(entity).unwrap();
        app.update();
        let injury = app.world().get::<Injury>(entity).unwrap();
        assert_eq!(injury.severity, severity_before);
        assert_eq!(injury.applied_tick, applied_at);
    }

    #[test]
    fn severity_from_body_endpoints() {
        let intact = Body::new_humanoid();
        assert_eq!(severity_from_body(&intact), 0);

        // Destroy every limb → fraction 0 → severity 255.
        let mut wrecked = Body::new_humanoid();
        for limb in wrecked.parts.iter_mut() {
            limb.current = 0;
        }
        assert_eq!(severity_from_body(&wrecked), 255);
    }
}
