# Fix Hungry Survivors Choosing Explore Over Live Food

## Context

A hungry agent on `AgentGoal::Survive` should withdraw from faction storage or scavenge a visible edible item before falling through to `Task::Explore`. Today the agent sometimes walks off exploring even when:

- Faction storage tiles physically hold edible `GroundItem`s, or
- A loose edible `GroundItem` sits within vision range.

Root cause is split across `htn_acquire_food_dispatch_system` (`src/simulation/htn.rs:3985`) and `goal_dispatch_system` (`src/simulation/tasks.rs`):

1. **Redundant cached gate.** `nearest_storage_tile` is *already* computed via a live `SpatialIndex + GroundItem` scan (`htn.rs:4084-4119`), so when the picker returns `Some(tile)` the tile **is known** to hold edible items. But `WithdrawFromStorageMethod::precondition` also requires `ctx.faction_food_stock > 0`, and `faction_food_stock` is filled from `FactionRegistry::food_stock(...)`, which is `compute_faction_storage_system`'s Economy-cadence cached rollup. When the rollup is stale at 0 (between sync passes, or after a recent consumption tick), the method's precondition fails even though `nearest_storage_tile` is `Some` and points at real food.

2. **Vision-only scavenge picker.** `CurrentVision::nearest_scavenge_target` (`htn.rs:4151`) is the only scavenge source. `vision_system` runs on a ~20-tick bucket-active cadence; until the next bucket fires, a freshly-spawned or freshly-dropped edible `GroundItem` next to the agent is invisible to the dispatcher. With no scavenge target and (1) blocking storage, only `ExploreForFoodMethod` remains.

