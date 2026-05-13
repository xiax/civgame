# HTN Dispatcher Consolidation

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 4.
**Depends on:** Phase E + Phase F of parent plan shipped (so `dispatch_for_goal` helper exists and `Scored` mode is default-on).

## Trigger

Pick up after Phase F lands (`Scored` mode default, Legacy cascade removed). Migrate in batches of ~7 to bound review.

## Scope

Phase E of the parent plan migrates 2 of the 23 `htn_*_dispatch_system` functions onto the new `dispatch_for_goal` helper. The remaining 21 are mechanical migrations — same pattern, no behaviour change. Total ~600 lines deleted from `htn.rs` once complete.

## Current state (from survey)

23 `htn_*_dispatch_system` functions all colocated in `src/simulation/htn.rs`. Each one:
1. Queries for agents with matching `AgentGoal`.
2. Builds a partial `PlannerCtx` (only the fields it needs).
3. Lists candidate methods for the goal (hardcoded).
4. Calls `score_method_with_history` per candidate.
5. Picks the argmax and expands into `ActionQueue`.

After Phase E of parent plan, `dispatch_for_goal(goal, ctx, commands)` does steps 3–5 generically by reading `MethodRegistry`.

## The 21 dispatchers

`htn_acquire_food`, `htn_acquire_good`, `htn_stockpile_food`, `htn_equip_hunting_spear` (goal-agnostic — special-case), `htn_scout`, `htn_return_surplus`, `htn_tame_horse`, `htn_plant_from_storage`, `htn_build_claimed_blueprint`, `htn_deliver_hunt_kill`, `htn_engage_prey`, `htn_join_hunt_party`, `htn_combat_faction`, `htn_deliver_material_to_craft_order`, `htn_work_on_craft_order`, `htn_harvest_grain_for_craft_order`, `htn_harvest_plant`, `htn_play`, `htn_clear_obstacle` (goal-agnostic), `htn_dispatch_system` (legacy fallback — delete after all migrations), `htn_eat_dispatch_system`.

## Batches (suggested order, low-risk first)

**Batch 1 (simple, side-effect-free):** `htn_scout`, `htn_return_surplus`, `htn_play`, `htn_tame_horse`, `htn_plant_from_storage`, `htn_harvest_plant`.

**Batch 2 (craft pipeline — interdependent):** `htn_deliver_material_to_craft_order`, `htn_work_on_craft_order`, `htn_harvest_grain_for_craft_order`. Migrate together; they share `target_craft_order` context.

**Batch 3 (hunt pipeline):** `htn_join_hunt_party`, `htn_engage_prey`, `htn_deliver_hunt_kill`, `htn_combat_faction`. Share hunt-related `PlannerCtx` sub-struct.

**Batch 4 (gather pipeline):** `htn_acquire_food`, `htn_acquire_good`, `htn_stockpile_food`, `htn_eat_dispatch_system`. Highest traffic — migrate last.

**Special handling:** `htn_equip_hunting_spear` + `htn_clear_obstacle` are goal-agnostic (fire on condition, not on `AgentGoal`). Either:
- (a) Leave as side-channel systems outside the `dispatch_for_goal` flow.
- (b) Model as new `AgentGoal::Equip` / `AgentGoal::ClearObstacle` and register via `MethodRegistry`.
- Recommend (a) — they're orthogonal to goal selection.

**Final cleanup:** Delete `htn_dispatch_system` (legacy fallback) once all 21 are migrated and no callers remain.

## Per-migration recipe

For each dispatcher:
1. Register its methods in `MethodRegistry` at startup (one-line entry in `simulation_plugin_setup`).
2. Replace dispatcher body with `dispatch_for_goal(goal, &mut planner_ctx, &mut commands)` call.
3. Verify the `PlannerCtx` sub-fields it relied on are populated by upstream context-builders. If a field is missing, extend the relevant sub-struct populator.
4. Calibration test: queue identical agent state pre- and post-migration; assert same method selected.

## Open questions a real plan must resolve

- **Method ordering inside `MethodRegistry[goal]`.** Does insertion order matter, or is scoring fully deterministic? If order-sensitive, document why.
- **PlannerCtx sub-struct boundary.** Where exactly do hunt fields end and combat fields begin? Some fields (e.g. `prey_target_entity`) span both. Decide on duplication vs shared sub-struct.
- **Legacy `htn_dispatch_system` deletion safety.** It's the fallback path — grep for any system that still routes through it before deletion.
- **System scheduling.** Today all 23 are in `ParallelB` and individually scheduled. After consolidation, is there one parent system that fans out per-goal, or 23 thin wrappers that each call `dispatch_for_goal`? Recommend: thin wrappers initially to preserve Bevy schedule debugability.

## Acceptance criteria

- All 21 dispatchers migrated to `dispatch_for_goal`.
- `htn_dispatch_system` (legacy) deleted.
- `htn.rs` line count drops by ~600.
- No behaviour change on calibration tests.
- Adding a new method = registering it in `MethodRegistry` + implementing `Method` trait — no new dispatch system.
