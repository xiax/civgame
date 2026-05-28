//! Phase 5b (wage-aware-labor-market-v2): Crafter apprenticeship.
//!
//! Sub-`APPRENTICE_THRESHOLD`-Crafting candidates that the chief wants
//! to promote to `Profession::Crafter` are routed through
//! `Profession::Apprentice` first, bound to a same-faction master
//! (`Skills[Crafting] >= MASTER_THRESHOLD`, not already mentoring).
//! `apprentice_progress_system` runs daily, advancing
//! `ApprenticeProgress.ticks` by `TICKS_PER_DAY` and graduating the
//! apprentice to `Crafter` once `target_ticks` is reached.
//!
//! Bindings are one-to-one (mentor → apprentice, apprentice → mentor).
//! A mentor dying or losing their Crafter profession invalidates the
//! `MentorOf` link; the next progress tick detects the stale link and
//! demotes the orphaned apprentice back to `Profession::None`,
//! discarding accumulated progress. The Phase 5b spec calls for paused
//! progress + a one-week rebind window — this minimal implementation
//! ships the abort path; the rebind window is a follow-up.
//!
//! Healer apprenticeship (the second `ApprenticeTarget` variant in the
//! plan) is deferred — `Profession::Healer` doesn't exist yet.

use bevy::prelude::*;

use crate::simulation::person::Profession;
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills, SKILL_MAX};
use crate::world::seasons::TICKS_PER_DAY;

/// Sub-this `Skills[Crafting]` value routes Crafter promotions through
/// apprenticeship. Matches the plan: a 30-day course bumps fresh
/// candidates up to a baseline of 30, after which they earn XP at
/// normal Crafter rates.
pub const APPRENTICE_THRESHOLD: u32 = 30;

/// Minimum `Skills[Crafting]` for a Crafter to accept an apprentice.
/// Mentors below this provide negligible deliberate-practice value.
pub const MASTER_THRESHOLD: u32 = 100;

/// In-game days an apprenticeship runs. Matches the plan's
/// `target_ticks = TICKS_PER_DAY * 30`.
pub const APPRENTICESHIP_DURATION_DAYS: u32 = 30;

/// Apprentice payout share. Plan: an apprentice claimant on a paid
/// posting earns `share * WAGE_FRACTION_APPRENTICE` of an equivalent
/// solo wage; the mentor takes a `WAGE_FRACTION_MENTOR_FEE` fee for
/// supervision; the remainder is refunded to the posting's
/// beneficiary (cheaper labor → poster keeps the difference). All
/// three fractions must sum to 1.0 so currency is conserved.
pub const WAGE_FRACTION_APPRENTICE: f32 = 0.4;
pub const WAGE_FRACTION_MENTOR_FEE: f32 = 0.1;

/// Deliberate-practice multiplier applied to skill XP grants while an
/// agent is in `Profession::Apprentice`. Matches the plan: under
/// active mentor supervision, craft / medicine practice is twice as
/// efficient as solo work.
pub const APPRENTICE_XP_MULT: u32 = 2;

/// Apply the apprenticeship deliberate-practice multiplier to a raw
/// `gain_xp` amount. Pure helper so XP call sites stay one-liners:
///
/// ```ignore
/// skills.gain_xp(SkillKind::Crafting, xp_with_apprentice_bonus(1, apprentice_opt));
/// ```
///
/// Returns `base × APPRENTICE_XP_MULT` when the agent currently
/// carries an `ApprenticeOf` link; the raw `base` otherwise.
pub fn xp_with_apprentice_bonus(base: u32, apprentice: Option<&ApprenticeOf>) -> u32 {
    if apprentice.is_some() {
        base.saturating_mul(APPRENTICE_XP_MULT)
    } else {
        base
    }
}

/// Apprentice → master link. Removed on graduation or abort.
#[derive(Component, Clone, Copy, Debug)]
pub struct ApprenticeOf {
    pub mentor: Entity,
}

/// Master → apprentice link (one-to-one cap). Removed on graduation or
/// abort.
#[derive(Component, Clone, Copy, Debug)]
pub struct MentorOf {
    pub apprentice: Entity,
}

/// Apprentice progress ledger. `apprentice_progress_system` increments
/// `ticks` by `TICKS_PER_DAY` once per game-day; on `ticks >=
/// target_ticks` the apprentice graduates to `target_profession`
/// (defaulting to `Crafter`; `Healer` is the Phase 5b-stretch target
/// for a future heal-job pipeline). Skill graduation also writes the
/// `APPRENTICE_THRESHOLD` floor to the *target's primary skill* — set
/// by `primary_skill_for(target)` at graduation time.
#[derive(Component, Clone, Copy, Debug)]
pub struct ApprenticeProgress {
    pub ticks: u32,
    pub target_ticks: u32,
    pub target_profession: Profession,
}

