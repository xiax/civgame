# HTN Dispatcher Consolidation

**Status:** Multi-method batch shipped (2026-05-23). Single-method promotion + side-channel rename deferred as enumerated below — both worth doing per `feedback_future_proof_ok.md`, both separable from this batch.

## What landed in this batch (7 dispatchers)

All seven now route through `dispatch_for_goal` (htn.rs:585). The hand-rolled `methods_for(...).iter().filter(...).max_by(...)` argmax pattern is gone from each.

- `htn_dispatch_system` (legacy Sleep) → renamed `htn_sleep_dispatch_system` (htn.rs:~3540).
- `htn_engage_prey_dispatch_system` (htn.rs:~8290).
- `htn_join_hunt_party_dispatch_system` (htn.rs:~8653).
- `htn_combat_faction_dispatch_system` (htn.rs:~9039) — dropped the unused `AbstractTaskKind` tuple field at the per-goal match.
- `htn_acquire_food_dispatch_system` (htn.rs:~3996).
- `htn_acquire_good_dispatch_system` (htn.rs:~4686, two argmax sites: Stockpile + Haul).
- `htn_stockpile_food_dispatch_system` (htn.rs:~5423).

The terminal-`Explore` synthetic backstop in `htn_acquire_food_dispatch_system` + `htn_stockpile_food_dispatch_system` stays — fires when even the `MF_FALLBACK_ONLY` Explore method's precondition fails. `score_method_with_history_and_disposition`'s "Phase E / 3 dispatchers" doc comment was rewritten.

## Concrete-vs-fallback partition: now declarative

`MethodFlags::MF_FALLBACK_ONLY` (htn.rs:340) is the new flag. `dispatch_for_goal` does a two-pass pick: concretes first, then fallback methods only if no concrete picked. Encodes `feedback_live_preconditions_no_bias` on the method itself so future dispatchers can't accidentally let an Explore win against a history-biased concrete. Set on `ExploreForFoodMethod`, `ExploreForMaterialMethod`, `ExploreForFoodForStorageMethod`. `htn_acquire_food_dispatch_system`'s explicit `MethodId::EXPLORE_FOR_FOOD` exclusion list is gone.

## Acceptance vs targets

- `htn_dispatch_system` (legacy Sleep): deleted (renamed; the misleading name is gone). ✓
- Seven in-scope dispatchers route through `dispatch_for_goal`. ✓
- `wc -l src/simulation/htn.rs`: **14627 → 14539 (-88)**. Plan target was ≥300 — missed. The hand-rolled argmax block was only ~12 lines per site; the win is structural (one canonical pattern + declarative partition), not line count.
- All 1077 tests pass. No behaviour change on baseline / hunt / raid / combat / wage-aware / smoke fixtures.

## Follow-ups (good future-proofing moves, separable from this batch)

### Single-method promotion to registry-driven

Six dispatchers today are "direct dispatch" — one inline expand, no method. Per `feedback_future_proof_ok.md`, route them through the registry too (one `Method` impl wrapping today's inline expand, one `reg.register(...)` line):

- `htn_prepare_field_dispatch_system` (htn.rs:7521)
- `htn_plant_from_storage_dispatch_system` (htn.rs:7154) — single `WithdrawAndPlantSeedMethod` already registered; dispatcher still has bespoke argmax to drop
- `htn_build_claimed_blueprint_dispatch_system` (htn.rs:7660)
- `htn_work_on_craft_order_dispatch_system` (htn.rs:9922)
- `htn_harvest_grain_for_craft_order_dispatch_system` (htn.rs:10160)
- `htn_deliver_material_to_craft_order_dispatch_system` (htn.rs:9542)

For each: one `Method` impl, one register call, dispatcher body shrinks to the `dispatch_for_goal` template. Adding a second method later (e.g. ox-drawn prepare-field, skill-tiered craft variant) becomes a one-line registration.

### Side-channel rename (cosmetic, no logic change)

These fire on world state, not on `AgentGoal` — the `_dispatch_` suffix is misleading. Drop it:

- `htn_equip_hunting_spear_dispatch_system` → `equip_hunting_spear_system`
- `htn_deliver_hunt_kill_dispatch_system` → `deliver_hunt_kill_system`
- `htn_clear_obstacle_dispatch_system` → `clear_obstacle_system`
- `bureaucrat_admin_dispatch_system` → `bureaucrat_admin_system`

Touches `src/simulation/mod.rs` scheduling chain. No behaviour change.

## Reference template

`htn_socialize_dispatch_system` (htn.rs:~8858) is the canonical shape: goal gate → idle gate → build `PlannerCtx` → `dispatch_for_goal(..)` → expand head → route via `assign_task_with_routing` → record `MethodOutcome::FailedRouting` on failure → enqueue tail. Migrate any new goal off this template.
