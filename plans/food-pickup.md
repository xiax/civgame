**Fix Survive Ignoring Nearby Storage/Ground Food**

**Summary**
- Make hungry `Survive` workers trust live world state for storage and loose food instead of waiting on cached storage totals or bucketed `CurrentVision`.
- Keep `ExploreForFood` as a real fallback: it should only run when no concrete live food source is available.

**Implementation Changes**
- In `htn_acquire_food_dispatch_system`, compute storage availability from live `GroundItem`s on faction storage tiles and feed that into the withdraw context, so `WithdrawFromStorageMethod` works even when `FactionStorage.totals` is stale.
- Add a live visible loose-food scan before `CurrentVision.nearest_scavenge_target`, using `SpatialIndex`, edible `GroundItem`s, storage-tile exclusion, reachability, and the same LOS/radius semantics as vision.
- Change AcquireFood method selection so concrete methods (`Withdraw`, `Scavenge`, `Forage`, `Fish`) are scored first; `ExploreForFoodMethod` is considered only if no concrete method precondition is satisfied.
- Add/adjust the `Survive + Gather` preserve arm if needed while touching the food chain, so the forage leg is explicitly protected like `Scavenge`, `WithdrawFood`, `Fishing`, and `Eat`.
- Update `src/simulation/CLAUDE.md` to document live storage/loose-food acquisition and Explore-as-fallback behavior.

**Interfaces / Types**
- No new crate dependencies.
- No player-facing API changes.
- Internal behavior change: `PlannerCtx.faction_food_stock` for AcquireFood should reflect live edible storage availability, not only the cached faction rollup.

**Test Plan**
- Add a regression where storage contains edible `GroundItem`s but `FactionStorage.totals` is manually stale/zero; a hungry Survive worker must dispatch `Task::WithdrawFood`, not `Explore`.
- Add a regression where an edible loose `GroundItem` is in range but the worker’s `CurrentVision` is empty; the worker must dispatch `Task::Scavenge` with trailing `Task::Eat`.
- Add a regression with recent food-method failures in `MethodHistory`; live reachable food must still beat `ExploreForFood`.
- Run focused tests for AcquireFood storage/scavenge/forage, then `cargo test --bin civgame`.

**Assumptions**
- “Nearby food” means faction storage provisions or loose edible ground items, not primarily berry/grain plants.
- Storage tiles remain the intended path for owned provisions; loose food on storage tiles should be withdrawn, not scavenged.
