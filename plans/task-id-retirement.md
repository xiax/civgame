# Plan: retire `PersonAI.task_id` — DONE

## Status

All three steps shipped. 706 / 706 tests pass.

## What landed

### Step 2 — typed variants for the six legacy-only tasks

`simulation/typed_task.rs` grew four new variants (the producer/consumer pairs
that the field was still load-bearing for):

- `Task::ConstructBed { blueprint }` — `player_command.rs` Build branch
  dispatches it when `kind == BuildSiteKind::Bed`. `construction::
  construction_system`'s worker branch now resolves the blueprint via
  `aq.current.as_construct_bed()` first, falling back to `as_construct()` /
  `target_entity` for legacy paths.
- `Task::Deconstruct { tile }` — produced by `building_upgrade_system`
  (Sequential) and `player_command::dispatch_one` (player Deconstruct order).
  Consumed by `construction::deconstruct_system` via the existing
  `aq.current_task_kind() == TaskKind::Deconstruct as u16` check, which now
  works because `task_kind_for(Task::Deconstruct)` is wired.
- `Task::Terraform { tile }` — `terraform_dispatch_system` dispatches it
  alongside the legacy routing call. `terraform_system` reads via
  `aq.current_task_kind()`.
- `Task::MilitaryAttack { foe }` — `player_command::dispatch_one`
  (`PlayerCommand::MilitaryAttack`) plus `military_task_system`'s
  reroute-on-foe-moved path both dispatch it. The reroute path calls
  `aq.cancel(); aq.dispatch(...)` to keep the typed channel coherent with the
  refreshed legacy `dest_tile`.

The two remaining `TaskKind` variants (`Trader`, `Craft`) had no live
producer — `TaskKind::Craft` was dead since the CraftOrder pipeline replaced
the legacy per-agent crafter, and `TaskKind::Trader` was never written
anywhere; the only live consumer (`economy/transactions.rs::market_buy_system`)
guarded a state-flip on `aq.current_task_kind() == TaskKind::Trader as u16`,
which was unreachable. That branch is gone. The enum variants stay for now
(label / hand-cost / interacts-from-adjacent metadata still indexes by the
`u16` discriminant), but nothing dispatches them.

### Step 3 — field deletion

- `pub task_id: u16` is gone from `simulation::person::PersonAI`. So is
  `PersonAI::UNEMPLOYED`; the sentinel migrated to
  `simulation::typed_task::UNEMPLOYED_TASK_KIND` (`u16::MAX`) and is
  re-exported from `simulation::person` so existing call-sites keep
  resolving.
- `assign_task_with_routing` no longer writes the legacy field. Every
  producer that previously relied on the mirror now calls
  `aq.dispatch(Task::X { ... })` after the routing call.
- `ActionQueue::finish_task` / `cancel_chain` no longer touch the legacy
  field. The bundled exit helpers stay as-is.
- `typed_task::task_id_matches_current` is deleted along with the
  `TestSim::assert_task_state_coherent` / `coherence_check_enabled` /
  `skip_coherence_check` machinery. `skip_coherence_check` survives as a
  no-op shim so existing tests still compile.
- Test fixtures that previously read or wrote `ai.task_id` directly were
  cleaned up — assertions on the field became vacuous (the typed channel
  already pins the same state), and direct setters were already paired with
  a `aq.dispatch(Task::X { ... })` adjacent line.

### Step 7 — docs

- `src/simulation/CLAUDE.md`'s ActionQueue section drops the
  "legacy `task_id` co-exists" language. The legacy-only producer list
  moved into the producer bullet so the doc reflects the current shape.
- `src/simulation/typed_task.rs` module header rewritten — no more "Phase 4
  in-progress" framing; the typed channel is canonical.

## What stayed legacy

- `PathRequest.task_id: u16` is unchanged. Per the plan, that's a
  diagnostic-only snapshot on the pathfinding side and remains untouched.
- `TaskKind` enum still exists. It's indexed by the surviving consumer
  helpers (`task_kind_label`, `task_requires_free_hands`, `task_is_labor`,
  `task_interacts_from_adjacent`) which take a raw `u16` and dispatch
  against `TaskKind::X as u16`. `aq.current_task_kind()` is what they're
  fed today. The enum is *projection target*, not state.
