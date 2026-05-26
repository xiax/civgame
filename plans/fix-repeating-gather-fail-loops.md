# Fix Repeating Gather Failure Loops

## Summary
This is systemic, not just one bad tree. The worker is stuck because target selection, target invalidation, method failure history, and job claiming are split across separate systems.

The likely loop is: `GatherWood` keeps reading a stale wood cluster from shared knowledge, `gather_system` reports depletion only to the worker’s finest tier, and the claimed `Stockpile` job keeps forcing the worker back to `GatherWood`. The fallback `Explore` method cannot rescue this while any concrete stale target still passes precondition, because fallback methods are only considered when no concrete method passes.

## Key Changes
- Replace tile-only gather selection with target provenance:
  - Change `GatherKnowledge::nearest_target_tile(...)` to return a `GatherTarget { tile, kind, tier, cluster_id }`.
  - Store that provenance in the active gather claim, rather than only `(tile, MemoryKind)`.
  - Add exact invalidation APIs to `SharedKnowledge`, so failures remove the specific stale rep/cluster instead of doing a best-effort nearest-kind lookup.

- Make depletion symmetric with lookup:
  - `nearest_in_tier_set` reads household → settlement → faction.
  - Gather failure must invalidate the source tier and any promoted copies in the actor’s accessible tier set, not only the worker’s finest tier.
  - This directly addresses the mismatch around [gather.rs](/Users/xiao1/civgame/src/simulation/gather.rs:475) and [shared_knowledge.rs](/Users/xiao1/civgame/src/simulation/shared_knowledge.rs:641).

- Add dispatch preflight for gather targets:
  - Before dispatching `Task::Gather`, validate that the chosen target still matches the requested resource: mature tree for wood, mature edible plant for food, valid stone/reed tile for those resources.
  - If invalid, invalidate the knowledge target and skip dispatch for that tick. Do not produce a visible `FailedTarget` loop.

- Make job failure backoff real:
  - Update `job_claim_system` to read `GoalCooldown` or a small job-claim backoff and skip postings whose mapped goal is cooling down.
  - When `job_claim_release_system` releases a worker after `fail_count >= 3`, stamp cooldown/backoff before removing the claim.
  - This matches the intended behavior documented in [goals.rs](/Users/xiao1/civgame/src/simulation/goals.rs:2004), but currently missing from [jobs.rs](/Users/xiao1/civgame/src/simulation/jobs.rs:4344).

- Generalize the pattern for other tasks:
  - Introduce a lightweight task failure feedback contract: target failures invalidate the target source; routing failures cool down that route/claim; job failures apply backoff.
  - Start with gather/stockpile, then reuse the same event/helper shape for scavenge, haul, build, and craft target loss.

## Test Plan
- Add `SharedKnowledge` unit tests for promoted stale resource invalidation across household/settlement/faction tiers.
- Add a regression test where a household worker claims `Stockpile{Wood}` from a faction-tier stale wood sighting and must not repeatedly dispatch `GatherFromKnown` to the same tile.
- Add a race test where one worker harvests a tree before another arrives; the second worker should retarget nearby or invalidate the stale source, not loop.
- Add a job-claim test proving a worker released after repeated failure does not immediately reclaim the same posting while backoff is active.
- Run `cargo test --bin civgame` or targeted `cargo test --bin civgame <test_name>` during implementation.

## Assumptions
- Keep the HTN fallback partition intact; the fix is to make stale concrete preconditions become false, not to let `Explore` beat known-good concrete work.
- No new crates.
- Update `AGENTS.md` / `src/simulation/CLAUDE.md` to document the new target provenance and failure feedback contract.