3. **`MethodHistory` failure penalty can invert concretes against Explore.** Argmax uses `score_method_with_history = utility − failures × 0.4`. Two recent failures push `WithdrawFromStorage` (1.0) to `0.2`, below `ExploreForFoodMethod` (`UTIL_EXPLORE_FALLBACK = 0.3`). This bias is still useful *among concretes* (it's what drives `FishForImmediateFoodMethod`'s depleted-spot avoidance — its precondition is partial-live and doesn't check `FishStock`), but it should not be able to demote any concrete *below the Explore fallback*. Once preconditions go live (fixes 1 + 2), a "failure" from stale data doesn't recur — but Fish-style partial-live preconditions still benefit from intra-concrete bias.

4. **Missing preserve arm for the forage chain.** `ForageFromKnownMethod` expands `[Task::Gather, Task::Eat]` under `Survive`. `tasks.rs:893-1180` preserves `(Survive, Eat)`, `(Survive, WithdrawFood)`, `(Survive, Scavenge)`, `(Survive, Fishing)`, `(Survive, Explore)` — but **not** `(Survive, Gather)`. A forage chain mid-walk gets stale-reset every dispatcher tick, dropping the agent back into argmax.

## Recommended Approach

A minimal, behaviour-preserving fix in four edits. Each addresses one root cause; together they make the user's plan goal hold without changing the rest of the food-acquisition design.

### 1. Make `nearest_storage_tile` the single source of truth for storage availability

In `htn_acquire_food_dispatch_system` (`src/simulation/htn.rs:3985`):

- Keep the existing live `SpatialIndex + GroundItem` scan that already verifies an edible item exists on the tile (`htn.rs:4104-4111`).
- Derive `faction_food_stock` from that scan: when `nearest_storage_tile.is_some()`, set `faction_food_stock = 1` (sentinel — the method only checks `> 0`). When `None`, keep `0`.
- Optionally also fold the cached `faction_registry.food_stock(...)` value in as `max(live_sentinel, cached)` so the variable still reflects real availability when storage tiles are unreachable but pack-inventory totals exist.

Net effect: `WithdrawFromStorageMethod::precondition` (`ctx.faction_food_stock > 0 && ctx.nearest_storage_tile.is_some()`) becomes consistent — both halves either pass or fail together. No precondition change needed; no other dispatcher that builds `PlannerCtx` is affected because they all set `faction_food_stock: 0` and never compute a storage tile via the live scan.

### 2. Add a `SpatialIndex` live scavenge scan as a fallback to `CurrentVision`

After computing the existing `current_vision.nearest_scavenge_target(...)` result (`htn.rs:4151`), when the result is `None`, run a bounded live scan:

- Iterate `spatial` entries within chebyshev `VIEW_RADIUS` (the same radius `vision_system` uses) around the agent.
- For each candidate of `IndexedKind::GroundItem`, look up the `GroundItem` component, require `item.resource_id.is_edible() && qty > 0`.
- Apply the same storage-tile exclusion (`storage_tile_map.tiles.contains_key(&t)`).
- Apply the same `reach_from_agent` reachability gate (already defined in scope at `htn.rs:4135`).
- Apply LOS via the existing `line_of_sight` helper at eye height `current_z + 1`.
- Rank by the existing `detour_dist` closure (`htn.rs:4147`); pick the minimum.

Set `scavenge_target_entity` / `scavenge_target_tile` from the live result when vision was empty. No method changes — `ScavengeFoodFromGroundMethod` already gates on `ctx.scavenge_target_entity.is_some()`.

Bound the scan: cap iterations at `VIEW_RADIUS²` tiles and break on first acceptable hit ranked by chebyshev (pre-detour cheap rank, then refine with detour over the few-candidate shortlist). The scan only runs when vision was empty, so it costs nothing in the common case.

### 3. Concrete-first partition; keep `score_method_with_history` among concretes

In the argmax block (`htn.rs:4287-4296`), partition the candidate set:

- **Concretes** (Withdraw / Scavenge / Forage / Fish): argmax via `score_method_with_history(...)` (current scoring intact).
- **Fallback** (Explore): only considered when no concrete's precondition passes.

Rationale:

- Withdraw / Scavenge / Forage have fully-live preconditions (fixes 1 + 2). For these, failure bias is theoretically redundant — but the implementation cost of keeping it is zero and it harmlessly biases among concretes (e.g. a route that just failed shifts the pick to a sibling concrete next tick).
- `FishForImmediateFoodMethod` is the load-bearing case: its precondition (`fish_spot_tile.is_some()`) verifies a *water tile* exists, but **not** that `FishStock` has biomass. Depleted-spot avoidance was explicitly designed to use `MethodHistory` failure-bias (see `simulation/CLAUDE.md` Fishing section) — the executor records `record_target_failure` on no-catch and the next dispatcher tick biases the method down so forage/scavenge wins. Dropping bias entirely would let a depleted spot keep re-firing until `chronic_failure_release` (much slower).
- Partitioning Explore out of the argmax solves the user's original complaint (hungry agent walks off exploring instead of withdrawing): a heavily-biased concrete can no longer fall below `UTIL_EXPLORE_FALLBACK` and let Explore win. Concretes compete among themselves; Explore only runs when none is viable.
- `ExploreForFoodMethod` keeps its `UTIL_EXPLORE_FALLBACK = 0.3` utility unchanged — purely structural separation.

Implementation: filter methods by `id() != MethodId::EXPLORE_FOR_FOOD`, argmax over those via `score_method_with_history`; if empty (no concrete passes precondition), find `EXPLORE_FOR_FOOD` and use it. The existing terminal-Explore fallback path (`htn.rs:4298-4342`) remains as the next layer when even `ExploreForFoodMethod`'s precondition fails (rare — its precondition is just `hunger ≥ EAT_TRIGGER_HUNGER`).

### 4. Preserve the `Survive + Gather` forage chain

In `goal_dispatch_system` (`src/simulation/tasks.rs:893`-ish), add one arm next to the existing `(Survive, Scavenge)` arm:

```
AgentGoal::Survive if aq.current_task_kind() == TaskKind::Gather as u16 => {
    Some(TaskKind::Gather as u16)
}
```

This mirrors the Scavenge / WithdrawFood / Fishing / Eat / Explore preserves under Survive and protects the `ForageFromKnownMethod` expansion `[Gather, Eat]`.

### 5. Flatten the `AcquireFood` utility tiers

Drop `ScavengeFoodFromGroundMethod`'s `UTIL_VISIBLE_GROUND (1.5)` baseline to `UTIL_BASELINE (1.0)` so all four concrete food methods score on the same tier with distance as the only differentiator:

- `WithdrawFromStorageMethod`: `1.0 − dist_penalty`
- `ScavengeFoodFromGroundMethod`: `1.0 − dist_penalty` *(was `1.5 − dist_penalty`)*
- `ForageFromKnownMethod`: `1.0 − dist_penalty` *(unchanged)*
- `FishForImmediateFoodMethod`: `1.0 − dist_penalty` *(unchanged)*
- `ExploreForFoodMethod`: `0.3` *(unchanged — last resort)*

Rationale: once preconditions are live, all four concretes are equally "verified available." The original 1.5 premium on scavenge was inherited from a model where vision-confirmed targets were more trusted than memory-derived ones. With live `SpatialIndex` scanning, that distinction disappears — every concrete method is reading the same kind of verified live data.

What this gives up: the previous bias where an equidistant scavenge target was preferred over a storage tile. If a perishability nudge is needed later (loose food despawns; storage is durable), a small +0.05 bump on Scavenge is the principled add-back — distance still dominates ordering at that magnitude.

Out of scope: `AcquireGood`'s `ScavengeFromGroundMethod` (still `UTIL_VISIBLE_GROUND`) and `UTIL_CLAIMED_HAUL = 2.0` for haul-to-blueprint. Those encode different intents (material gather with cached availability; committed claim respectively) and aren't part of this fix.

### 6. Documentation

Update `src/simulation/CLAUDE.md`:

- Under **Memory & gathering** → "CurrentVision vision-first dispatch": note that `htn_acquire_food_dispatch_system` falls back to a bounded live `SpatialIndex` scan when vision is empty (one bucket cadence of staleness no longer hides edible drops).
- Under **HTN domain** → "Methods (highlights)": document `AcquireFood` scoring — all four concrete methods at `UTIL_BASELINE = 1.0 − dist_penalty`; Explore at `0.3` as last resort.
- Under **HTN domain** → "AcquireFood dispatch": document the concrete-first partition — `score_method_with_history` argmaxes over Withdraw/Scavenge/Forage/Fish; `ExploreForFoodMethod` only runs when no concrete's precondition passes. This preserves Fish's `MethodHistory`-based depleted-spot avoidance while blocking the inversion-against-Explore that motivated the fix.
- Under "Method-design rules": note that tier baseline differences (`UTIL_BASELINE` / `UTIL_VISIBLE_GROUND` / `UTIL_CLAIMED_HAUL`) encode *intent*, not *availability* — when availability is uniformly verified across a method group, the group should share a baseline (food methods now all at `UTIL_BASELINE`).

## Critical Files

- `src/simulation/htn.rs` — `htn_acquire_food_dispatch_system` (lines ~3985-4350): live-derived `faction_food_stock`, live scavenge fallback, partitioned argmax.
- `src/simulation/tasks.rs` — `goal_dispatch_system` preserve arms (lines ~893-912): add `(Survive, Gather)`.
- `src/simulation/CLAUDE.md` — documentation.
- `src/simulation/memory.rs` — read-only reference for `VIEW_RADIUS` and the `nearest_scavenge_target` storage-exclusion / reachability signature being mirrored.
- `src/simulation/line_of_sight.rs` — read-only, reuse existing LOS helper.

Out of scope for this plan: changing `vision_system` cadence, changing `compute_faction_storage_system` cadence, or refactoring `PlannerCtx`. All four edits are local and reversible.

## Reusable Existing Code

- Live storage scan loop and `reach_from_agent` closure — already present at `htn.rs:4084-4142`; share the closure with the new scavenge scan.
- `CurrentVision::nearest_scavenge_target` signature (storage-exclusion + reachability + detour distance) — mirror its filter set in the fallback scan.
- `detour_est.tiles(...)` — already constructed in scope (`htn.rs:4145`).
- `pick_explore_tile` and the terminal-Explore fallback (`htn.rs:4298-4342`) — keep unchanged; it is the relief valve when no concrete method *and* `ExploreForFoodMethod` both fail to find anything.
- `score_method_with_history` and `MethodId::*` — unchanged; only the candidate-set partitioning changes.
- The `(Survive, X)` preserve-arm pattern in `tasks.rs:893+` — copy the Scavenge arm exactly.

## Verification

End-to-end testing — favour behavioural fixture tests over `info!` instrumentation, since this fix is about decisions that flip on visible state.

1. **`cargo test --bin civgame`** for the existing `htn::tests` module to confirm no method precondition / scoring tests regress.
2. **New fixture tests** exercising the three behavioural invariants:
   - `acquire_food_picks_withdraw_when_storage_scan_finds_food` (pure ctx test in `htn::tests`): build a ctx with `nearest_storage_tile = Some(_)`, derived `faction_food_stock = 1`, no scavenge target, no gather target, hunger ≥ trigger. Argmax returns `WithdrawFromStorageMethod`.
   - `acquire_food_picks_scavenge_when_vision_empty_but_live_scan_finds_item` (uses `test_fixture.rs::TestSim`): place an edible `GroundItem` adjacent to a hungry agent, force `CurrentVision` to be empty (skip a vision bucket via `tick_n` count), run one dispatcher tick, assert `aq.current` is `Task::Scavenge`.
   - `acquire_food_ignores_method_history_when_live_target_exists` (TestSim): seed `MethodHistory` with two `FailedRouting` entries for `WITHDRAW_FROM_STORAGE`; provide a live reachable storage tile holding `grain` `GroundItem`s; tick one dispatcher pass; assert `aq.current` is `Task::WithdrawFood`, not `Task::Explore`.
3. **Existing onenter / spawn fixtures**: re-run `cargo test --bin civgame` to confirm nothing in seed/migration/farm pipelines broke (Survive→Eat is a hot path during the spawn-time food buffer drawdown).
4. **In-game smoke**: `cargo run`, advance Spring of a Subsistence start, open the inspector on a hungry adult. Verify the activity log shows `WithdrawFood` / `Scavenge` / `Gather`-then-`Eat`, not `Explore`, while storage holds grain or while loose berries are within sight. Per memory, **never use `cargo run -- --sandbox`** for testing.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| Cascade dispatcher (try Withdraw → Scavenge → Forage → Fish → Explore in order) | Loses distance-based ordering among concretes — a closer scavenge target legitimately outranks a far storage tile via `UTIL_VISIBLE_GROUND − dist_penalty`. |
| Drop `MethodHistory` bias entirely from `AcquireFood` argmax (raw utility) | Regresses fishing: `FishForImmediateFoodMethod` has a partial-live precondition (water tile present ≠ `FishStock` non-empty) and its depleted-spot avoidance is failure-bias-based by design. The partition (chosen approach) keeps bias among concretes but blocks the inversion-against-Explore that motivated the question. |
| Per-target failure bias keyed on `(method_id, target_tile)` | Adds per-agent state; the live picker already excludes the previously-failed target on the next tick (filter by `is_edible() && qty > 0`, reachability, LOS), so this would just duplicate the picker's work. |
| Cap the failure penalty so concretes can't fall below `UTIL_EXPLORE_FALLBACK` | Tunes a magic number rather than addressing the principle (live preconditions don't need bias). |

