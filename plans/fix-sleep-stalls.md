# Fix Sleep Stalls From Invalid Hotspot Paths

## Context

A worker stalls on its Sleep task. Two faults compose:

1. **Hotspot fast path emits false failure.** `pathfinding/worker.rs::compute_land` (lines 484–541) — when the same-chunk goal has a hotspot flow-field and `walk_to_goal` returns a path, `first_invalid_step` validates 3D continuity. On bad step the function returns `FailSubReason::NoRouteStepContinuity` instead of falling through to A*. The miss path (`walk_to_goal` returns `None`) already sets `hotspot_miss = true` and falls through — the bad-step branch should do the same. A stale cached flow-field is a recoverable cache miss; only A* itself producing a bad-step path indicates a real planner/runtime inconsistency.
2. **`release_to_idle` records phantom HTN Success.** `simulation/movement.rs::release_to_idle` (lines 58–86) calls `aq.cancel()` (clears `current` → `Idle` + queued) but does **not** clear `PersonAI.active_method`. Next tick, `htn_method_completion_system` observes `current == Idle && queued_is_empty && active_method.is_some()` and writes `MethodOutcome::Success` against the stale method. The bug is **general, not Sleep-specific** — every HTN method whose movement is preempted via `release_to_idle` is mis-attributed. 5 call sites in `movement.rs` (~285, ~305, ~369, ~616, ~1117).

The cancel-path fix matches the convention already documented in `src/simulation/CLAUDE.md` → "Cancel paths record failure": `gather::finish_gather`, `items::item_pickup_system`, `items::finish_scavenge`, `production::finish_withdraw_material` all push `record_target_failure(...)` / `record_routing_failure(...)` against `active_method` before dropping the chain. `release_to_idle` is the one cancel surface that violates the convention.

## Implementation

### 1. Pathfinding fast-path fallback (`src/pathfinding/worker.rs`)

In `compute_land` hotspot fast path (around lines 496–516):

- When `walk_to_goal(...)` returns `Some(path)` and `first_invalid_step(chunk_map, req.start, &path, TraversalProfile::Land)` returns `Some(_)`, **do not return `NoRouteStepContinuity`**. Set `hotspot_miss = true` and fall through to the A* segment starting at line 542.
- Keep the `None`-from-`walk_to_goal` path (lines 530–531) unchanged.
- Preserve the A* path's `NoRouteStepContinuity` return (lines 652–660) — A* producing a bad step is a real inconsistency, not a cache miss.
- Add a distinct counter to `PathfindingDiagnostics` (definition at worker.rs:33–64):
  - `hotspot_fastpath_bad_steps: u64` — "Hotspot field returned a path, but `first_invalid_step` rejected it; fell through to A*."
  - Keep existing `hotspot_fastpath_misses` (for `None`-from-`walk_to_goal`) — different root causes; splitting them makes cache drift diagnosable from telemetry.
- Under `path_flags.verbose_logs`, emit one `debug!` per bad step with `(start, goal, first_bad_index)`.

### 2. Cancel-path outcome recording (`src/simulation/movement.rs`)

Change `release_to_idle` to record the HTN outcome before dropping the chain.

- **Signature.** Add `history: &mut MethodHistory`, `now: u64`, `outcome: MethodOutcome`. Inside, if `ai.active_method.is_some()`, push `(method_id, outcome, now)`, then clear `ai.active_method = None`. Then run the existing state/queue/transform reset.
- **Per call-site outcome:**

  | Site (movement.rs) | Trigger | Outcome |
  |---|---|---|
  | ~285 | `FollowStatus::Failed(_)` arm | `MethodOutcome::FailedRouting` |
  | ~305 | Cooldown release, `Idle`-arm same-goal re-fail | `MethodOutcome::FailedRouting` |
  | ~369 | Mid-nav goal change into a cooldowned goal | `MethodOutcome::FailedRouting` |
  | ~616, ~1117 | Read site context; `FailedRouting` if route-derived, `Interrupted` if goal-flip-derived |

- **Query plumbing.** Add `&mut MethodHistory` to the iterated tuple in `movement_system` (and the boundary-recovery system); thread `clock.tick` for `now: u64`.
- **Why not silent-clear.** The "lower-touch" alternative (clear without recording) avoids phantom Success but loses telemetry. `score_method_with_history` penalises methods with recent `FailedRouting` so the agent doesn't immediately re-pick the same broken plan. Silent-clear lets it loop. The `yield_for_maintenance_boundary` silent-clear convention is for a benign safe-boundary yield, not a route failure.

### 3. Sleep dispatcher: keep current in-place fallback (`src/simulation/htn.rs`)

No change. Existing pattern (routing fail → record `FailedRouting` + in-place `Task::Sleep { bed: None }`) is correct. With §1 in place, that fallback fires less often because reachable home/bed targets no longer bounce off a stale hotspot cache.

## Tests

- **Pathfinding unit test.** Build a small `ChunkMap` where a hand-constructed `FlowField` has `cell_z` that no longer matches live chunk Z. Call `compute_land` for an in-chunk goal; assert success (A* fallback), `hotspot_fastpath_bad_steps == 1`, `hotspot_fastpath_misses == 0`.
- **Simulation regression in `test_fixture`.** Spawn one Person on a flat world, set `ai.active_method = Some(MethodId::SLEEP)`, dispatch `Task::Sleep { bed: None }`, synthesise `FollowStatus::Failed`. Tick twice. Assert `MethodHistory` last entry is `(SLEEP, FailedRouting, _)`, not `Success`, and `ai.active_method == None`.
- **Run:** `cargo test --bin civgame pathfinding`, `cargo test --bin civgame sleep`, `cargo test --bin civgame htn_method_completion`, `cargo test --bin civgame movement`, `cargo check`.
- **In-vivo:** `cargo run`, focus on a Sleep-goal worker, watch inspector — no `Sleep → Success` flicker mid-walk; bedded arrival still records Success.

## Deferred follow-up — hotspot field invalidation

Root cause of the original symptom is the hotspot flow-field's `cell_z` drifting from live chunk Z. This plan ships the safety net; a follow-up at `plans/hotspot-field-invalidation.md` should investigate:

- Every `HotspotField` builder and rebuild trigger (in `pathfinding/hotspot.rs` / `flow_field.rs`).
- Whether rebuild fires on `TileChangedEvent`, `WallConstructed` / `WallDestroyed`, `TileCarvedEvent` from `excavation::finalize_carved_tile`, dig-down / mining completions, and Z-mutating events generally.
- The new `hotspot_fastpath_bad_steps` counter lets us measure invalidation drift in-game once §1 ships.

## Out of scope

- No new crate dependencies.
- No UI wording changes — accounting fix removes false-success display automatically.
- No change to the hotspot flow-field algorithm itself.
- No change to `gather::finish_gather` / `items::*` / `production::finish_withdraw_material` cancel sites — they already record correctly.
