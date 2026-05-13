# Heal Pipeline

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 3a.
**Depends on:** Phases A–E of parent plan (proves the pattern). Optional: Follow-up 2 (opportunity-producers) for cleaner integration.

## Trigger

Pick this up as the **first** modern-age domain after behavioural-richness lands — `Profession::Healer` scaffolding already exists, so it's the lowest-friction proof of "one scorer + one HTN method per role + one assignment system + one job-kind" pattern.

## Scope

Add a paid heal-job pipeline so injured agents seek care and Healers respond. Today `Profession::Healer` exists in `profession_choice` (primary skill = `Medicine`, workshop-affine to `Shrine`, EV table surfaces it) but there's no assignment system, no `JobKind::Heal`, no patient-side scorer.

## Current state (from survey + spot-checks needed)

- `Profession::Healer` recognized; `Skills::Medicine` exists.
- `Shrine` workshop kind already affine.
- **Verify:** does `Injury` / `Wound` component exist? Search `src/simulation/combat.rs`, `src/simulation/person.rs`.
- **Verify:** does `ResourceCatalog` have a medicine / herb resource? Check `assets/` or `src/world/resource_catalog.rs`.

## Files to touch

- New or existing `src/simulation/medicine.rs` — `Injury { severity, type, applied_tick }` component; `injury_progression_system` (untreated injuries worsen, treated injuries heal).
- `src/simulation/jobs.rs` — add `JobKind::Heal { target: Entity, severity_at_post }`.
- `src/simulation/goal_scorers.rs`:
  - `HealNeedScorer` (Safety class): score from `injury.severity × distance_to_nearest_healer`; gates `AgentGoal::SeekCare` (new variant).
  - `ProvideCareScorer` (Subsistence class): for Healers, score from nearest patient × `prof_affinity(Healer)` × wage signal.
- `src/simulation/faction.rs` — `chief_healer_assignment_system` mirroring `chief_bureaucrat_appointment_system` + survival override + demote buffer.
- `src/simulation/htn.rs` — new methods `SeekCare`, `ProvideCare`. New `AbstractTask::HealPatient`.
- `src/simulation/goals.rs` — add `AgentGoal::SeekCare`, `AgentGoal::ProvideCare`.
- `src/ui/inspector.rs` — surface injury state + nearest healer in agent inspector.

## Open questions a real plan must resolve

- **Injury source.** Combat-only, or also disease / accident / starvation? Start combat-only.
- **Medicine resource.** Required input for healing, or skill-only? If resource-gated, what's the recipe?
- **Triage rules.** Severe-first vs proximity-first vs household-first? Suggest severity-weighted distance: `score = severity / (1 + distance/8)`.
- **Healer compensation.** Patient-paid (like a contract) vs faction-paid (chief posts via `chief_healer_assignment_system`)? Probably faction-paid in subsistence/mixed, patient-paid in market.
- **Healing duration.** Fixed ticks per severity unit? Skill-dependent (`Medicine` skill reduces time)?
- **XP grant.** Where does `Medicine` XP come from? Per-tick during heal + bonus on patient recovery.
- **Apprentice path.** Healer-apprentice already supported by `ApprenticeProgress.target_profession` per parent CLAUDE.md — verify and reuse.

## Acceptance criteria

- Injured agent transitions to `AgentGoal::SeekCare` and walks to nearest Healer (or Shrine).
- Healer auto-promotes when faction has injured + treasury (mixed/market) or always (subsistence).
- Healing reduces `Injury.severity` over time; agent returns to normal goals when healed.
- Apprentice Healer gains `Medicine` XP at 2× rate (via existing `xp_with_apprentice_bonus`).
- Inspector shows injury / healer state.
- Calibration test: 3 injured agents + 1 Healer recover within N days; without a Healer they degrade.
