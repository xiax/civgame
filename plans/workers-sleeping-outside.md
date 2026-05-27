# Fix Workers Sleeping Outside Despite Free Beds

## Summary
- Root cause: `assign_beds_system` only scans unclaimed beds within a hard-coded 30-tile box around `FactionData.home_tile`.
- `htn_sleep_dispatch_system` only routes to a bed when the worker already has a live `HomeBed`; otherwise it routes home or sleeps in place.
- Fix the bed-claim layer so valid same-faction settlement beds are claimed before sleep dispatch relies on fallback sleeping.

## Key Changes
- Update `assign_beds_system` in `src/simulation/construction.rs` to decide bed eligibility by ownership/settlement context, not only fixed distance:
  - reject beds owned by another faction’s member;
  - accept unowned beds on same-faction `PlotIndex` plots;
  - fall back to a population/phase-scaled home radius for legacy or pre-plot beds.
- Preserve existing spouse-pairing and reassignment behavior, but feed those passes from the broadened claimable-bed set.
- When a worker receives a `HomeBed` while already on an unbedded `Task::Sleep { bed: None }`, cancel that sleep chain so the next dispatch routes them to the newly assigned bed instead of finishing the night outside.
- Update `src/simulation/CLAUDE.md` and the root `AGENTS.md` sleep/settlement notes to document that `HomeBed` assignment uses owned plots/settlement scope, not a fixed home radius.

## Test Plan
- Add a regression test where a same-faction worker claims an unowned bed beyond 30 tiles when the bed sits inside a same-faction residential plot.
- Add a negative test where an unowned/owned bed in another faction’s plot is not claimed.
- Add a dispatch test confirming a tired worker with the new `HomeBed` gets `Task::Sleep { bed: Some(bed_entity) }`.
- Run `cargo test --bin civgame` or at minimum the sleep/bed-assignment regression tests plus `cargo check`.

## Assumptions
- “Free beds” means unowned beds that are part of the worker’s own settlement/plot territory, not beds in another faction’s home.
- The durable contract remains: sleep routing uses `HomeBed`; the fix belongs in bed assignment rather than making HTN sleep opportunistically steal arbitrary beds.
