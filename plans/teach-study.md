# Teach / Study Pipeline

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 3b.
**Depends on:** Phases A–E of parent plan. Follow-up 3a (heal pipeline) recommended as the pattern-validating first domain.

## Trigger

Pick up after Heal pipeline (Follow-up 3a) lands. Apprenticeship already exists in tree (`ApprenticeOf` / `MentorOf` in `apprenticeship.rs`); extend to formal schooling so Curiosity Disposition has a meaningful behavioural lever.

## Scope

Formalise teaching beyond the existing 1-on-1 apprentice pattern. Add `WorkshopKind::School`, `Profession::Scholar`, `JobKind::Teach`, `JobKind::Study`. Curiosity (Disposition axis) drives `StudyScorer`.

## Current state

- `ApprenticeOf { mentor }` / `MentorOf { apprentice }` paired components in `src/simulation/apprenticeship.rs`.
- `ApprenticeProgress { ticks, target_profession }` graduates apprentices.
- `xp_with_apprentice_bonus` already grants 2× Crafting XP to apprentices.
- No School workshop, no Scholar profession, no group-teaching mechanic.

## Files to touch

- `src/simulation/workshop.rs` (or wherever `WorkshopKind` lives) — add `School` variant.
- `src/simulation/profession.rs` — add `Profession::Scholar` (primary skill = `Literacy` or `Scholarship`, new `SkillKind`).
- `src/simulation/skills.rs` — add `SkillKind::Literacy` (gated on tech).
- `src/simulation/jobs.rs` — `JobKind::Teach { school, subject_skill }`, `JobKind::Study { school, subject_skill }`.
- `src/simulation/goal_scorers.rs`:
  - `StudyScorer` (Esteem class): `school_proximity × (1 + curiosity/255 × 1.5) × (1 - current_skill/SKILL_MAX)`.
  - `TeachScorer` (Subsistence class): for Scholars, group-teaching utility scales with N students present.
- `src/simulation/apprenticeship.rs` — generalise to support group-teaching XP grants.

## Open questions a real plan must resolve

- **Literacy as tech gate.** Recommend yes — gate `School` workshop on a `WRITING` or `LITERACY` tech.
- **Subject coverage.** Schools teach which skills? Just Literacy + Scholarship, or any skill the Scholar has at Mastery? Recommend: Scholar's mastered skills only.
- **Cohort-aware.** Children-only studies? Or any agent? Recommend: prioritise children but allow adults.
- **XP mechanism.** Reuse `xp_with_apprentice_bonus` (2× during study) or new multiplier? Reuse for simplicity.
- **Class size limit.** Cap students-per-Scholar to avoid degenerate cases.
- **Time-of-day.** Study during daytime only? Anchor on `TimeOfDay`.

## Acceptance criteria

- Curious agents preferentially seek out `School` workshops to study.
- Scholar profession auto-assigns via `chief_scholar_assignment_system`.
- Students gain XP in the subject skill at accelerated rate while in study session.
- Literacy tech gate prevents Schools from spawning pre-tech.
- Calibration test: same-disposition curious vs incurious agents have measurably different `Literacy` skill after a game-year.
