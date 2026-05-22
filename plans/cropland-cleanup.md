**Cropland And Personal Gardens Cleanup**

**Summary**
Make `TileKind::Cropland` plot-bound: visible Cropland only appears inside Agricultural plots. Fertility controls nutrients/yield, not whether villagers till part of their own field. Personal backyard gardens stay as real Agricultural parcels/plots, resized to match house geometry.

**Key Changes**
- Update `seed_starting_farms_system` so the starter patch is selected inside the seeded Agricultural plot and every selected tile becomes `Cropland` regardless of fertility or mosaic role.
- Keep `FieldTileIndex` entries for every Agricultural plot tile; use natural fertility as nutrient ceiling/yield input, never as a Cropland eligibility gate.
- Remove/disable seed-time `seed_farmstead_yard` Cropland stamping. Seeded houses must not paint fake out-of-zone farm tiles.
- Replace the fixed 4x4 kitchen-garden parcel in `append_kitchen_gardens` with house-sized real Agricultural parcels:
  - Hut gardens: `3x3`.
  - Longhouse gardens: `3x4`.
  - Longhouse gardens attach to a 3-tile short wall, so the shared house-wall edge and garden edge are the same length.
  - Do not attach a longhouse garden to the 5-tile long wall; if neither short-wall side is clear/reachable, skip that personal garden for that survey.
- Keep gardens as true Agricultural parcels that flow through normal plot carving, household child-plot claiming, field prep, and Cropland creation.
- Adjust the garden placement overlap check so the attached garden may abut its owning residential parcel/house wall, while still avoiding roads, other parcels, blocked terrain, and unreachable pockets.
- Update simulation docs to state: Cropland is Agricultural-plot-only; personal gardens are real 3x3/3x4 Agricultural parcels, not hidden seed-time yard stamping.

**Tests**
- Startup farm test: every seeded Cropland tile is inside `PlotIndex.ag_tiles` and an Agricultural `Plot`.
- Low/mixed-fertility seeded-plot test: selected starter patch tiles still become Cropland.
- Residential seeding test: seeded houses do not create Cropland outside Agricultural plots.
- Kitchen-garden geometry tests: huts emit `3x3`, longhouses emit `3x4`, longhouse shared edge is length 3, and no fixed `4x4` garden remains.
- Run `cargo test --bin civgame`.

**Assumptions**
- Visible Cropland outside an Agricultural plot is always a bug.
- Personal backyard gardens are real gameplay plots from the kitchen-garden system.
- Longhouse gardens intentionally use the short wall, not the long rear wall.
