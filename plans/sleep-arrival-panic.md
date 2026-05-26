# Fix Sleep Arrival Panic

## Summary
- Root cause: `movement_system` treats a blocked non-adjacent arrival as a taskless move and calls `aq.assert_idle()`. With `Task::Sleep { bed: Some(..) }` still current, that violates the typed-task invariant and panics.
- Fix the arrival collision path so only truly taskless player moves assert idle; live typed tasks are cancelled as routing failures.

## Implementation Changes
- In `src/simulation/movement.rs`, update the `!bumped && !is_adjacent_task` branch:
  - If `aq.current_task_kind() == UNEMPLOYED_TASK_KIND`, keep `aq.assert_idle(&mut ai)`.
  - Otherwise call `record_routing_failure(&mut history, &mut ai, now)` and `aq.cancel_chain(&mut ai)`.
  - Keep the existing `PathFollow` reset and stand-reservation release.
- Add a small private helper if needed so the branch is readable and directly unit-testable.
- No public API, ECS component, resource, or schedule changes.

## Test Plan
- Add a regression test in `src/simulation/movement.rs` that sets `ActionQueue.current = Task::Sleep { bed: None/Some }`, `ai.state = Seeking`, `ai.active_method = Some(MethodId::SLEEP)`, then exercises the blocked non-adjacent arrival helper.
- Assert the test records `MethodOutcome::FailedRouting`, clears `active_method`, resets `ActionQueue.current` to `Idle`, and leaves `ai.state == Idle` without calling `assert_idle`.
- Run:
  - `cargo test --bin civgame movement`
  - `cargo test --bin civgame sleep`

## Assumptions
- A blocked occupied sleep destination should be treated as a routing failure, not as successful sleep-on-arrival.
- Player `Move` remains taskless (`UNEMPLOYED_TASK_KIND`) and keeps the existing idle-arrival behavior.
