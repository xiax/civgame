# Farm Claim Stale-Target Fix

## Summary
Fix the Farm idle loop where a worker holds a chief `JobKind::Farm` claim at ~99%, no dispatcher can find phase work, and `goal_contract_backstop_system` records repeated `Unknown -> FailedTarget`.

Root cause from inspection: `FieldWork` postings can become impossible after posting, especially Spring `Plant` jobs overcommitting the same seed stock across multiple plots. Claimed `FieldWork` postings are also shielded from stale cleanup, so the worker keeps resuming an unexecutable claim.

## Key Changes
- In [jobs.rs](/Users/xiao1/civgame/src/simulation/jobs.rs), make Spring Plant posting seed-budget aware across all plots:
  - Track `seed_remaining` per faction.
  - Subtract remaining live Plant postings before emitting new ones.
  - After emitting a Plant posting, decrement `seed_remaining`.
  - Ensure total open Plant targets never exceeds available seed stock.

- Add a Farm `FieldWork` reconciliation path before `chief_job_posting_system`:
  - Recompute live remaining capacity for Prepare, Plant, and Harvest from current tiles/plants/seeds.
  - If a posting has no possible remaining work, remove it immediately and release all `JobClaim` / `ClaimTarget` holders.
  - If partial work was already completed, shrink `target` to completed work and complete it as successful partial work.
  - If zero work was done and the posting is impossible, cancel it as unsuccessful.

- Keep worker-role policy unchanged:
  - Non-Farmer workers may still help with seasonal chief Farm postings.
  - This fix only prevents impossible claimed jobs from pinning workers idle.

- Update farm notes in [AGENTS.md](/Users/xiao1/civgame/AGENTS.md) to document seed-budgeted Plant postings and immediate stale `FieldWork` reconciliation.

## Test Plan
- Add regression tests in [test_fixture.rs](/Users/xiao1/civgame/src/simulation/test_fixture.rs):
  - Multiple prepared plots with limited seeds produce Plant postings whose combined target is `<= seed_total`.
  - A claimed Plant posting with no seed/plantable capacity releases the worker instead of leaving `JobClaim` pinned.
  - A claimed Harvest posting whose mature crop vanished is removed/released without waiting for chronic-failure cadence.
  - A partially completed stale `FieldWork` posting shrinks/completes instead of sitting at 99%.

- Run:
  - `cargo test --bin civgame farm`
  - `cargo test --bin civgame job_claim`
  - `cargo check`

## Assumptions
- The desired behavior is “do all currently possible farm work, then free workers promptly,” not “hold the exact original target forever.”
- Partial farm work should be paid/completed when the remaining target becomes impossible through world drift or prior overcommit.
- No new crates or save-format/data-schema changes are needed.
