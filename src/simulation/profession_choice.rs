//! Phase 4b scaffolding (wage-aware-labor-market-v2).
//!
//! Pure-function helpers + shared `demote_profession_state` teardown
//! consumed by the legacy assignment systems. The full unified
//! `profession_choice_system` (single EV-driven argmax replacing the
//! cores of `faction_profession_system`, `faction_hunter_assignment_system`,
//! and `chief_bureaucrat_appointment_system`) is a follow-up; this
//! module ships the building blocks so that follow-up is mechanical.
//!
//! Today's wiring:
//! - `demote_profession_state` is called from the hunter and bureaucrat
//!   assignment systems' demote arms, replacing three near-identical
//!   copies of the same AI / ActionQueue / Carrying teardown.
//! - `skill_competence` / `job_kinds_for` / `aggregate_wage_per_day`
//!   / `expected_wage` are exposed so the future EV system can compose
//!   them without re-importing every dependency.

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::faction::{
    release_reservation, FactionData, FactionMember, StorageReservations,
};
use crate::simulation::jobs::JobKind;
use crate::simulation::person::{AiState, PersonAI, Profession};
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills, SKILL_MAX};
use crate::simulation::typed_task::ActionQueue;
use bevy::prelude::*;

/// Skill competence normalised to `[0.2, 1.0]`. The 0.2 floor prevents a
/// fresh agent's EV from collapsing to zero — a Hunter promotion has to
/// be possible even when nobody has any Combat XP yet.
pub fn skill_competence(skill: u32) -> f32 {
    let clamped = skill.min(SKILL_MAX) as f32 / SKILL_MAX as f32;
    0.2 + clamped * 0.8
}

/// Map a profession to the `(JobKind, Option<ResourceId>)` keys whose
/// wages count toward that profession's expected income. `None` for
/// `target_rid` matches every variant of that kind in the signal map.
///
/// Returned by-value rather than `&'static` so future per-faction or
/// per-era extensions (e.g. Crafter relying on `Stockpile{wood}` *and*
/// `Craft{tools}`) can compose without redesigning the trait. Sized for
/// the common path — under 4 entries per profession.
pub fn job_kinds_for(prof: Profession) -> &'static [JobKind] {
    match prof {
        Profession::None => &[],
        Profession::Farmer => &[JobKind::Farm, JobKind::Stockpile],
        Profession::Hunter => &[JobKind::Stockpile],
        Profession::Bureaucrat => &[JobKind::Build],
        Profession::Trader => &[JobKind::Haul],
        // Crafter earns from `Craft` postings primarily; `Stockpile`
        // captures the upstream gather → deposit work crafters often
        // do alongside (a fletcher gathers wood when craft demand is
        // slack). `Haul` is omitted — that's Trader territory.
        Profession::Crafter => &[JobKind::Craft, JobKind::Stockpile],
        // Phase 5b: Apprentices are mid-training Crafters. They draw
        // wages from the same kinds — when a paid Craft posting lands
        // they help (and earn the apprentice-fraction share) — so the
        // EV signal feeds the same way.
        Profession::Apprentice => &[JobKind::Craft, JobKind::Stockpile],
        // Phase 5b-stretch: Healer draws wages from the Craft pipe
        // until a dedicated `JobKind::Heal` lands (the precondition
        // for Healers to be EV-promotable). Today the `Craft` slot is
        // the closest analogue for a paid skilled service.
        Profession::Healer => &[JobKind::Craft],
        // sleepy-dove scaffolding: Architects author construction
        // blueprints. They earn wages from Build postings when the
        // poster-pool wiring lands; no payroll today.
        Profession::Architect => &[JobKind::Build],
    }
}

/// Map a profession to the primary skill it draws on.
pub fn primary_skill_for(prof: Profession) -> Option<SkillKind> {
    match prof {
        Profession::None => None,
        Profession::Farmer => Some(SkillKind::Farming),
        Profession::Hunter => Some(SkillKind::Combat),
        Profession::Bureaucrat => Some(SkillKind::Social),
        Profession::Trader => Some(SkillKind::Trading),
        Profession::Crafter => Some(SkillKind::Crafting),
        // Phase 5b: Apprentice's primary skill is what they're
        // training toward.
        Profession::Apprentice => Some(SkillKind::Crafting),
        Profession::Healer => Some(SkillKind::Medicine),
        // sleepy-dove scaffolding: Architects rank on Building skill.
        Profession::Architect => Some(SkillKind::Building),
    }
}

