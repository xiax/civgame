# Balanced Spring Farming Fix

## Summary
Fix Spring farming so preparation cannot monopolize the planting window. When both `Prepare` and `Plant` fieldwork are available, reserve a phase split inside the Farm worker pool: **60% Plant / 40% Prepare**, biased toward planting because missed planting windows cannot be recovered until next year.

## Key Changes
- Add internal farming constants/helper for Spring phase claim caps:
  - Plant gets `ceil(total_spring_farm_cap * 0.60)`.
  - Prepare gets the remaining share, with at least 1 preparer only when total cap allows it.
  - If no Plant posting exists, Prepare may use the full Farm cap as today.
- Update `job_claim_system` to track active `FieldWork` claims by `FarmWorkPhase` and enforce the split only for Spring `Prepare` + `Plant` postings.
- Keep existing seasonal priority and posting behavior, but prevent Prepare postings from filling the shared Farm bucket before Plant workers can claim.
- Include `FarmWorkPhase` in same-target comparisons for `FieldWork` so Prepare and Plant are not treated as interchangeable work on the same plot.
- Optionally tighten chief Plant target emission to spend a per-faction seed budget across plots, avoiding overposting more Plant targets than available grain seed.
- Update farming notes in repo docs to describe the Spring split.

## Test Plan
- Add a regression test with one Spring Prepare posting and one Spring Plant posting at equal priority; with 10 workers, assert claims split toward Plant instead of all going to Prepare.
- Add a test where only Prepare exists; assert Prepare can still claim the full seasonal Farm allocation.
- Add a small helper/unit test for phase cap math at low worker counts, especially total caps of 1, 2, and larger village sizes.
- Run `cargo test --bin civgame` or the focused farming/job-claim tests if full test time is too high.

## Assumptions
- “Balanced Split” means Plant must be protected, but Prepare should continue concurrently.
- The initial split should be 60/40 Plant/Prepare; this can be tuned later if playtesting shows fields are underprepared.
- No new crates or public gameplay APIs are needed.
