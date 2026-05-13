# Civic / Public Works / Military

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 3d.
**Depends on:** Phases A–E of parent plan.

## Trigger

Pick up after Heal + Teach + Trade prove the modern-age domain pattern is solid. Civic touches the most existing systems (bureaucrats, treasury, raids, drafted) so it's last among the domain follow-ups.

## Scope

Add paid-military and civic-enforcement professions distinct from the current ad-hoc `Drafted` + `chief_bureaucrat_appointment_system` setup. Martial Disposition gets a real per-agent lever beyond raid participation.

**Recommended phasing inside this follow-up:**
1. Paid-soldier path first (cleanest extension — raids + drafting already exist).
2. Magistrate / law-enforcement second.
3. Crime model + public-order metrics last (largest design space).

## Current state

- `Drafted` component — chief-mandated combat participation.
- `chief_bureaucrat_appointment_system` — paid public-works role exists.
- `state_funds_public_works` tech gates bureaucrat promotion.
- Raids handled by `is_under_raid` / `raid_target` faction state.
- **Verify:** does `Profession::Soldier` exist? Probably no — currently combat is goal-driven (`Raid`, `Defend`) rather than profession-driven.

## Files to touch

**Phase 1 (Soldier):**
- `src/simulation/profession.rs` — add `Profession::Soldier` (primary skill = `Combat`).
- `src/simulation/jobs.rs` — `JobKind::Patrol`, `JobKind::GuardPost`.
- `src/simulation/goal_scorers.rs`:
  - `SoldierDutyScorer` (Subsistence class): for Soldiers, scores patrol/guard contracts × `(1 + martial/255 × 1.5)`.
- `src/simulation/faction.rs` — `chief_soldier_assignment_system` mirroring bureaucrat pattern (treasury-gated, survival override).
- `src/simulation/htn.rs` — new methods `Patrol`, `GuardPost`. Reuse existing `Defend` / `Raid` methods for combat ops.

**Phase 2 (Magistrate):**
- `src/simulation/profession.rs` — `Profession::Magistrate`.
- `src/simulation/jobs.rs` — `JobKind::Enforce`, `JobKind::JudgeDispute`.
- `src/simulation/crime.rs` (new) — `Crime { perpetrator, victim, kind, severity, witnesses }`.
- `src/simulation/goal_scorers.rs` — `MagistrateScorer`.

**Phase 3 (Public order):**
- `src/simulation/faction.rs` — `PublicOrder` metric on `FactionData`; affects mood / migration pressure / spawning.

## Open questions a real plan must resolve

- **Martial vs Drafted overlap.** Is `Drafted` superseded by `Profession::Soldier`, or do both coexist? Recommend: `Drafted` for emergency raid response, `Soldier` for permanent paid force.
- **Crime model.** Simplest: theft only (item-from-storage), violence only (attack-without-raid), or both? Defer to Phase 3.
- **Magistrate output.** Verdict → punishment → effect on perpetrator's relationships / faction standing. Define the loop.
- **Tech gates.** `STATE_FUNDS_PUBLIC_WORKS` exists; add `RULE_OF_LAW`, `STANDING_ARMY` techs?
- **Public-order feedback.** Crimes lower order, magistrates raise order, soldiers raise order — how does order affect gameplay (mood multiplier, raid attractiveness, settlement growth)?
- **Player intent.** Most ambitious of the modern-age domains — check with user before committing to crime model scope.

## Acceptance criteria (per phase)

- **P1:** Martial agents auto-promote to Soldier when faction has treasury + raid history. Soldiers run patrol routes. Calibration: martial vs non-martial promotion rate differs.
- **P2:** Magistrate adjudicates a synthetic theft scenario end-to-end.
- **P3:** PublicOrder metric responds to soldier headcount + crime rate + magistrate verdicts.
