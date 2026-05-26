# Fix Repeating Gather Failure Loops

## Status — SHIPPED 2026-05-26

All four sections implemented:

- §1 Typed `GatherTarget` + `invalidate_cluster` / `invalidate_tile_across_tier_set` /
  `decrement_cluster` on `SharedKnowledge`; `ClusterId::UNKNOWN` sentinel for
  live-world picks; `GatherKnowledge::resolve_target` / `nearest_target` helpers.
- §2 `PersonAI.active_gather_claim: Option<GatherTarget>`; gather failure paths
  walk the agent's tier-set and call `invalidate_cluster` when concrete id known;
  successful-harvest depletion also walks the tier-set. P6b retarget preserves
  the original `source_tier`/`cluster_id`.
- §3 `gather::is_target_still_valid` validator + `PlannerCtx.gather_target_valid`
  + preflight wired into `GatherFromKnown`/`ForageFromKnown`/
  `ForageFromKnownForStorage`/`GatherAndHaulToPersonalBlueprint` preconditions.
  `GatherKnowledge` bundles `PlantMap`+`Plant` query to keep dispatchers under
  Bevy's 16-param ceiling.
- §4 `JOB_CLAIM_BACKOFF_TICKS = 600` (~30 s) stamped on `GoalCooldown` for
  `posting_goal(p)` when `fail_count >= MAX_FAIL_COUNT`. `job_claim_system`
  reads `Option<&GoalCooldown>` and skips matching postings.

## Context

Workers get stuck in tight `GatherWood`/`GatherFood`/`GatherStone` failure loops: the
dispatcher re-picks the same stale tile every tick, the worker arrives, finds the tree
felled (or the plant picked, or the cluster harvested by a sibling), and the loop
restarts. Diagnosis confirmed by code reading — four contributing gaps:

1. **Read/write tier asymmetry** in `SharedKnowledge`. `nearest_in_tier_set` walks
   Household → Settlement → Faction finest-first (`shared_knowledge.rs:625-670`), but
   `gather_system` reports depletion using the worker's single finest tier
   (`gather.rs:475-484`, `shared_knowledge.rs:551-576`). A household worker that read a
   faction-tier cluster invalidates only the household copy — the faction rep persists
   and is the very thing it reads again next tick.
2. **No target provenance on the gather claim.** `GatherClaim` carries `(tile,
   MemoryKind, expires_tick)` (`gather_claims.rs:33-37`). Failure paths fall back to
   `cluster_at(tile)` which is non-deterministic.
3. **No HTN dispatch preflight.** `GatherFromKnownMethod::precondition` checks only
   `gather_target_tile.is_some()` (`htn.rs:1896-1904`); nothing re-validates "is there
   still a mature tree / mature plant / live resource on this tile?". The arrival-time
   `P6B_RETARGET_COOLDOWN` neighbor scan (`gather.rs:519-522`) only fires *after* the
   worker walked there.
4. **No job-claim backoff.** `job_claim_system` (`jobs.rs:4390-4727`) doesn't consult
   `GoalCooldown`; `job_claim_release_system` (`jobs.rs:5269-5356`) drops the claim
   after `fail_count >= 3` without stamping any backoff. The released worker can
   re-claim the same posting next tick.

Goal: stale concretes evaluate `false`, so the HTN fallback partition (`Explore`) wins
cleanly and the worker retargets or scouts instead of pacing.

## Design choices

- **Preflight site**: HTN `precondition` (not post-dispatch validator) — preserves the
  fallback partition.
- **Backoff store**: existing `GoalCooldown` — no parallel `ClaimCooldown` resource.
- **Scope**: gather-only in this PR; `TaskOutcome` contract deferred to
  [task-outcome-feedback-contract.md](task-outcome-feedback-contract.md).

## Plan

### 1. Typed `GatherTarget` provenance

In `src/simulation/shared_knowledge.rs`:

```rust
pub struct GatherTarget {
    pub tile: (i32, i32),
    pub kind: MemoryKind,
    pub source_tier: KnowledgeTier,
    pub cluster_id: ClusterId,
}
```

- Replace `GatherKnowledge::nearest_target_tile(...) -> Option<((i32,i32), MemoryKind)>`
  with `nearest_target(...) -> Option<GatherTarget>`.
