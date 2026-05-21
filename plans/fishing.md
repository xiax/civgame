# Fishing System Plan

## Summary
Add fishing as a terrain-based food system that plugs into existing technology, memory, HTN goals, food storage, and economy loops. Fishing will be unlocked by the existing Mesolithic `FISHING` tech, produce catalog-driven edible fish resources, deplete and regenerate local water stocks, and remain extensible for weirs, nets, boats, species, and preservation without requiring fish entities per tile.

The first implementation should support historically plausible shore, bridge, marsh, river, lake, and coastal fishing. Boats and constructed weirs should be modeled as later upgrades, not required for baseline fishing.

## Key Changes

- Add a new `fishing` simulation module with tile/spot-level fish ecology:
  - `FishHabitat`: `River`, `Lake`, `Marsh`, `Coast`.
  - `FishingMethod`: start with `Handline` and `Trap`; reserve `Weir`, `Net`, and `BoatLine` for later tech/structure upgrades.
  - `FishStock` stores habitat, current biomass, capacity, last regen tick, seasonal bias, and recent pressure.
  - Stocks are keyed by fishable water tile or deterministic spot tile, not individual fish entities.
  - Stock generation is deterministic from world seed, terrain, water kind, river context, season, and biome.
  - Daily regen runs in `SimulationSet::Economy`; harvest clamps stock to `0..capacity`.

- Define fish resources in the resource catalog:
  - `fish`: edible, perishable, food-tagged, tradeable, counted automatically by `FactionStorage::food_total()`.
  - `preserved_fish`: edible preserved ration, produced by a new smoking/drying recipe gated by existing `FOOD_SMOKING`.
  - Add core ID accessors for hot paths and a simple fish sprite key in the existing sprite library.
  - Keep policies catalog-driven so Mixed/Market economies include fish automatically.

- Add fishing awareness without breaking generic forage:
  - Fishable spots should be recorded as `MemoryKind::Resource(fish)`.
  - Do not report fish as `AnyEdible`, because existing forage methods would try to gather directly from impassable water.
  - Awareness only records spots when the person/faction has `FISHING` awareness or learned knowledge and there is a reachable stand tile.
  - Known fish spots can be used by SharedKnowledge, exploration, and HTN planning like other resource memories.

- Add `TaskKind::Fish` and `Task::Fish { spot_tile, stand_tile, method, output_resource }`:
  - Requires one free hand.
  - Interacts from an adjacent/reachable stand tile, never from the water tile itself.
  - Is labor and can satisfy existing `Survive` and `GatherFood` chains.
  - Executor validates tech, reachability, stock availability, claims, and passable stand tile before doing work.
  - On completion, fish goes into carried inventory when possible, otherwise spills as a ground item at the stand tile.
  - If stock is exhausted or invalidated, release the claim and cancel the tail task cleanly.

- Add `FisheryClaims`:
  - Claim by stock/spot tile and worker to prevent many workers from dogpiling one depleted spot.
  - Claims have TTL and release on success, cancellation, preemption, failed validation, or worker despawn.
  - Dispatchers should prefer unclaimed, higher-stock, closer spots.

- Extend existing HTN food behavior rather than adding a new high-level goal:
  - Under `AgentGoal::Survive`, add a `FishForImmediateFood` method that expands to `Fish -> Eat`.
  - Under `AgentGoal::GatherFood`, add a `FishForStorage` method that expands to `Fish -> DepositToFactionStorage`.
  - Add an `ExploreForFish` fallback using `Explore { kind: MemoryKind::Resource(fish) }` when the faction knows fishing and water is near home but no viable spot is known.
  - Fishing utility should compete with forage/hunt based on expected yield, stock, distance, hunger urgency, season, and existing food-yield multipliers.

