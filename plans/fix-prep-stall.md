# Fix Field-Prep Workers Stalling After Arrival

## Summary
- Existing `prepare_field` tests pass, but they only cover direct dispatch and forced `Working` completion.
- The likely stall is bucket-gating: workers can reach the field, but if their `BucketSlot` is outside the current `SimClock.population` span, `clock.is_active(slot)` is forever false, so `work_progress` never advances and `prepare_field_task_system` never completes.
- Fix the liveness hole in scheduling, then add a full dispatch → movement → work → completion regression for `PrepareField`.

## Key Changes
- In [schedule.rs](/Users/xiao1/civgame/src/simulation/schedule.rs), make `SimClock::is_active` normalize stale slots with `slot % population` when `population > 0`, preserving the existing `population == 0 => true` fixture behavior.
- In [mod.rs](/Users/xiao1/civgame/src/simulation/mod.rs), explicitly order `farm::prepare_field_task_system` after `movement::movement_system` and before `gather::gather_system`, matching the existing comment and dependency on movement-driven `Working`/progress state.
- Add a short note to [src/simulation/CLAUDE.md](/Users/xiao1/civgame/src/simulation/CLAUDE.md) documenting that bucket slots may be stale after deaths/materialization churn and are normalized for liveness.

## Test Plan
- Add a unit/regression test for `SimClock::is_active` where `population = 1`, active window is `0..1`, and a high `BucketSlot` still becomes active via normalization.
- Add an integration test in [test_fixture.rs](/Users/xiao1/civgame/src/simulation/test_fixture.rs): create a one-tile Agricultural plot, assign a farmer a `PrepareField` posting, force the worker’s `BucketSlot` above `population`, tick the real schedule, and assert the worker reaches `Working`, accumulates progress, stamps `TileKind::Cropland`, removes the posting, and releases `JobClaim`.
- Run `cargo test --bin civgame prepare_field` and the new schedule/bucket test filter.

## Assumptions
- The observed symptom is workers standing with `Preparing Field` indefinitely after reaching the field, not just several workers crowding one tile.
- No farming rules, resource costs, or job-posting targets change; this is a liveness fix for already-assigned work.
