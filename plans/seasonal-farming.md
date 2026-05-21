# Seasonal Farming Priority Fix

## Summary
Make in-season farming a true food-security priority, not just another low-share communal job. Spring prep/planting and autumn harvest should outrank normal chief construction, hauling, and crafting, while still yielding to player orders and acute survival food-gathering.

## Key Changes
- Move shared annual farming constants into `farm.rs` and reuse them from settlement sizing and priority code.
- Replace `farm_pressure`’s tiny `grain >= members * 4` target with annual food-need pressure based on `GRAIN_PER_PERSON_PER_YEAR` and the existing safety margin.
- Give open seasonal `FieldWork` postings a high chief priority, capped below `PLAYER_PRIORITY`; when `food_pressure >= 80`, emergency calorie stockpile stays above farm work.
- In `job_claim_system`, add an effective Farm cap floor for open seasonal `FieldWork` so roughly 60% of workers can claim spring/autumn farm jobs when not in acute food crisis. Keep assigned summer caretaker work and Plow on the normal farm budget.
- Make Farm dispatch phase-correct:
  - Prepare claims only run `PrepareField`.
  - Plant claims only run planting.
  - Harvest claims only run harvest.
  - Private/no-claim household farming remains autonomous and seasonal-gated by `FarmWorkScorer`.
- Add a phase-aware Farm progress helper so Prepare/Plant/Harvest postings only increment from matching work; wire harvest completion through `gather_system` so harvest postings actually complete.

## Public Interfaces
- Add/rehome public farm planning constants in `src/simulation/farm.rs`.
- Add a small jobs helper for phase-aware `FieldWork` progress recording.
- No new crates, no save/schema migration, no new ECS component required.

## Tests
- Unit tests for annual farm pressure, seasonal Farm priority, and critical-food override.
- Job-claim regression: seasonal farm backlog attracts the intended worker share despite open Build/Haul/Craft jobs; player postings still win.
- Dispatch/progress regressions: Prepare claims cannot plant/harvest, Plant claims cannot satisfy Prepare, Harvest claims increment and complete on mature crop harvest.
- Run `cargo test --bin civgame` targeted farm/job tests, then the full binary test suite if practical.

## Docs
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` to describe seasonal hard-priority farming, the annual pressure target, and phase-correct Farm progress.
