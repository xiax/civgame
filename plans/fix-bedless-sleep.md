# Fix Bedless Sleep With Valid Beds

## Summary
- Existing `sleep` and `bed` test filters pass, so the old 30-tile eligibility fix is present.
- The remaining likely hole is claim consistency: workers can keep `Task::Sleep { bed: None }` when a valid bed claim exists or when stale `Bed.owner` / `HomeBed` state prevents a free bed from being reclaimed.
- Treat bed assignment as a bidirectional invariant: `Person.HomeBed == bed` and `Bed.owner == person` must agree, be live, and be same-root-faction eligible.

## Key Changes
- In [construction.rs](/Users/xiao1/civgame/src/simulation/construction.rs), add a reconciliation step at the start of `assign_beds_system`:
  - Clear `Bed.owner` when the owner entity is gone, the owner’s `HomeBed` does not point back to that bed, or the bed is no longer eligible for the owner’s root faction.
  - Treat a person’s `HomeBed` as stale when the bed entity is missing, the bed owner is not that person, or the bed is ineligible for that person’s root faction.
  - Feed the homeless assignment pass from this validated claim state, not just `bed_query.get(home_bed).is_err()`.

- In [htn.rs](/Users/xiao1/civgame/src/simulation/htn.rs), make sleep dispatch ignore invalid bed claims:
  - Change the bed lookup to read both `Transform` and `Bed`.
  - Only set `ctx.home_bed/home_bed_tile` when `Bed.owner == Some(actor)`.
  - Keep the existing fallback behavior when no valid bed claim exists.

- In [sleep.rs](/Users/xiao1/civgame/src/simulation/sleep.rs), add a guarded reroute for active bedless sleep:
  - If current task is `Task::Sleep { bed: None }`, the worker has a live valid `HomeBed`, and they are not already on that bed, cancel the sleep chain so the next dispatch routes to `Task::Sleep { bed: Some(...) }`.
  - Avoid retry loops by skipping this cancellation for a short cooldown after a recent `MethodId::SLEEP` routing failure; add a small `MethodHistory` helper if needed.

- Update [src/simulation/CLAUDE.md](/Users/xiao1/civgame/src/simulation/CLAUDE.md) and [AGENTS.md](/Users/xiao1/civgame/AGENTS.md) to document that bed claims are reconciled bidirectionally and bedless sleep reroutes when a valid bed appears.

## Test Plan
- Add regression tests for:
  - A stale `Bed.owner` pointing to a dead/mismatched/non-reciprocal person is cleared and the bed becomes claimable.
  - A person with `HomeBed(Some(bed))` where `Bed.owner != Some(person)` is treated as homeless and assigned a valid bed.
  - A worker already sleeping with `Task::Sleep { bed: None }` cancels and reroutes after receiving a valid `HomeBed`.
  - A recent failed bed route does not cause every-tick wake/cancel churn, but retries after the cooldown.

- Run:
  - `cargo test --bin civgame sleep`
  - `cargo test --bin civgame bed`
  - `cargo check`

## Assumptions
- “Available bed” means a live same-root-faction bed that is unowned after reconciliation, not a foreign bed or a genuinely unreachable bed.
- If a bed route truly fails, workers may temporarily sleep in place, but they should retry after a bounded cooldown instead of finishing every night outside forever.
