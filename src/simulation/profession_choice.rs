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
use crate::simulation::faction::{release_reservation, FactionData, StorageReservations};
use crate::simulation::jobs::JobKind;
use crate::simulation::person::{AiState, PersonAI, Profession};
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
    if let Some(ai) = ai {
        if ai.reserved_resource.is_some() {
            release_reservation(reservations, ai);
        }
        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.target_entity = None;
        ai.work_progress = 0;
    }
    if let Some(aq) = aq {
        aq.cancel();
    }
    commands
        .entity(entity)
        .remove::<crate::simulation::corpse::Carrying>();
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
    }
}
