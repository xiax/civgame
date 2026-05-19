# Organic Biome Edge Plan

## Summary
Make biome borders organic in both loaded gameplay terrain and the spawn/world map preview. The fix will keep canonical climate/hydrology logic stable, while adding a separate surface-biome/ecotone layer for visible terrain so borders become wavy, feathered, and patchy instead of square blocks.

## Key Changes
- Add a surface-biome API in `src/world/biome.rs`:
  - Keep `classify` and `classify_at_tile` unchanged for canonical climate, water salinity, nomad scoring, and world-sim logic.
  - Add `SurfaceBiomeSample { base, accent, accent_weight }` plus `surface_biome_sample_at_tile(globe, tx, ty)` and `classify_surface_at_tile(globe, tx, ty)`.
  - Use seeded low-frequency value noise from `globe.seed` to domain-warp land-biome rainfall/temperature sampling.
  - Preserve hard hydrology safety gates: clear ocean remains ocean, clear land never randomly becomes ocean, rivers/reservoirs still override terrain after biome selection.
  - Default constants: warp scale `128` tiles, warp amplitude `24` tiles, ecotone probe radius `18` tiles, max accent weight `0.35`.

- Integrate the surface-biome layer in `src/world/terrain.rs`:
  - Use `classify_surface_at_tile` for chunk `biome_cache`, `biome_bands`, topsoil kind/depth, and fertility estimates.
  - Update `surface_v` so `local_detail_amp` comes from the surface biome, avoiding abrupt relief changes at visual borders.
  - Keep river/riparian/reservoir passes authoritative and later in generation, so water bodies still stamp over the softened terrain.
  - Update `climate_fertility_estimate_at` to use the same surface-biome selection as chunk generation.

- Improve ecotone tile texture:
  - Near a detected biome edge, choose between base and accent biome bands with smooth seeded noise, not per-tile random speckle.
  - Let transitional biomes naturally appear as fringe materials, such as `Scrub` between grassland/desert, `Grass` between forest/wetland, and rocky foothill patches near mountains.
  - Do not add new crates.

- Update map previews in `src/ui/world_map.rs` and `src/ui/spawn_select.rs`:
  - Render biome colors from `classify_surface_at_tile`, so preview matches generated terrain.
  - Increase `WORLD_MAP_OVERSAMPLE` from `2` to `4`.
  - Load spawn/world map textures with linear filtering instead of nearest-neighbor block scaling.
  - Update spawn-select “dominant biome” sampling to use a small surface-biome tile grid over each mega-chunk instead of raw stored climate-cell biomes.

- Update docs:
  - Document the new surface-biome/ecotone layer in `AGENTS.md`.
  - Update `src/world/CLAUDE.md` for terrain generation details.
  - Update `src/ui/CLAUDE.md` for preview rendering changes.
  - Do not bump `GLOBE_FILE_VERSION`, because the serialized globe schema and stored cell data remain unchanged.

## Test Plan
- Add unit tests for deterministic surface-biome sampling across repeated calls and seeds.
- Add guard tests that clear ocean stays ocean and clear inland land does not become ocean through edge noise.
- Add a synthetic biome-boundary test proving the surface classifier creates varied, non-straight transition patterns near a land-biome threshold.
- Add terrain tests that generated chunks remain deterministic and fertility estimates stay in sync with surface-biome selection.
- Add world-map image tests for oversample dimensions and basic non-empty biome variation.
- Run `cargo test --bin civgame`.

## Assumptions
- The goal is visual/terrain organicness, not changing high-level biome semantics for AI, water salinity, or world simulation.
- Coastlines should be visually smoother in the preview, but terrain water placement remains governed by elevation, rivers, and reservoirs.
- Transition bands should be noticeable but conservative: roughly 12-36 tiles wide, with no large random biome islands far from an actual border.
