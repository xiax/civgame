# Day-One 5× Performance Fix

## Summary
- Optimize for first-day 100-worker 5× play, where decay has not had time to run.
- Main target: stop `SharedKnowledge` from creating thousands of duplicate public resource clusters across household tiers, then smooth any remaining promotion/path spikes.
- Success: average tick ≤ 10 ms and Economy p99 no longer spikes near 90 ms during the first day.

## Key Changes
- Add suspect timing for `cluster_tier_promotion` and `path_worker` so the current hidden spikes are visible in the Performance panel.
- Change vision write-through policy:
  - Public static resources from vision, such as wild plants, stone, reeds, prey/herd observations, write to Settlement tier when available, otherwise Faction tier.
  - Household tier is reserved for genuinely private resources, such as `ResourceOwner::Household`.
  - Ground items stay live-only through `CurrentVision`/`SpatialIndex`; do not create `SharedKnowledge` clusters.
- Budget `cluster_tier_promotion_system` with a cursor and `PerfWorkBudget::cluster_promotions_per_tick`, rather than copying every source cluster on a 200-tick burst.
- Optimize promotion neighbor lookup with an `Entity -> Snap` map instead of repeated linear searches.
- Move path worker capacity into `PerfWorkBudget::path_requests_per_tick`, defaulting to 64 instead of hardcoded 192, to protect the 10 ms 5× tick budget during failure storms.
- Update docs in `src/simulation/CLAUDE.md` and `src/pathfinding/CLAUDE.md`.

## Test Plan
- Verify public sightings from 100 household/market workers create one shared settlement/faction knowledge surface, not one duplicate cluster set per household.
- Verify household-owned resources still write/read through Household tier.
- Verify visible ground items still dispatch via `CurrentVision`/`SpatialIndex`.
- Verify promotion processes no more than the configured per-tick budget and resumes across ticks.
- Verify path worker drains no more than `path_requests_per_tick`.
- Run `cargo check` and `cargo test --bin civgame`.
- Re-test first-day 100-worker 5×: watch knowledge clusters, Economy p99, path failures, and new suspect timers.

## Assumptions
- Public resource knowledge does not need to be duplicated per household on day one.
- Private household resources still need household-level knowledge.
- Decay/TTL changes are secondary and should not be counted on for the first-day fix.
