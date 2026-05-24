# Realistic Terrain Relief Overhaul

## Summary
Replace the current biome-amplitude `surface_v` terrain with a full natural-status geomorphology layer. Terrain z variability will be driven by biome plus natural world-map status: elevation band, slope, local relief, coast/river/lake/wetland state, aquifer depth, and mountain proximity. Settlement/exploration/faction state will not affect terrain generation.

This is a breaking worldgen retune: bump `GLOBE_FILE_VERSION`, regenerate `world.bin`, and keep the existing chunk/pathfinding architecture.

## Public Interfaces
- Add serialized relief data in [src/world/globe.rs](/Users/xiao1/civgame/src/world/globe.rs):
```rust
pub enum ReliefClass {
    OceanShelf,
    CoastalPlain,
    Floodplain,
    BasinWetland,
    LowlandPlain,
    RollingHills,
    UplandPlateau,
    Badlands,
    Foothills,
    MountainSlope,
    MountainRidge,
}

pub struct WorldCell {
    pub relief: ReliefClass,
    pub slope: u8,
    pub local_relief: u8,
    // existing fields unchanged
}
```
- Add `Globe::sample_relief(tile_x, tile_y) -> ReliefSample` with interpolated slope/local-relief and nearest/dominant relief class.
- Replace `terrain::surface_v` internals in [src/world/terrain.rs](/Users/xiao1/civgame/src/world/terrain.rs) with a continuous `surface_z_f32(...)` pipeline, keeping `surface_height(...)` as the public discrete-z entry point.
- Add world-map/spawn tooltip support for relief status and estimated z range in [src/ui/world_map.rs](/Users/xiao1/civgame/src/ui/world_map.rs) and spawn-select.

## Implementation Changes
- Rework globe generation into explicit geomorphology phases:
  - Keep plate tectonics, erosion, hydrology, and climate, but compute per-cell diagnostics after elevation normalization: `slope_norm_3x3`, `local_relief_5x5`, `topographic_position`, `coast_distance_cells`, `major_river_distance_cells`, `mountain_distance_cells`, and aquifer depth.
  - Classify relief in priority order: ocean, wet basin, floodplain, coastal plain, mountain ridge/slope, badlands, foothills, upland plateau, rolling hills, lowland plain.
  - Suggested thresholds: plains require `slope <= 0.012` and `local_relief <= 0.045`; rolling hills allow roughly `0.025 / 0.080`; foothills/mountains require high relief, mountain proximity, or `elev_f >= 0.82`.

- Replace biome-only z noise with relief profiles:
  - Floodplain/coastal/plain/wetland: broad rolls only, target chunk z range `0..=2`, almost no adjacent ±1 chatter.
  - Rolling/upland: moderate relief, target chunk z range `2..=5`.
  - Badlands/foothills: rougher, eroded channels, exposed stone.
  - Mountain slope/ridge: largest relief, ridged noise, cliffs allowed but not everywhere.
  - Biome remains a secondary modifier, not the main relief driver.

- Decouple surface material from elevation:
  - Land biome palettes should no longer create arbitrary `Water`; water comes from ocean, river, reservoir, or aquifer-marsh logic only.
  - Stone exposure should come from relief/slope/elevation, so plains stay grass/scrub/soil instead of becoming rocky because a noise value crossed a high band.
  - Update fertility estimation to mirror the new material and relief model, with floodplains/lowland plains fertile, steppe drier, badlands low, wetlands productive but settlement-hostile.

- Upgrade water-shaped terrain:
  - River stamping should flatten floodplain shoulders based on stream order/discharge, then carve the channel bed.
  - Lakes/wetlands should flatten basin surfaces and soften immediate shoreline relief.
  - Keep `surface_ground_z` vs `surface_z` water invariants intact.

- Integrate with selection and settlement:
  - Spawn/world-map tooltip shows relief class, slope, local z range, fertility, and water status.
  - Add `average_relief_in_megachunk` or equivalent sampling for overlays and spawn scoring.
  - Home selection and organic settlement scoring should prefer LowlandPlain/Floodplain/CoastalPlain/RollingHills, penalize BasinWetland/Badlands, and reject MountainSlope/MountainRidge for normal starts.

## Test Plan
- Unit tests for deterministic relief classification on synthetic elevation/hydrology samples.
- Globe tests across seeds `42`, `123`, and one high random seed:
  - Ocean remains roughly `25..=35%`.
  - Mountains remain roughly `5..=15%`.
  - Habitable land includes a meaningful share of `LowlandPlain` or `Floodplain`.
  - River termini still reach ocean, pole, lake, or wetland sinks.
- Terrain tests:
  - Plain/floodplain chunks have low z range and very few adjacent z deltas above `1`.
  - Mountain chunks have higher z range than plains.
  - Hydrology water-column invariants still hold.
  - `climate_fertility_estimate_at` stays close to generated chunk fertility averages.
- UI tests keep world-map image generation valid and add relief-overlay/tooltip smoke coverage.
- Verification commands: `cargo test --bin civgame` and `cargo check`.

## Assumptions
- Full overhaul is allowed to invalidate cached `world.bin`; bump `GLOBE_FILE_VERSION`.
- No new crates.
- Keep current world size, chunk size, z range, and Bevy/pathfinding architecture.
- Natural status excludes explored/settled/faction ownership state.