The chosen design articulates a principle the rest of the codebase can apply: **failure bias is for ordering *among* viable concretes; a fallback like Explore should be structurally separated rather than competing in the same argmax**. Within concretes, `score_method_with_history` continues to do useful work (Fish's depleted-spot bias, Withdraw's per-route bias against an unreachable storage tile). Outside concretes, Explore is a partition fallback, not a scoring competitor.

## On the Original Plan

The original `food-pickup.md` is directionally correct. This plan retains all five of its bullets and tightens three details the user's draft glossed:

- The live storage scan is already implemented; the actual stale gate is the redundant cached `faction_food_stock`. Fixing it via a derived sentinel is cheaper than recomputing the live scan.
- Explore-as-fallback is enforced by partitioning the candidate set: argmax over concretes via the existing `score_method_with_history`, fall through to Explore only when no concrete's precondition passes. (An earlier revision proposed dropping failure bias entirely for `AcquireFood`; that regressed fishing's depleted-spot avoidance, which deliberately relies on `MethodHistory` — see §3.)
- The forage chain preserve arm the user flagged is genuinely missing (Scavenge / WithdrawFood / Fishing are preserved but Gather under Survive isn't). One-line addition.

After approval, mirror this plan to `/Users/xiao1/civgame/plans/food-pickup.md` per the local-plan-files convention.