- Keep construction optional in v1:
  - Baseline fishing works with no structures once `FISHING` is known.
  - Preserve data hooks for future `FishingWeir`, `Net`, `Dock`, and boat-based methods.
  - Use the existing craft/preservation pipeline for preserved fish first; add dedicated fish racks/weirs only after baseline fishing is stable.

- Historical behavior defaults:
  - Systematic fishing begins with existing Mesolithic `FISHING`.
  - River/coastal fishing is productive but local and depletable.
  - Spring/autumn river runs improve yields; winter reduces most open-water yields.
  - Marsh/lake/coast habitats differ in capacity and regeneration.
  - Salt or brackish water can be fishable even when not drinkable.
  - Boats from `LOG_RAFT` and `DUGOUT_CANOE` can later unlock farther/deeper spots and higher methods.

## Integration Details

- Scheduling:
  - Fish spot awareness runs after movement/vision or as a nearby dedicated simulation system.
  - HTN method selection remains in `ParallelB`.
  - `fish_task_system` runs in `Sequential`, near gather/production, after movement and before eating/deposit follow-up.
  - Fish stock regeneration runs in `Economy` on a daily cadence.

- Terrain and reachability:
  - Fishable water includes `River`, `Marsh`, fresh/salt lake/coast `Water`, and possibly `Bridge` as a stand/fishing access tile.
  - Fishing always separates `spot_tile` from `stand_tile`.
  - Stand tiles must be passable, reachable from the worker, and adjacent to the fishable spot.
  - River-bank logic should prefer same-bank reachable stands and avoid routing workers into impassable water.

- Economy and food:
  - Fish deposits use existing `DepositToFactionStorage`.
  - Stockpiled fish should satisfy existing food/calorie chief postings.
  - Eating fish should work through existing edible-resource logic.
  - Preserved fish should be treated like other preserved rations by the two-pass eating behavior.

- Skills and activity:
  - Add `ActivityKind::Fishing` so fishing can drive tech learning/adoption separately from generic foraging.
  - Prefer adding `SkillKind::Fishing` for extensibility; update fixed-size skill arrays, labels, tests, and UI inspectors accordingly.
  - Hunters and general workers may fish; no new `Fisher` profession is required for v1.

## Test Plan

- Unit tests:
  - Fishable classification handles river, marsh, lake/coast water, bridge access, and non-water rejection.
  - Stand-tile search never chooses impassable water and respects reachability.
  - Fish stock initialization is deterministic and stock regen/depletion clamps correctly.
  - Seasonal multipliers affect yield without making stock negative.
  - `TaskKind::Fish` labels, hand requirements, labor status, and task mapping are correct.
  - HTN methods expand to `Fish -> Eat` and `Fish -> DepositToFactionStorage`.
  - Fish memories are recorded as `Resource(fish)`, not `AnyEdible`.
  - Fish and preserved fish are edible and counted by faction food totals.
  - Preserved fish recipe is gated and produces the expected resource.

- Integration tests:
  - A faction with `FISHING`, low food, and a nearby river dispatches a worker to fish, then eat or deposit.
  - A faction without `FISHING` does not select fishing and falls back to current food behavior.
  - Multiple workers spread across available fish spots instead of exhausting one claimed spot.
  - Depleted spots stop attracting workers until regeneration.
  - Salt coastal water is fishable but still not drinkable.
  - Fish deposits satisfy existing chief food stockpile demand.
  - Mixed/Market policy presets include fish through catalog-driven resource policy.

- Verification command:
  - Run focused fishing tests first, then `cargo test --bin civgame`.

## Assumptions

- Baseline v1 fishing is shore/bridge/bank fishing only; no boat movement or deep-water travel yet.
- Fish are modeled as local renewable stocks, not spawned animals.
- Existing `FISHING`, `FOOD_SMOKING`, `LOG_RAFT`, and `DUGOUT_CANOE` techs are reused instead of adding new techs immediately.
- No new crates are needed.
- Construction features like weirs, docks, nets, and fish racks are designed into the data model but can be implemented after the core loop is working.
