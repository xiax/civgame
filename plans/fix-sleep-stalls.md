# Fix Sleep Stalls From Invalid Hotspot Paths

## Summary
- The worker gets stuck because a same-chunk hotspot flow-field route to the sleep/home target emits an invalid 3D step, producing `NoRoute (continuity)` instead of falling back to normal A*.
- The UI then reports `Sleep → Success` because movement cancels the task but leaves `PersonAI.active_method` set, so `htn_method_completion_system` records a false success.
- Fix both sides: make invalid hotspot paths non-fatal, and make path-failure cancellation record or clear the HTN method correctly.

## Implementation Changes
- In `src/pathfinding/worker.rs`, change the hotspot fast path in `compute_land`:
  - If `walk_to_goal(...)` returns a path and `first_invalid_step(...)` finds a bad step, do not return `NoRouteStepContinuity`.
  - Treat it as a hotspot miss: optionally log under `verbose_logs`, set `hotspot_miss = true`, and continue into the normal A* path already below the fast path.
  - Keep true A*/amphibious continuity failures as `NoRouteStepContinuity`, since those still indicate planner/runtime inconsistency outside the cache fast path.
- Add a targeted diagnostic counter only if useful, e.g. `hotspot_bad_step_fallbacks`, but do not expose a new UI concept unless needed. Existing `hotspot_fastpath_misses` can be reused if we want the smallest patch.
- In `src/simulation/movement.rs`, update `release_to_idle` so path-failure/external route cancellation does not become an HTN success:
  - Preferred: make `release_to_idle` accept `Option<(&mut MethodHistory, u64)>` or split a new helper for route failure that records `MethodOutcome::FailedRouting` via `record_routing_failure`.
  - Lower-touch alternative: clear `ai.active_method = None` before `aq.cancel()` so `htn_method_completion_system` cannot write a false `Success`.
  - Use the preferred option for `FollowStatus::Failed` and cooldown release paths; use the lower-touch clear for purely local drift/boundary recovery if there is no clear failure outcome.
- Preserve Sleep fallback behavior in `src/simulation/htn.rs`:
  - If `assign_task_with_routing` itself returns `false`, keep the current in-place sleep fallback.
  - The new path-worker fallback should mean reachable home/bed targets no longer bounce through `release_to_idle` just because a hotspot cache was bad.

## Tests
- Add a pathfinding regression test that builds a hotspot flow-field capable of returning a bad continuity path, then verifies `compute_land` falls through to A* and succeeds instead of returning `NoRouteStepContinuity`.
- Add or adjust a simulation-level regression around route cancellation:
  - Dispatch an HTN `Sleep` task with `active_method = SLEEP`.
  - Simulate a path failure release.
  - Assert `MethodHistory` does not record `Sleep → Success`; ideally it records `FailedRouting`.
- Run:
  - `cargo test --bin civgame pathfinding`
  - `cargo test --bin civgame sleep`
  - `cargo test --bin civgame htn_method_completion`
  - `cargo check`

## Assumptions
- No new crates.
- The primary fix should keep hotspot flow-fields enabled because they are a performance optimization for faction centers/storage.
- Invalid hotspot paths are recoverable cache misses; normal A* remains the authoritative fallback for same-chunk movement.
- UI wording can stay unchanged once the underlying failure/success accounting is correct.