/// Sum the per-day EMA across every `(kind, _)` key in `faction.wage_signal`
/// that matches one of `prof`'s job kinds. `target_rid` is wildcarded —
/// `Stockpile{wheat}` and `Stockpile{wood}` both count toward a Farmer's
/// expected wage. Returns 0.0 when no matching key has accumulated samples.
pub fn aggregate_wage_per_day(faction: &FactionData, prof: Profession) -> f32 {
    let kinds = job_kinds_for(prof);
    if kinds.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0;
    for ((kind, _rid), ema) in faction.wage_signal.iter() {
        if kinds.contains(kind) {
            sum += ema.ema_per_day;
        }
    }
    sum
}

/// Expected wage = aggregate signal × skill competence × capital factor.
/// `capital_factor` is in `[1.0, 2.0]` from `capital::capital_factor`.
pub fn expected_wage(
    faction: &FactionData,
    prof: Profession,
    skills: &Skills,
    capital_factor: f32,
) -> f32 {
    let wage = aggregate_wage_per_day(faction, prof);
    if wage <= 0.0 {
        return 0.0;
    }
    let competence = primary_skill_for(prof)
        .map(|k| skill_competence(skills.get(k)))
        .unwrap_or(1.0);
    wage * competence * capital_factor
}

/// Shared profession-demote teardown. Replaces three near-identical
/// copies in `faction_hunter_assignment_system` /
/// `chief_bureaucrat_appointment_system` / future `profession_choice_system`.
///
/// Drops any active task, releases the storage reservation, and strips
/// the `Carrying` marker so a corpse-bearing demote doesn't leave a
/// stale link. The caller still owns the `*prof = Profession::None` write
/// — this helper only handles the side-state cleanup.
pub fn demote_profession_state(
    entity: Entity,
    ai: Option<&mut PersonAI>,
    aq: Option<&mut ActionQueue>,
    reservations: &StorageReservations,
    commands: &mut Commands,
) {
    // Atomic teardown when both are present (`cancel_chain` handles both
    // fields). Otherwise fall back to whichever was available.
    match (ai, aq) {
        (Some(ai), Some(aq)) => {
            if ai.reserved_resource.is_some() {
                release_reservation(reservations, ai);
            }
            ai.target_entity = None;
            aq.cancel_chain(ai);
        }
        (Some(ai), None) => {
            if ai.reserved_resource.is_some() {
                release_reservation(reservations, ai);
            }
            ai.state = AiState::Idle;
            ai.target_entity = None;
            ai.work_progress = 0;
        }
        (None, Some(aq)) => {
            aq.cancel();
        }
        (None, None) => {}
    }
    commands
        .entity(entity)
        .remove::<crate::simulation::corpse::Carrying>();
}

/// Phase 4b hysteresis multiplier: a cross-profession switch fires only
/// when the candidate target's `expected_wage` strictly exceeds the
/// agent's `expected_wage` in their current profession by this factor.
/// The 20% band absorbs single-tick wage-signal jitter and stops agents
/// from churning between professions whose EV crosses by inches.
pub const EV_SWITCH_HYSTERESIS: f32 = 1.20;

/// Switching cost (in EV-units) applied when an agent considers leaving
/// their current profession's primary skill behind. We approximate the
/// "skill regret" term from the plan as a constant fraction of the
/// peak — switching away from a mastered skill is costlier than
/// switching away from a barely-practised one. Scoped to the switcher;
/// the per-system EV ranking inside the existing assignment loops
/// (which only compares candidates *within* one profession) is
/// unaffected.
pub const SKILL_REGRET_FRACTION: f32 = 0.20;

