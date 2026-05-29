**Fix Disconnected Startup Roads**

**Summary**
- The screenshot is showing door doormats/connectors being carved, but the main planned street spine is not carved at startup.
- `kickoff_initial_survey_system` builds `SettlementBrain.road_segments`, but only desire paths are queued, and desire paths are tick-gated at `tick=0`.
- `project_initial_settlement_plans_system` then records the same layout hash, so `settlement_planner_system` later thinks the spine was already handled and never queues it.

**Key Changes**
- Add a shared helper that enqueues a settlement brain’s `road_segments` as `RoadCarveJob::Segment` with `road_width_for(...)`.
- Call it from the startup survey path before the first `road_carve_system` drain, so the seed-time spine is actually painted before the post-seed resurvey.
- Also queue post-seed resurvey spine changes if the recomputed brain differs, so final seed structures do not leave newly planned spine pieces invisible.
- Update the stale comments/docs in [src/simulation/mod.rs](/Users/xiao1/civgame/src/simulation/mod.rs) and settlement notes to match the real startup flow.

**Tests**
- Keep the existing doormat cardinal-neighbour tests.
- Add a stronger Neolithic/Bronze startup test: after `trigger_onenter`, every player-faction planned spine centerline tile in the flat fixture is `TileKind::Road`.
- Add a road-component assertion: player door doormats and the carved spine belong to one cardinally connected road graph, not isolated local stubs.

**Assumptions**
- Preserve the current organic road design and widths; this is a queue/schedule bug, not a new road-planning algorithm.
- Roads should remain protected from structures, farms, wells, and blueprints through the existing `road_carve_system` guards.
- I verified the current existing test passes, which is expected because it only proves “each door has a nearby road tile,” not “the whole road network is connected.”
