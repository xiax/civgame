# Storage-First Survival Food Fix

## Summary
- Fix the policy gap where `Survive (Faction has food)` can still choose nearby forage/fish/scavenge over stocked faction storage.
- Make storage withdrawal the first survival food method whenever the hungry worker is empty-handed and a reachable storage tile has edible food.
- Fix `AnyEdible` plant targeting so food plans only target plants that can yield edible food right now, not plants that merely have some edible profile in another season or harvest mode.
- Keep forage, fishing, and loose-food scavenging as fallback behavior when storage has no reachable edible item or storage withdrawal recently fails.

## Implementation Changes

### 1. Encode Storage-First Policy In `AcquireFood`
- In [src/simulation/htn.rs](/Users/xiao1/civgame/src/simulation/htn.rs), update `htn_acquire_food_dispatch_system` after `nearest_storage_tile` and `faction_food_stock` are computed.
- Define a local boolean like `force_storage_withdraw`:
  - true when `nearest_storage_tile.is_some()`;
  - true only for non-SOLO faction members;
  - false when `history.recently_failed_count(MethodId::WITHDRAW_FROM_STORAGE, now) > 0`, so one fresh routing/target failure lets fallback methods run for the next 600 ticks.
- When `force_storage_withdraw` is true, build `PlannerCtx` with:
  - `nearest_storage_tile` and `faction_food_stock` populated;
  - `scavenge_target_entity = None`;
  - `scavenge_target_tile = None`;
  - `gather_target_tile = None`;
  - `fish_spot_tile = None`.
- Leave the existing scavenge/forage/fish discovery code intact, but only expose those candidates to the method registry when `force_storage_withdraw` is false.
- Keep `EatFromInventory` unchanged: if `total_edible(agent, carrier) > 0`, the in-place eat dispatcher still handles it before storage withdrawal.

### 2. Remove The Misleading Reachability Fallback For Storage Priority
- In `nearest_storage_tile` selection, keep the reachable-only scan as the storage-first source of truth.
- Do not treat connectivity-blind edible storage as storage-first eligible; if no reachable edible storage exists, allow forage/fish/scavenge to compete normally.
- Preserve the cached `FactionRegistry::food_stock` only as diagnostic/context stock, not as permission to force a worker toward unreachable storage.

### 3. Tighten `AnyEdible` Plant Validity
- Add a helper in [src/simulation/plants.rs](/Users/xiao1/civgame/src/simulation/plants.rs) or [src/simulation/plant_catalog.rs](/Users/xiao1/civgame/src/simulation/plant_catalog.rs), for example:
  `species_has_current_edible_harvest(species, stage, season, toolkit) -> bool`.
- The helper should return true only when a harvest profile:
  - matches current `GrowthStage`;
  - matches current season for `OnFruitSeason`;
  - satisfies any tool requirement using the worker’s `ToolKit`;
  - yields at least one resource whose class is `Food`.
- Preserve legacy fallback:
  - raw `PlantKind::Grain` and `PlantKind::BerryBush` count as edible when mature;
  - `PlantKind::Tree` does not count as `AnyEdible` unless a species-backed current edible profile is available.

### 4. Apply The Same Edible Rule In Dispatch And Vision
- In `htn_acquire_food_dispatch_system`, add `Option<&ToolKit>` to the worker query and pass the worker’s toolkit plus `calendar.season` into the `gather_target_valid` check.
- Update `gather::is_target_still_valid` so `MemoryKind::AnyEdible` uses the new current-edible helper for species-backed plants instead of `def.yields_food()`.
- In [src/simulation/memory.rs](/Users/xiao1/civgame/src/simulation/memory.rs), update `vision_system` to report `MemoryKind::AnyEdible` for species-backed plants only when the current observer could harvest an edible profile now.
- Keep non-food resource memory behavior stable unless a direct bug appears during implementation; the target symptom is food plans treating non-current fruit trees as edible.

### 5. Improve Inspector Clarity
- In [src/ui/inspector.rs](/Users/xiao1/civgame/src/ui/inspector.rs), keep showing the active HTN method, but make `TaskKind::Gather` state less misleading when possible:
  - `ForageFromKnown` plus a species fruit profile should display a food-oriented label such as `Foraging <resource>` instead of only `Harvesting Tree`;
  - ordinary wood gathering should continue to display tree/wood gathering.
- This is debug/UI clarity only; simulation behavior must be fixed in HTN/plant validity first.

## Test Plan
- Add a regression in [src/simulation/test_fixture.rs](/Users/xiao1/civgame/src/simulation/test_fixture.rs): hungry empty-handed worker, stocked reachable faction storage, and a nearer known grain/berry target. After dispatch, assert:
  - `aq.current == Task::WithdrawFood { tile: storage_tile }`;
  - `aq.peek_next() == Some(Task::Eat)`;
  - no `Task::Gather` is dispatched.
- Add a fallback regression: hungry empty-handed worker, no reachable edible storage, known mature berry/grain. Assert the existing `ForageFromKnown → [Gather, Eat]` chain still works.
- Add a storage-failure fallback regression: seed one recent `WITHDRAW_FROM_STORAGE` failure in `MethodHistory`, provide storage plus nearby forage, and assert forage/scavenge/fish can be selected during the failure TTL.
- Add an out-of-season fruit-tree regression: mature species tree with an edible autumn profile, current season not autumn, injected/visible `AnyEdible` sighting. Assert `AcquireFood` does not dispatch `Task::Gather` for that tree as food.
- Add an in-season fruit-tree positive regression if fruit trees are intended food: mature oak in autumn, hungry worker, no storage. Assert the food gather path yields edible acorns/fruit and does not fell the tree.
- Run focused tests first:
  `cargo test --bin civgame acquire_food_goal_dispatches_withdraw_then_eat_chain`
  `cargo test --bin civgame hungry_agent_forages_then_eats`
  `cargo test --bin civgame agent_on_wheat_tile_dispatches_gather`
- Run the full binary test suite before final delivery:
  `cargo test --bin civgame`.

## Documentation
- Update [src/simulation/CLAUDE.md](/Users/xiao1/civgame/src/simulation/CLAUDE.md) to document:
  - `Survive → AcquireFood` is storage-first when reachable faction edible storage exists;
  - forage/fish/scavenge are fallback methods under storage absence, unreachable storage, or recent storage-withdraw failure;
  - `AnyEdible` plant memory means “currently harvestable edible,” not “species can ever yield food.”

## Acceptance Criteria
- A starving worker with `Faction has food` should withdraw from faction storage before gathering wild food.
- Workers should not harvest/fell trees under a food-survival plan unless the current harvest action actually produces edible food.
- Existing emergency foraging still works when the faction truly has no reachable edible storage.
- The inspector should make the chosen method understandable enough that `Survive (Faction has food)` no longer appears to contradict the active task.