/// Compute the EV penalty paid for abandoning skill peaks tied to the
/// agent's current profession. Returns 0.0 for `Profession::None` /
/// `Profession::Apprentice` (no anchored peak to regret) and otherwise
/// `peak[primary_skill] × SKILL_REGRET_FRACTION × (faction wage signal
/// of the current profession) / SKILL_MAX`. Returns 0.0 when no signal
/// has accumulated for the current prof — there's nothing to regret if
/// the role pays nothing today.
pub fn switching_cost_skill_regret(
    faction: &FactionData,
    current: Profession,
    peaks: &crate::simulation::skills::SkillPeaks,
) -> f32 {
    if matches!(current, Profession::None | Profession::Apprentice) {
        return 0.0;
    }
    let Some(skill) = primary_skill_for(current) else {
        return 0.0;
    };
    let peak_norm = peaks.0[skill as usize] as f32 / SKILL_MAX as f32;
    let wage = aggregate_wage_per_day(faction, current);
    peak_norm * SKILL_REGRET_FRACTION * wage
}

/// Faction-level upper bound on the headcount of a target profession,
/// mirroring the same per-faction caps the legacy assignment systems
/// honour. Returns `None` when the target profession's faction-level
/// preconditions aren't met (gate failure) — the switcher uses `None`
/// to mean "this target is not even allowed for this faction right now."
pub fn faction_cap_for(faction: &FactionData, target: Profession) -> Option<usize> {
    use crate::simulation::faction::{
        BUREAUCRAT_MIN_RATIO, CRAFTER_MAX_DIVISOR, FARMER_SURVIVAL_FLOOR,
    };
    use crate::simulation::technology::HUNTING_SPEAR;
    let adults = faction.member_count as usize;
    if adults == 0 {
        return None;
    }
    let per_head = faction.storage.food_total() / faction.member_count as f32;
    let survival = per_head < FARMER_SURVIVAL_FLOOR;
    if survival {
        // Under survival override, only Farmer is welcome. Switching
        // *into* Hunter / Bureaucrat / Crafter is locked out.
        return match target {
            Profession::Farmer => Some(adults),
            _ => None,
        };
    }
    match target {
        Profession::Hunter => {
            if !faction.techs.has(HUNTING_SPEAR) {
                return None;
            }
            Some(adults / 2)
        }
        Profession::Bureaucrat => {
            if !faction.state_funds_public_works {
                return None;
            }
            // Min 1 if the ratio is non-zero — mirrors the legacy
            // `max(1, …)` floor in the bureaucrat assignment system.
            Some(
                ((adults as f32 * BUREAUCRAT_MIN_RATIO).round() as usize)
                    .max(1)
                    .min(adults),
            )
        }
        Profession::Crafter => Some(adults / CRAFTER_MAX_DIVISOR),
        Profession::Farmer => Some(adults),
        Profession::Trader => None,
        Profession::None | Profession::Apprentice => None,
        // Phase 5b-stretch: Healer cap mirrors Crafter (skilled-service
        // ceiling). Auto-promotion path doesn't exist yet, but the cap
        // is in place for when the Heal-job pipeline ships and the
        // switcher / inspector EV table evaluate Healer as a target.
        Profession::Healer => Some(adults / CRAFTER_MAX_DIVISOR),
        // sleepy-dove scaffolding: no faction-wide cap. Gating is
        // per-settlement (one architect per settlement that needs one);
        // returning None until the appointment system ships keeps the
        // cross-profession switcher from promoting into Architect.
        Profession::Architect => None,
    }
}

/// Shadow record of an agent's last-observed profession. Maintained by
/// `profession_change_log_system` so it can emit an
/// `ActivityEntryKind::ProfessionChanged` event on every real transition
/// without each promote/demote site needing to write a log entry inline.
/// Apprenticeship-specific transitions (None → Apprentice → Crafter) are
/// silenced here — the apprenticeship lifecycle emits its own dedicated
/// events with mentor info attached, so this log line would be a noisy
/// duplicate.
#[derive(Component, Clone, Copy, Debug)]
pub struct LastSeenProfession(pub Profession);

