**Fix Disconnected Startup Roads** — SHIPPED

**Root cause (verified in source)**
- `compute_settlement_survey_core` sets the survey's `road_pushes` only from `desire_path_push`, which tick-gates and returns nothing at `tick = 0` (`tick - last_path_carve_tick = 0 < DESIRE_PATH_INTERVAL`).
- Both startup passes (`kickoff_initial_survey_system`, `resurvey_after_seeding_system`) run at tick 0, so `brain.road_segments` (the spine) was built but never enqueued as `RoadCarveJob::Segment`.
- Only the seed pass's doormat `Connector`s reached the OnEnter `road_carve_system` drain. Connectors route to *planned* spine tiles, so they carved stubs to where the spine should be — but the centerline was never painted. `project_initial_settlement_plans_system` then recorded the layout hash, so `settlement_planner_system` saw a matching hash and skipped its spine enqueue too.
- Queue/schedule bug, not a planning-algorithm bug.

**Fix**
- Added `organic_settlement::enqueue_spine_segments(segments, faction_id, era, road_queue)` — pushes `RoadCarveJob::Segment` per `StreetSegment` with `road_width_for(seg.tier, era)` (mirrors `settlement_planner_system`).
- Called it from `survey_one_settlement` (the startup-only chokepoint shared by kickoff + resurvey; NOT the async core). Kickoff push carves in the OnEnter drain → connects doormats; resurvey push carves on tick 1 → catches building-driven spine shifts. Idempotent (`try_write_road` no-ops an already-`Road` tile).
- No change to `project_initial_settlement_plans_system` / `settlement_planner_system` — the hash-skip is now correct (spine already carved at startup).

**Tests** (`test_fixture.rs`)
- Existing doormat cardinal-neighbour tests kept.
- `neolithic_/bronze_startup_spine_is_carved_and_connected` (via `assert_startup_spine_carved_and_connected`): after `trigger_onenter`, flood-fill carved `Road` tiles in a box around the player home and assert (a) ≥60 carved Road tiles, (b) the largest cardinal component ≥20 tiles, (c) ≥2/3 of doormats join that main component. Measured contrast (seed `0xE7A_5EED`): pre-fix = 23 Road tiles / largest 3 / 1-of-10 doormats; post-fix = 120+ / 45+ / 9-of-10. Confirmed failing pre-fix.
- A strict "every centerline tile is Road" / "single connected component" assertion is NOT used: the carve skips segment endpoints, so 4-way crossings fragment into ~7 arms (pre-existing), and the spine legitimately routes around seeded structures/farms. The thresholds above cleanly separate pre/post without asserting something false.

**Docs** — updated `mod.rs` OnEnter-chain comments + `simulation/CLAUDE.md` seeding note (spine queued by survey pass, not seed pass).
