# TaskOutcome Feedback Contract (skeleton — follow-up to gather-fail-loops fix)

## Why

Once the gather-only fix lands ([fix-repeating-gather-fail-loops.md](fix-repeating-gather-fail-loops.md)),
the invalidate-and-cooldown pattern lives inline in `gather_system` + `htn.rs`. The
same loop shape — stale target + no provenance + no backoff — can hit scavenge, haul,
build, craft. Extract a single feedback contract instead of open-coding it at each
site.

## Sketch

```rust
pub enum TaskFailure {
    TargetGone   { resolved: ResolvedTarget },   // tree felled, item picked, plant gone
    Unreachable  { resolved: ResolvedTarget },   // pathfinding gave up
    JobCapped    { posting: PostingId },         // posting-level repeat fail
}

pub fn report_task_failure(
    actor: Entity,
    failure: TaskFailure,
    shared: &mut SharedKnowledge,
    cooldown: &mut GoalCooldown,
    plan_history: &mut PlanHistory,
    now_tick: u32,
);
```

Central dispatch: tier-symmetric invalidate (using provenance), `PlanHistory` bump,
`GoalCooldown` stamp.

## Call sites to convert

- `src/simulation/gather.rs` — `gather_system` (arrival miss + mid-work loss); replaces
  the inline invalidation landed in the gather-only fix.
- `src/simulation/scavenge.rs` (mirror of `ScavengeFood` path).
- Future `task_complete_system` haul failure path.
- `src/simulation/construction.rs` — build target gone / blueprint canceled.
- `src/simulation/crafting.rs` — recipe ingredient missing at workbench.

## Open questions

- Module home: `goal_contract.rs` (already mediates goal/task lifecycle) vs. a new
  `task_outcome.rs`. Lean toward new module — `goal_contract.rs` is goal-scoped, this
  is task-scoped.
- `ResolvedTarget` shape: probably an enum carrying `GatherTarget` for resources, a
  `(PostingId, BlueprintId)` for build, etc. Keep narrow per task family.
- Cooldown granularity: per-`AgentGoal` (current `GoalCooldown` key) vs. per-(goal,
  resolved-target). The latter avoids penalizing GatherWood broadly when one cluster
  failed; trade-off is a wider cooldown table.
- Should `report_task_failure` write to `SimDiagnostics` so the inspector can show
  recent task failures per agent?

## Entry points

- After gather fix ships, start in `gather_system` — refactor inline invalidation into
  a `TaskFailure::TargetGone` call. Confirm tests stay green.
- Then convert `scavenge.rs` (closest sibling).
- Each subsequent system (haul / build / craft) is a separate small PR.

## Out of scope

- Don't change `GoalCooldown` storage shape until needed by a real second consumer.
- Don't unify with `PlanHistory` — keep them as separate bias signals per existing
  memory (`feedback_plan_history_design`).