/// Centralised profession-change activity-log emitter. Runs in Economy
/// after the four profession-mutation systems (`faction_profession_system`,
/// `faction_hunter_assignment_system`, `chief_bureaucrat_appointment_system`,
/// `chief_craft_assignment_system`) and after `apprentice_progress_system`,
/// so a same-tick promote-then-graduate transition lands as a single
/// final log entry. The shadow `LastSeenProfession` component is lazily
/// inserted the first time an agent's profession is read.
pub fn profession_change_log_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut q: Query<
        (
            Entity,
            &Profession,
            &FactionMember,
            Option<&mut LastSeenProfession>,
        ),
        Changed<Profession>,
    >,
    mut events: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
) {
    let now = clock.tick;
    for (entity, prof, member, last_opt) in q.iter_mut() {
        let from = last_opt.as_ref().map(|l| l.0).unwrap_or(Profession::None);
        if from == *prof {
            // First-tick spawn or no-op re-write — refresh shadow and skip.
            if let Some(mut last) = last_opt {
                last.0 = *prof;
            } else {
                commands.entity(entity).insert(LastSeenProfession(*prof));
            }
            continue;
        }
        // Apprenticeship transitions get their own log entries with
        // mentor info — suppress the generic ProfessionChanged here so
        // the log doesn't double up.
        let suppress_apprenticeship_dup = matches!(*prof, Profession::Apprentice)
            || matches!(
                (from, *prof),
                (Profession::Apprentice, Profession::Crafter)
                    | (Profession::Apprentice, Profession::Healer)
                    | (Profession::Apprentice, Profession::None)
            );
        if !suppress_apprenticeship_dup {
            events.send(crate::ui::activity_log::ActivityLogEvent {
                tick: now,
                actor: entity,
                faction_id: member.faction_id,
                kind: crate::ui::activity_log::ActivityEntryKind::ProfessionChanged {
                    from,
                    to: *prof,
                },
            });
        }
        if let Some(mut last) = last_opt {
            last.0 = *prof;
        } else {
            commands.entity(entity).insert(LastSeenProfession(*prof));
        }
    }
}