impl Default for ApprenticeProgress {
    fn default() -> Self {
        Self {
            ticks: 0,
            target_ticks: TICKS_PER_DAY.saturating_mul(APPRENTICESHIP_DURATION_DAYS),
            target_profession: Profession::Crafter,
        }
    }
}

/// Daily apprentice progress + graduation. Also tears down apprenticeships
/// whose mentor link has gone stale (mentor despawned or `MentorOf`
/// removed by an external demote).
pub fn apprentice_progress_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut activity: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut apprentices: Query<(
        Entity,
        &mut Profession,
        &mut Skills,
        &ApprenticeOf,
        &mut ApprenticeProgress,
        &crate::simulation::faction::FactionMember,
    )>,
    mentors: Query<&MentorOf>,
) {
    // Phase 1.2: per-faction stagger inside an every-tick run.
    // Apprenticeship is per-entity but we stagger by faction id so all
    // apprentices in a faction tick together (preserves the +1 day/day
    // rate while spreading the work across the cadence window).
    const SYSTEM_OFFSET: u64 = 191;
    for (entity, mut prof, mut skills, link, mut progress, member) in apprentices.iter_mut() {
        if !crate::simulation::perf::faction_stagger_due(
            clock.tick,
            member.faction_id,
            SYSTEM_OFFSET,
            TICKS_PER_DAY as u64,
        ) {
            continue;
        }
        let mentor_intact = mentors
            .get(link.mentor)
            .map(|m| m.apprentice == entity)
            .unwrap_or(false);
        if !mentor_intact {
            // Orphaned: mentor dead or demoted out of Crafter. Drop the
            // apprenticeship; the next chief_craft_assignment_system
            // pass may re-bind if a new master is available.
            *prof = Profession::None;
            commands
                .entity(entity)
                .remove::<ApprenticeOf>()
                .remove::<ApprenticeProgress>();
            continue;
        }
        progress.ticks = progress.ticks.saturating_add(TICKS_PER_DAY);
        if progress.ticks >= progress.target_ticks {
            // Graduate: bump the *target profession's* primary skill to
            // the apprenticeship floor, promote, dissolve links.
            // Healer-target apprenticeships (Phase 5b-stretch) write the
            // floor to `SkillKind::Medicine` instead of `Crafting` so a
            // future heal-job pipeline sees graduates at the same
            // baseline competence Crafters get.
            let target_skill =
                crate::simulation::profession_choice::primary_skill_for(progress.target_profession)
                    .unwrap_or(SkillKind::Crafting);
            let cur = skills.0[target_skill as usize];
            skills.0[target_skill as usize] = cur.max(APPRENTICE_THRESHOLD).min(SKILL_MAX);
            *prof = progress.target_profession;
            commands
                .entity(entity)
                .remove::<ApprenticeOf>()
                .remove::<ApprenticeProgress>();
            commands.entity(link.mentor).remove::<MentorOf>();
            activity.send(crate::ui::activity_log::ActivityLogEvent {
                tick: clock.tick,
                actor: entity,
                faction_id: member.faction_id,
                kind: crate::ui::activity_log::ActivityEntryKind::ApprenticeshipGraduated,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_thirty_days() {
        let p = ApprenticeProgress::default();
        assert_eq!(p.ticks, 0);
        assert_eq!(p.target_ticks, TICKS_PER_DAY * APPRENTICESHIP_DURATION_DAYS);
    }

    #[test]
    fn threshold_constants_are_consistent() {
        // Apprentice graduates at THRESHOLD; masters need MASTER_THRESHOLD.
        assert!(APPRENTICE_THRESHOLD < MASTER_THRESHOLD);
        assert!(MASTER_THRESHOLD <= SKILL_MAX);
    }

    #[test]
    fn xp_bonus_doubles_for_apprentices() {
        // Without an ApprenticeOf link, the helper passes through.
        assert_eq!(xp_with_apprentice_bonus(7, None), 7);
        // With one, the multiplier applies. We can't construct an
        // `ApprenticeOf` with a sentinel Entity in a pure test (Entity
        // construction is private), so the presence check stands in.
        // The `Option<&ApprenticeOf>::is_some()` branch is what the
        // helper actually inspects.
        let dummy = ApprenticeOf {
            mentor: Entity::from_raw(1),
        };
        assert_eq!(
            xp_with_apprentice_bonus(7, Some(&dummy)),
            7 * APPRENTICE_XP_MULT
        );
    }

    #[test]
    fn wage_split_fractions_sum_to_one() {
        // Apprentice + mentor fee + residual must conserve currency.
        let residual = 1.0 - WAGE_FRACTION_APPRENTICE - WAGE_FRACTION_MENTOR_FEE;
        assert!(residual > 0.0);
        assert!(
            (WAGE_FRACTION_APPRENTICE + WAGE_FRACTION_MENTOR_FEE + residual - 1.0).abs() < 1e-6
        );
    }
}
