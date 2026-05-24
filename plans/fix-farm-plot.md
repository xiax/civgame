**Fix Farm Plot Plant Clearing**

**Summary**
- Split “wild plants must not seed here” from “anything here should be destroyed.”
- Agricultural plot tiles should clear existing plants only when the plot is created, then allow deliberate crop planting normally.
- Settlement/road/doormat reservations stay protected from later wild plants and obstacles.

**Key Changes**
- Remove `PlotIndex::ag_tiles` from persistent `SeedReservation`.
- Add one-time plant cleanup to agricultural plot creation in `land::carve_plots_system`: when a new Agricultural tile is inserted into `ag_tiles`, despawn any existing plant on that tile without creating mature harvest yields.
- Keep wild plant suppression for fields by making wild plant spawn/scatter checks consult `PlotIndex::ag_tiles` directly, separate from `SeedReservation`.
- Keep `react_obstacle_under_structure_system` reactive only for real occupied/reserved settlement surfaces: structures, blueprints, doormats, and planned roads, not farm plots.
- Make `resolve_clear_yields` stage-aware so clearing an immature plant cannot produce mature grain/seed output.
- Add an explicit season guard so private/autonomous `Farm` planting only dispatches during `SpringPrepPlant`; Autumn `Farm` workers harvest instead of planting first.

**Test Plan**
- Regression: create an Agricultural plot over an existing plant; the plant is removed once at plot creation.
- Regression: plant a grain seed on an Agricultural tile; reactive cleanup must not delete it or spawn grain products.
- Unit test: clearing `GrowthStage::Seed` or `Seedling` grain never yields mature grain.
- Dispatch test: Autumn private `AgentGoal::Farm` with mature grain chooses harvest, not `PlantFromStorage`.
- Run `cargo test --bin civgame plant -- --nocapture` plus the new targeted farm/obstacle tests.

**Assumptions**
- Plot creation cleanup should remove pre-existing plants without rewarding harvest goods.
- Wild chunk seeding and wild scatter should still avoid Agricultural plot tiles.
- Reordering all plant seeding before settlement generation is not enough by itself because chunks and wild scatter happen later too; the durable fix is separating wild-spawn exclusion from obstacle cleanup.