/// Phase 4b unified cross-profession switcher with per-agent EV
/// hysteresis. Replaces the *missing* X → Y transition path — the
/// existing four assignment systems handle None ↔ X (promote from
/// idle / demote to idle), but a fully-employed Hunter who would
/// genuinely earn more as a Crafter has no path today except a
/// round-trip through None across two cadence cycles.
///
/// Daily pass:
/// - For each employed agent (Hunter / Bureaucrat / Crafter — Farmer
///   is excluded because food/head dominates its assignment;
///   Trader has no chief-driven counterpart; Apprentice is bound to a
///   mentor and shouldn't switch out mid-training):
///   - Compute `EV(current)` and `EV(target)` for every other candidate
///     profession via `expected_wage` (which already folds in capital).
///   - Pick the best alternative whose EV exceeds
///     `EV(current) × EV_SWITCH_HYSTERESIS` and whose faction cap still
///     has room (current headcount of target < `faction_cap_for`).
///   - Apply switching cost via `switching_cost_skill_regret(faction,
///     current, peaks)` — a high-peak Hunter pays a real EV penalty to
///     leave Combat behind.
///   - On accept: tear down current profession via
///     `demote_profession_state`, then set the new profession. If the
///     target is `Crafter` and `Skills[Crafting] < APPRENTICE_THRESHOLD`,
///     route through `Profession::Apprentice` with a same-faction
///     mentor (mirroring `chief_craft_assignment_system`).
#[allow(clippy::too_many_arguments)]
pub fn cross_profession_switch_system(
    clock: Res<SimClock>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
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
        &crate::simulation::skills::SkillPeaks,
        &crate::economy::agent::EconomicAgent,
        &crate::simulation::carry::Carrier,
        &Transform,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        Option<&mut PersonAI>,
        Option<&mut ActionQueue>,
    )>,
) {
    if clock.tick % crate::world::seasons::TICKS_PER_DAY as u64 != 0 {
        return;
    }

    // Pre-pass: per-(faction, profession) headcounts + Crafter mentor pool.
    let mut counts: crate::collections::AHashMap<(u32, Profession), usize> = crate::collections::AHashMap::default();
    let mut available_mentors: crate::collections::AHashMap<u32, Vec<Entity>> = crate::collections::AHashMap::default();
    for (entity, prof, member, skills, _peaks, _, _, _, _, _, _) in query.iter() {
        if member.faction_id == crate::simulation::faction::SOLO {
            continue;
        }
        *counts.entry((member.faction_id, *prof)).or_insert(0) += 1;
        if *prof == Profession::Crafter {
            let crafting = skills.0[crate::simulation::skills::SkillKind::Crafting as usize];
            if crafting >= crate::simulation::apprenticeship::MASTER_THRESHOLD
                && mentors_q.get(entity).is_err()
            {
                available_mentors
                    .entry(member.faction_id)
                    .or_default()
                    .push(entity);
            }
        }
    }

    // Candidate target professions for the switcher. Farmer is excluded
    // (assignment is food-dominated, not wage-dominated). Trader has
    // no chief-driven assignment loop today. Apprentice is a transient
    // training role — switchable to only via the Crafter apprenticeship
    // branch below.
    const CANDIDATES: [Profession; 3] = [
        Profession::Hunter,
        Profession::Bureaucrat,
        Profession::Crafter,
    ];

    let mut planned: Vec<(Entity, Profession)> = Vec::new();
    let mut planned_apprentice: Vec<(Entity, Entity)> = Vec::new();

    for (entity, prof, member, skills, peaks, agent, carrier, xf, household, _, _) in query.iter() {
        let fid = member.faction_id;
        if fid == crate::simulation::faction::SOLO {
            continue;
        }
        // Only switch agents who are actively employed in one of the
        // candidate professions. The existing per-profession systems
        // still handle None ↔ X transitions.
        if !CANDIDATES.contains(prof) {
            continue;
        }
        let Some(faction) = registry.factions.get(&fid) else {
            continue;
        };
        let agent_tile = crate::world::terrain::world_to_tile(xf.translation.truncate());

        let cap_current = crate::simulation::capital::capital_factor(
            agent,
            carrier,
            agent_tile,
            fid,
            household,
            *prof,
            &ownership,
            &plots,
            &plot_index,
        );
        let ev_current = expected_wage(faction, *prof, skills, cap_current);
        let regret = switching_cost_skill_regret(faction, *prof, peaks);

        let mut best: Option<(Profession, f32)> = None;
        for &target in &CANDIDATES {
            if target == *prof {
                continue;
            }
            let Some(cap_target) = faction_cap_for(faction, target) else {
                continue;
            };
            let cur_count = counts.get(&(fid, target)).copied().unwrap_or(0);
            if cur_count >= cap_target {
                continue;
            }
            let cap = crate::simulation::capital::capital_factor(
                agent,
                carrier,
                agent_tile,
                fid,
                household,
                target,
                &ownership,
                &plots,
                &plot_index,
            );
            let ev = expected_wage(faction, target, skills, cap) - regret;
            if ev <= 0.0 {
                continue;
            }
            if ev > ev_current * EV_SWITCH_HYSTERESIS && best.map(|(_, b)| ev > b).unwrap_or(true) {
                best = Some((target, ev));
            }
        }

        if let Some((target, _)) = best {
            // Apprenticeship gate for Crafter switches with low skill.
            if target == Profession::Crafter {
                let crafting = skills.0[crate::simulation::skills::SkillKind::Crafting as usize];
                if crafting < crate::simulation::apprenticeship::APPRENTICE_THRESHOLD {
                    if let Some(pool) = available_mentors.get_mut(&fid) {
                        if let Some(mentor) = pool.pop() {
                            planned.push((entity, Profession::Apprentice));
                            planned_apprentice.push((entity, mentor));
                            *counts.entry((fid, Profession::Apprentice)).or_insert(0) += 1;
                            *counts.entry((fid, *prof)).or_insert(1) -= 1;
                            continue;
                        }
                    }
                }
            }
            planned.push((entity, target));
            *counts.entry((fid, target)).or_insert(0) += 1;
            *counts.entry((fid, *prof)).or_insert(1) -= 1;
        }
    }

    if planned.is_empty() {
        return;
    }

    let plan_set: crate::collections::AHashSet<Entity> = planned.iter().map(|(e, _)| *e).collect();
    let mut new_prof: crate::collections::AHashMap<Entity, Profession> = crate::collections::AHashMap::default();
    for (e, p) in &planned {
        new_prof.insert(*e, *p);
    }

    for (entity, mut prof, _member, _skills, _peaks, _, _, _, _, ai_opt, aq_opt) in query.iter_mut()
    {
        if !plan_set.contains(&entity) {
            continue;
        }
        let to = *new_prof.get(&entity).expect("plan_set ⊂ new_prof");
        demote_profession_state(
            entity,
            ai_opt.map(|x| x.into_inner()),
            aq_opt.map(|x| x.into_inner()),
            &reservations,
            &mut commands,
        );
        *prof = to;
        // `ProfessionChanged` is emitted centrally by
        // `profession_change_log_system` via its `Changed<Profession>`
        // observer; the explicit `ApprenticeshipStarted` send below
        // covers the (suppressed) X → Apprentice case.
    }

    // Bind apprenticeship links + emit ApprenticeshipStarted events.
    for (entity, mentor) in planned_apprentice {
        commands
            .entity(entity)
            .insert(crate::simulation::apprenticeship::ApprenticeOf { mentor })
            .insert(crate::simulation::apprenticeship::ApprenticeProgress::default());
        commands
            .entity(mentor)
            .insert(crate::simulation::apprenticeship::MentorOf { apprentice: entity });
        let fid = query
            .get(entity)
            .map(|(_, _, m, _, _, _, _, _, _, _, _)| m.faction_id)
            .unwrap_or(crate::simulation::faction::SOLO);
        activity.send(crate::ui::activity_log::ActivityLogEvent {
            tick: clock.tick,
            actor: entity,
            faction_id: fid,
            kind: crate::ui::activity_log::ActivityEntryKind::ApprenticeshipStarted { mentor },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_competence_bounds() {
        assert!((skill_competence(0) - 0.2).abs() < 1e-6);
        assert!((skill_competence(SKILL_MAX) - 1.0).abs() < 1e-6);
        // Saturating clamp: values above SKILL_MAX cap out at 1.0.
        assert!((skill_competence(SKILL_MAX + 100) - 1.0).abs() < 1e-6);
        let mid = skill_competence(SKILL_MAX / 2);
        assert!(mid > 0.55 && mid < 0.65);
    }

    #[test]
    fn job_kinds_per_profession() {
        assert!(job_kinds_for(Profession::None).is_empty());
        assert!(job_kinds_for(Profession::Farmer).contains(&JobKind::Farm));
        assert!(job_kinds_for(Profession::Hunter).contains(&JobKind::Stockpile));
        assert!(job_kinds_for(Profession::Bureaucrat).contains(&JobKind::Build));
        // Phase 5a: Crafter draws wages from Craft postings primarily.
        assert!(job_kinds_for(Profession::Crafter).contains(&JobKind::Craft));
        assert!(job_kinds_for(Profession::Crafter).contains(&JobKind::Stockpile));
    }

    #[test]
    fn primary_skill_per_profession() {
        assert_eq!(
            primary_skill_for(Profession::Farmer),
            Some(SkillKind::Farming)
        );
        assert_eq!(
            primary_skill_for(Profession::Hunter),
            Some(SkillKind::Combat)
        );
        assert_eq!(primary_skill_for(Profession::None), None);
        assert_eq!(
            primary_skill_for(Profession::Crafter),
            Some(SkillKind::Crafting)
        );
    }

    #[test]
    fn expected_wage_scales_linearly_with_capital() {
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{JobKind, WageEMA};
        let mut registry = FactionRegistry::default();
        let fid = registry.create_faction((0, 0));
        let faction = registry.factions.get_mut(&fid).unwrap();
        faction.wage_signal.insert(
            (JobKind::Stockpile, None),
            WageEMA {
                ema_per_day: 10.0,
                last_update_tick: 0,
                samples: 1,
            },
        );
        let mut skills = Skills::default();
        skills.0[SkillKind::Combat as usize] = SKILL_MAX; // 1.0 competence
        let base = expected_wage(faction, Profession::Hunter, &skills, 1.0);
        let with_tool = expected_wage(faction, Profession::Hunter, &skills, 1.5);
        assert!(base > 0.0);
        assert!((with_tool / base - 1.5).abs() < 1e-3);
    }

    #[test]
    fn expected_wage_zero_when_signal_empty() {
        use crate::simulation::faction::FactionRegistry;
        let mut registry = FactionRegistry::default();
        let fid = registry.create_faction((0, 0));
        let faction = registry.factions.get(&fid).unwrap();
        let skills = Skills::default();
        // No wage_signal entries → wage is zero regardless of capital.
        let ev = expected_wage(faction, Profession::Hunter, &skills, 2.0);
        assert_eq!(ev, 0.0);
    }
}
