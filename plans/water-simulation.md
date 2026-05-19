# Flowing Water, Reservoirs, Aquifers, And Dams

## Summary
Implement water as two connected layers:

- **Worldgen hydrology truth:** globe generation computes drainage, sources, reservoirs, river discharge, water levels, riverbeds, banks, aquifers, and salinity deterministically.
- **Loaded-chunk runtime water:** only loaded chunks simulate live water movement. Runtime effects are terrain/passability, drinking, fertility, settlement/animal water logic, and pathfinding updates. No drowning, building damage, or flood panic AI in v1.

This replaces “river tiles stamped one z lower” with water columns that have separate **ground/bed z**, **water level z**, **depth**, **flow direction**, and **reservoir/source identity**.

## Key Interfaces
- Add hydrology data to `Globe`:
  - `HydrologyMap { cells, reservoirs, sources }`
  - `HydroCell { filled_height, raw_height, flow_dir, flow_accum, discharge, reservoir_id, aquifer_level_z, aquifer_yield }`
  - `Reservoir { id, kind: Ocean | Lake | Wetland | Spring | Aquifer | Dam, level_z, outlet, capacity, salinity }`
  - Extend `RiverEdge` with `discharge`, `order`, `from_level_z`, `to_level_z`, `from_depth`, `to_depth`, `source_id`, and downstream/reservoir ids.
- Extend chunk water state without breaking existing callers:
  - Keep `surface_z_at()` as “visible/top surface,” which is water surface for wet columns.
  - Add `ground_z_at()`, `water_level_at()`, `water_depth_at()`, `water_column_at()`, and `set_water_column()`.
  - Add per-column cached `ground_z`, `ground_kind`, and `WaterColumn { level_z, bed_z, depth, volume, kind, flow_dir, reservoir_id, salinity, source_rate }`.
- Add water runtime systems:
  - New `simulation::water` or `world::water` module with `WaterRuntime`, `WaterDirtyQueue`, `WaterChangedEvent`.
  - Convert water changes into existing `TileChangedEvent` when passability/rendered kind changes, so rendering and pathfinding invalidation continue to work.
- Add construction support:
  - Add `BuildSiteKind::Dam`, `Dam`, and `DamMap`.
  - Add `TileKind::Dam` as passable, non-water-like, flow-blocking terrain.
  - Gate dams on `BRIDGE_BUILDING` for v1, with a stone/wood recipe and player right-click placement on river/lake/ocean water tiles.
  - Existing `Bridge` stays passable and water-like; dams block flow and create/raise upstream runtime reservoirs.

## Implementation Changes
- Rework globe hydrology:
  - Preserve both pre-fill terrain height and pit-filled spill height.
  - Compute weighted discharge from rainfall, aquifer recharge, and upstream area instead of raw cell count only.
  - Identify reservoirs from actual basin clusters, not circular lake discs.
  - Assign ocean cells a constant sea level, lakes/wetlands their basin spill level, springs/headwaters a source rate, and aquifers a local water table.
  - Extract rivers as a drainage graph with confluences; widths/depths increase by discharge/order and narrow on steep headwater slopes.
  - Enforce monotonic downstream water levels, allowing steep segments to read as rapids/waterfalls only when the terrain gradient is actually steep.
- Rework chunk generation:
  - Generate dry terrain first, then stamp water columns from hydrology metadata.
  - River channel tiles get `bed_z = water_level_z - depth`, not a fixed `surface_z - 1`.
  - Immediate banks are shaped from the local terrain and river context: normal rivers get dry banks around `water_level + 1`; deltas/wetlands become marsh/shallow water; mountain rivers may form deeper gorges.
  - Lakes use basin membership and spill level, not center/radius discs.
  - Ocean water uses constant sea level with coastal banks/beaches.
  - Aquifers do not flood all underground rock by default; exposed caves/dug cells below the water table become water sources.
- Add loaded-chunk water simulation:
  - Run at a fixed cadence, default every 5 fixed ticks, with a per-tick active-cell cap.
  - Active water columns exchange volume with cardinal neighbors based on water surface height, bed height, source rate, sinks, and dam barriers.
  - Rivers/reservoirs loaded from globe data act as stable boundary conditions; unloaded neighbors are ghost boundaries, not simulated cells.
  - Digging/filling/construction marks nearby cells dirty so channels, wells, breached reservoirs, and dammed rivers update locally.
  - Runtime-created reservoirs from dams are stored in `WaterRuntime`, scoped to loaded chunks.
- Integrate gameplay consumers:
  - Drinking uses hydrology salinity/source metadata instead of biome-only `water_kind_at`.
  - Wells store aquifer-derived yield/depth; dry wells cannot satisfy drink tasks.
  - Animals, nomads, settlement scoring, fertility, and river-distance logic continue to prefer fresh reachable water.
  - Pathfinding sees `Water`/`River` as impassable, `Bridge` as passable water-like, and `Dam` as passable non-water flow blocker.
- Update documentation:
  - Bump `GLOBE_FILE_VERSION`.
  - Update `src/world/CLAUDE.md` and root `AGENTS.md` for the new hydrology, chunk water cache, dam, well, and water-simulation rules.

## Test Plan
- Worldgen tests:
  - Rivers terminate in ocean, wetland, lake outlet, or valid pole sink.
  - River water levels are monotonic downstream.
  - River width/depth/discharge increase at confluences and major downstream segments.
  - Lake reservoirs have consistent level, outlet, capacity, and non-disc basin shape.
  - Ocean water is constant sea level.
  - Aquifer water tables stay below terrain except wetlands/springs.
- Chunk generation tests:
  - River channel bed is below water level by computed depth.
  - Banks are dry or marsh according to river context.
  - Lake/ocean tiles render at reservoir level and retain salinity.
  - `tile_at_3d` returns water for wet z slices and ground/bed material below.
- Runtime water tests:
  - Water flows downhill, fills a basin to spill level, then exits.
  - Exposed aquifer cells refill to water table.
  - A dam blocks downstream flow, raises upstream water, and spills when overtopped.
  - Bridge does not block flow and remains drinkable/passable.
  - Water changes emit path/render invalidation events.
- Gameplay tests:
  - Agents drink from fresh river/spring/well/lake water and reject salt/brackish water.
  - Dry wells fail gracefully.
  - Pathfinding reroutes after local flooding or dam placement.
- Verification command:
  - `cargo test --bin civgame`

## Assumptions
- Runtime water simulation is limited to loaded chunks; worldgen hydrology remains the source of truth outside loaded areas.
- V1 water affects terrain, pathing, drinking, wells, fertility, settlement scoring, and animals, but does not add drowning, structural damage, flood evacuation AI, or autonomous AI dam planning.
- No new crates are needed.