- Thread `GatherTarget` into `GatherClaim` (`gather_claims.rs:33-37`): claim becomes
  `(target: GatherTarget, expires_tick)`.
- Add exact-invalidation APIs to `SharedKnowledge`:
  - `invalidate_tile(tier, kind, tile)` — remove one rep.
  - `invalidate_cluster(cluster_id)` — drop the cluster across all tiers it was
    promoted to.
  - `decrement_cluster(cluster_id, by: u32)` — partial depletion.

### 2. Symmetric depletion

In `gather_system` (`gather.rs:475-484`), failure paths use the stored
`GatherTarget.source_tier` + `cluster_id` to call `invalidate_cluster` /
`decrement_cluster`, then walk every tier in the actor's tier set to strip promoted
copies. `awareness_gossip_system` honors a short per-(cluster_id, tier) suppression
window so gossip can't re-promote an just-invalidated cluster.

### 3. HTN dispatch preflight on `GatherFromKnown`

In `htn.rs:1896-1928`, `GatherFromKnownMethod::precondition` calls a new
`gather::is_target_still_valid(target: &GatherTarget, ...) -> bool`:

- Wood → `Tree` entity with `growth_stage == Mature` (or deadwood if axe-less).
- Food → `Plant` with mature edible yield, or edible ground item.
- Stone / Limestone / etc. → tile kind matches and is reachable.
- Reeds → marsh/river tile still holds reeds.

On miss: invalidate via §1 APIs, return `false`. Stale concrete drops from the
partition; `Explore` becomes eligible. Highest-leverage fix — even if §1/§2 had bugs,
preflight collapses every stale concrete to `false`.

### 4. Job-claim backoff via `GoalCooldown`

- `job_claim_release_system` (`jobs.rs:5269-5356`): when `fail_count >= MAX_FAIL_COUNT
  (3)`, stamp `goal_cooldown.stamp(posting_goal, now + JOB_CLAIM_BACKOFF_TICKS)` before
  removing `JobClaim`. Map via `posting_goal` (`jobs.rs:4751-4774`). Constant:
  `JOB_CLAIM_BACKOFF_TICKS = 600` (~30 s at 20 Hz, tunable).
- `job_claim_system` (`jobs.rs:4390-4727`): in candidate scoring, skip postings whose
  mapped goal is cooling for this worker.

## Critical files

- `src/simulation/shared_knowledge.rs` — typed `GatherTarget`, exact-invalidation APIs,
  tier-symmetric depletion.
- `src/simulation/gather_claims.rs` — claim carries `GatherTarget`.
- `src/simulation/gather.rs` — arrival/mid-work failure uses provenance; keep
  `P6B_RETARGET_COOLDOWN` neighbor scan as defense-in-depth.
- `src/simulation/htn.rs` — `GatherFromKnownMethod::precondition` runs preflight.
- `src/simulation/jobs.rs` — release stamps `GoalCooldown`; claim respects it.
- `src/simulation/goals.rs` — `GoalCooldown` (`:375-434`) consumed as-is.
- `src/simulation/CLAUDE.md` — document target provenance, preflight rule, job→cooldown
  wiring.

## Verification

- Unit tests:
  - `shared_knowledge`: cluster promoted to all three tiers is fully invalidated by one
    `invalidate_cluster`.
  - `gather`: household worker that read a faction-tier rep and fails on arrival
    leaves no rep in any tier.
  - `htn`: `GatherFromKnown::precondition` returns `false` for a felled tile; Explore
    becomes eligible.
  - `jobs`: worker released with `fail_count >= 3` cannot reclaim same posting until
    `JOB_CLAIM_BACKOFF_TICKS` expire.
- Regression: single-tree seed, two workers; second worker must retarget or Explore,
  not loop on the felled tile.
- `cargo test --bin civgame`.
- `cargo run` — settlement with `Stockpile{Wood}` posting; confirm via inspector that
  workers don't churn on a felled tile.

## Assumptions

- HTN fallback partition stays intact (`MF_FALLBACK_ONLY`); fix makes stale concretes
  `false`, never lets Explore outrank a known-good concrete.
- `GoalCooldown` is the right backoff surface; no parallel `ClaimCooldown`.
- No new crates.
- P6b arrival-retarget scan stays as defense-in-depth; should rarely fire once
  preflight is in place.
