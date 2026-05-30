# Day-One 5× Performance Fix — SHIPPED

## Goal
Smooth first-day, 100-worker, 5×-speed play: avg tick ≤10 ms, no Economy p99 spike
near 90 ms before decay/TTL thins state.

## Root cause
`vision_system` wrote every sighting to the agent's *finest* tier (Household if a
member, else Faction). With ~100 workers across many households, every *public*
sighting (stone, reeds, wild plants, prey, herds, ground items) was duplicated into
each household's tier, then `cluster_tier_promotion_system` copied them all up in a
`tick % 200` burst — the Economy p99 spike.

## Shipped (2026-05-29)

- **A. Instrumentation.** `cluster_tier_promotion` suspect timer (`speed.rs` slot 8 +
  `knowledge.rs` guard). Path worker telemetry (`worker_us_per_tick` /
  `paths_dispatched_per_tick` / `queue_len`) surfaced in debug panel "Performance"
  header (`ui/debug_panel.rs`); fixed stale "PreUpdate" worker comment.
- **B. Vision write-tier by owner** (`memory.rs::vision_system`). Per-sighting
  `tier_for(owner)` selector: `Public` → settlement tier (`SettlementMap::first_for_faction`)
  else faction; household-owned stays Household; `Person` stays finest. Reads still work
  via `TierSet::tiers()` finest-first — no promotion hop for household members.
- **C. Ground items dropped from SharedKnowledge** (`memory.rs`). Kept `CurrentVision`
  pushes (entity-anchored) + AnyEdible dual-push. Scavenge reads CurrentVision + live
  SpatialIndex; loose food swept by `chief_loose_stockpile_posting_system`. `knows_food`
  Calories gate now keys on plant clusters.
- **D. cluster_tier_promotion de-bursted** (`knowledge.rs`). Removed `% 200`; per-tick
  `ClusterPromotionCursor` + `budget.cluster_promotions_per_tick` (64) over officials via
  pure `select_promotion_slice`; linear `snaps.iter().find` → `AHashMap<Entity,usize>`.
- **E. Path worker knob** (`perf.rs` + `worker.rs`). `path_requests_per_tick` default
  192 (= `PATH_BUDGET_PER_TICK`, single-sourced). Tunable, not a regression — lower only
  after telemetry shows the drain is the spike. Real levers are B+C (less stale dispatch).
- **F. Docs** updated: `src/simulation/CLAUDE.md`, `src/pathfinding/CLAUDE.md`, `perf.rs`.

## Tests
`select_promotion_slice` unit tests (4) + behavioral `public_sighting_writes_to_settlement_tier_not_faction`
and `ground_item_not_written_to_shared_knowledge_but_live_visible` (`test_fixture.rs`,
new `register_settlement` helper). All 6 pass. Full suite 1505 pass; the 2 stragglers
(`destitute_tenant_evicted_after_two_misses`, `storage_first_falls_back_on_recent_withdraw_failure`)
are the known async-ordering flakes (pass in isolation), unrelated to this change.

## In-game verification (pending operator)
Debug panel first-day 100-worker 5×: Economy p99 ↓ toward ≤10 ms; flat
`cluster_tier_promotion` row; watch `worker_us_per_tick` to decide any path-budget cut;
`path_request_skipped_cooldown` + `path_failed_*` should fall; knowledge-cluster count
drops sharply (B+C).
