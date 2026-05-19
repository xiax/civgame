# World (`src/world/`)

Procedural terrain, globe generation, climate, geology, spatial index. See root `CLAUDE.md` for tile/chunk/Z conventions and `SpatialIndex` discipline.

## Water system

Real water columns + worldgen hydrology truth + persistent runtime water + dams + a background
fluid sim. Shipped in 7 phases — **phase log, rationale, and the actionable v2 deferral list live in
`plans/water-simulation.md`**; this section is the end state.

**Hydrology truth (`globe.rs`/`hydrology.rs`).** `Globe.hydrology: HydrologyMap { cells:
Vec<HydroCell>, reservoirs: Vec<Reservoir> }`. `HydroCell { raw_height, filled_height, flow_to,
discharge, reservoir_id, aquifer_level }`; `Reservoir { id, kind, spill_level, outlet_cell,
salinity }`; `ReservoirKind { Ocean(0)|Lake|Wetland|Endorheic|Spring|Dam }` (Spring/Dam runtime).
Pure fns: `weighted_discharge` (rainfall-weighted), `strahler_order`, `classify_reservoirs` (basin
clusters — closed → Endorheic/salinity 0.6, shallow → Wetland, else Lake; Ocean = reservoir 0,
salinity 1.0), `aquifer_table`, `build_hydrology`. Built in `generate_globe` step 7 (post-pass,
no pipeline reorder). Shared accessors `Globe::water_level_at/reservoir_at/salinity_at/
hydro_cell_at` (same data for chunk stamping **and** world-map overlay — no parallel formula).
`GLOBE_FILE_VERSION = 8`.

**Chunk water columns.** `Chunk` carries `surface_ground_z` (i8 bed), `surface_water_depth` (f32
Z-units, 0 = dry), `surface_reservoir_id` (u32, MAX = none). **`surface_z_at` keeps its original
meaning** (rendered top = water surface for wet tiles); `ground_z_at` is the *additive* solid-bed
accessor. Dry invariant `ground==surface, depth==0, rid==MAX` (re-asserted by `set_delta` on
built/dug/removed tiles). Accessors `ground_z_at`/`water_depth_at`/`water_level_at`/
`reservoir_id_at`/`water_column_at`/`apply_water_column`. `generate_chunk_from_globe` stamps rivers
`bed = water_surf - ceil(depth)` and reservoirs by basin membership (Lake/Endorheic→Water,
Wetland→Marsh, at `spill_level·GLOBE_H_TO_Z`; Ocean→biome). Terrain-elevation comparisons use
`ground_z_at`; **exception:** `land.rs::plot_value_factor` deliberately stays on `surface_z_at` (its
`z` feeds `tile_at_3d` for kind/fertility, so a wet tile must read the water surface).

**Persistent runtime water (`water_runtime.rs`).** Chunks regenerate fresh from `Globe+seed` on
stream-in (deltas not re-applied), so runtime water changes (dam pools, sim writes) would die on
unload. `RuntimeWater` Resource — tile-keyed `RuntimeWaterCell { ground_z, depth, reservoir_id,
salinity, source_rate }` + `dam_crests` + `runtime_reservoirs` — holds it **off-chunk** (a *cache*
of derived truth, rebuildable from `BridgeMap`/`DamMap`/dig-history; no disk persistence).
`RuntimeWater::set` removes `depth<=0` (drained → natural terrain). `restamp_runtime_water_on_chunk_load`
(WorldPlugin, FixedUpdate `.after(chunk_streaming_system)`, before PostUpdate
`refresh_changed_tiles_system`) re-applies `RuntimeWater` columns **and re-stamps `Bridge`/`Dam`
tiles from `BridgeMap`/`DamMap`** on each `ChunkLoadedEvent` (shared `stamp` closure) — this is what
keeps a bridged/dammed cell from reverting to River on reload (the structural-delta-not-reapplied
gap). Re-stamp uses `set_tile` (same path as finalize).

**Dam (`construction.rs` + `tile.rs`).** `TileKind::Dam=25` (26 variants) — passable + road-speed
(crest carries a road) but **not** water-like/fresh/drinkable (water blocked; the deliberate
`Bridge` contrast). The **`Dam` entity in `DamMap` is the durable truth**; the tile kind is its
cache projection (restamped from `DamMap`). `BuildSiteKind::Dam` mirrors the Bridge pipeline:
recipe 6 stone+4 wood / 180 work / `BRIDGE_BUILDING`-gated, `is_water_anchored()` covers
`Bridge|Dam`, finalize stamps the tile + entity + `RuntimeWater::register_dam(tile, crest_z)`,
deconstruct (shared `water_anchored_refund_tile`) restores the prior tile + `clear_dam` + bank
refund. `FurnitureMaps` bundles `dam_map`+`runtime_water`. Right-click Build Dam on River/Water;
v1 player-built only (no AI emitter — v2). `BuildRecipeIdx::Dam` is appended last (stable indices).

**Background fluid sim.** `water.rs` is a pure, Bevy-free, deterministic virtual-pipe shallow-water
core: `WaterGrid` of `Free`/`Pinned` cells + `dam_crests`. `step()` builds a **sorted** transfer
list (cardinal pipe edges ∝ surface-Δ + dam-weir over-crest spill), per-giver volume-clamps so no
Free cell goes negative, applies. **Conserves volume exactly** and is **bit-for-bit deterministic**
(both unit-tested, with basin-fill / dam pool→overtop→drain). `water_runtime.rs` wraps it:
`WaterSim` runs one `AsyncComputeTaskPool` task (20-tick cadence, mirrors the pathfinding
snapshot→spawn→poll pattern). `spawn` (PostUpdate) snapshots a bounded region (R=28 around dams +
runtime cells; self-terminating) — ocean/lake/unloaded → Pinned at hydrology Z, highest boundary
watercourse → discharge inlet `source`, dam footing → weir crest = footing + `DAM_RISE_Z=3`. `poll`
(PreUpdate) writes `RuntimeWater` (persistent) + live `ChunkMap`, restores natural kind on drain,
emits `TileChangedEvent` **only on wet/dry passability flip** (deadband). Main tick never blocks.

**Salinity + wells (`biome.rs`/`drink.rs`).** `WaterKind::{Fresh,Brackish,Salt}`; `water_kind_at`
(signature byte-identical) reads `Globe::salinity_at` via pure `classify_salinity` (Endorheic
brackish, ocean salt, rivers/open lakes fresh; River/Marsh always Fresh). `WaterKind::is_drinkable()`
(Fresh-only) is the single salt/brackish rejection rule (drink + animal water-seek). Aquifer wells:
`well_has_water`/`well_reaches` — a well yields only when `HydroCell.aquifer_level` is within the
`WELL_REACH_Z` shaft below `surface_z` (physical reachability); dry wells skipped by
`nearest_well_tile`, graceful `DrinkOutcome::WellDry`. Settlement/nomad/herd scoring rides
`TileKind::is_freshwater()` / `river_distance_at` (unchanged; `Dam`=false, `Bridge`=true) — they do
not read salinity, so no constant re-tune was needed (diagnosed, not assumed). Pathfinding is
unchanged (rides `TileChangedEvent`).

## World generation

- **Terrain noise (`terrain.rs::surface_v`):** Globe-anchored. Macro signal = bilinearly-sampled `Globe::sample_climate(tx,ty)` elevation; per-tile detail = 3-octave Perlin perturbation at biome-conditional amplitude `local_detail_amp(biome)` (range `0.07..0.15`): coastlands/wetlands/deserts/steppes hug macro at 0.07; mountains/badlands keep 0.15 so ridges read jagged; everything else 0.10. In-game tile z follows the world map's elevation field — Ocean low, Mountain peaks tall, Grassland near sea level — replacing the old purely-local-noise decoupling. The old `sign(c)*|c|^0.65` reshape is dropped; macro→detail split now provides shape.
- **Globe pipeline (`globe.rs::generate_globe`):** 512×256 climate-cell grid. (1) Lloyd-relaxed Voronoi over `NUM_PLATES=8` with **domain-warped assignment** (`plates::assign_nearest` offsets the lookup point by a two-octave Perlin warp at `WARP_FREQ=0.04`, `WARP_AMP=8` cells) → wavy/fingered plate boundaries. Uplift/subsidence stamped along boundaries; post-smooth uplift gets ±12% Perlin jitter (`JITTER_FREQ=0.05`). Bump `GLOBE_FILE_VERSION` when retuning warp/jitter constants. (2) Heightmap: macro-dominated multi-octave Perlin (52% on two low-freq octaves) + 30%×1.4 plate uplift. Noise frequencies scaled by `nscale = 256 / GLOBE_WIDTH` so doubling the grid keeps continents the same world-size. (3) `erosion.rs` thermal(20) → hydraulic(40). (3.5) **Sea-level alignment shift**: subtract the 30th-percentile so `h<=0` means ocean for both hydrology and biome classification — without this, rivers terminated mid-continent. (4) `hydrology.rs` pit_fill → flow_dirs (D8) → flow_accum → `extract_rivers(min_accum=80)` + `LakeMap` (lakes gated on `height > 0`; sub-sea basins become ocean). (5) **Three-segment elevation remap** anchored at `h_min → 0.0`, `h=0 → 0.22` (ocean line), `h_peak (90th-pct) → 0.82` (mountain line), `h_max → 1.0`. Guarantees ~30% ocean / ~10% mountain regardless of distribution (verified by `ocean_fraction_within_band` test). `climate.rs` temp from latitude+elev; rainfall = base Perlin + orographic. (6) Per-cell biome via `biome::classify`.
- **Grid scales:** `GLOBE_WIDTH=512, GLOBE_HEIGHT=256, GLOBE_CELL_CHUNKS=2` → climate cell = 2×2 chunks = 64×64 tiles. `MEGACHUNK_SIZE_CHUNKS=16` → mega-chunk = 16×16 chunks = 8×8 climate cells. Total world: 1024×512 chunks ≈ 32K×16K tiles.
- **Continuous climate:** `Globe::sample_climate(tile_x, tile_y)` bilinearly interpolates the four nearest cells (X-wrap, Y-clamp). `biome::classify_at_tile` runs *per-tile* — no biome stripes at cell boundaries.
- **Bridges**: `TileKind::Bridge` is a constructed tile that overlays a former `TileKind::River` cell. Pathfinding treats it as `Road`-speed (`tile_speed_multiplier = 1.4`); semantic helpers `is_water_like` / `is_freshwater` / `is_drinkable_candidate` all return `true` because the water still flows under the decking — nomad water-search, herd water-seek and the thirst pipeline keep treating the cell as a freshwater source. `is_passable` / `is_floor` return `true`. `is_stone_like` / `is_soil_like` return `false`. Bridges are spawned only via `BuildSiteKind::Bridge` (see root `CLAUDE.md` → Settlement construction → Rivers and bridges); world-gen never produces them. `chunk_map.river_distance_at` is not invalidated when a river is bridged — the cached cache reflects the underlying geography.
- **Fertility model (`terrain::surface_fertility_of`):** Per-tile fertility is `kind_fertility_factor(kind) × elevation_fertility_curve(v)` clamped to `u8`. Productive surface kinds — `Grass` (1.0×), `Marsh` (0.9×), `Forest` (0.7×), `Scrub` (0.3×) — all get non-zero fertility; everything else (Sand/Snow/Stone/Water/Sandstone/Granite/...) is 0. The elevation curve is a tent `(1 - |v - 0.45| * 2.0).max(0) * 255` (peaks at v=0.45, support roughly `[-0.05, 0.95]`), so vegetated tiles keep a sensible baseline even at biome-band edges. Pass 3 of chunk-gen multiplies this by `river_fertility_mult` (1.6× / 1.3× in the 2..=3 / 4..=5 riparian band), so Forest/Marsh/Scrub also receive the riverside boost. Consumers: wild plant spawn (`chunk_streaming.rs`), plot scoring (`land.rs`), settlement scoring (`organic_settlement.rs`), hover UI. **`TileKind::Cropland`** is a *runtime-stamped* (never world-gen) tilled-farm tile written over Grass/soil by `land::carve_plots_system` / seed-farm paths for Agricultural plots; it is `is_soil_like` (`soil_fertility_mult` 1.3, plant/fertility plumbing accepts it), pathing speed 0.9, and protected from road carving (`simulation::land::tile_is_farm_protected`).
- **Climate-only fertility estimate (`terrain::climate_fertility_estimate_at`):** Pure-climate counterpart to the chunk-gen fertility formula. Uses `Globe::sample_climate` + `biome::classify_at_tile` + `biome_bands(...).pick(v)` to pick a surface kind, then calls `surface_fertility_of(kind, v)` and applies `river_fertility_mult` via `Globe::nearest_river_chebyshev(tx, ty)` (O(total river polyline points)). Returns the *expected* fertility chunk-gen would produce at that tile — chunks add zero-mean Perlin variation around the climate-only `v`, so the climate estimate is the average. Used by `region::average_fertility_in_megachunk` (8×8 sample grid) to drive the world-map fertility overlay and the spawn-select / world-map hover tooltips without requiring loaded chunks.
- **Rivers**: `extract_rivers` produces `RiverEdge` between climate-cell centers; `chaikin_river_path` turns each into a deterministic curving tile polyline (3 perpendicular-jittered control points + two Chaikin corner-cut passes). Polylines stored on `RiverNetwork.edge_polylines` (parallel to `edges`). `terrain.rs::diamond_stamp` rasterises with a Manhattan-clamped diamond + width tapering, writes `TileKind::River`, sets the water column (`bed = water_surf - ceil(depth)`, see Water system), and populates `Chunk.surface_river_distance` for the feather ring (radius `RIVER_FEATHER_DIST=16` outside the channel). The riparian band shifts `BiomeBands.thresholds` toward greener slots (ranked by `greenness_rank`: Forest>Marsh>Grass>Scrub) and multiplies fertility ×1.6 / ×1.3 at distance 2-3 / 4-5 — biome/fertility/topsoil effects still hard-gate on `river_d ≤ 5` regardless of feather radius. Topsoil within `river_d ≤ 5` overrides to `Silt`. Settlement spawn (both `score_home_candidate` and `score_tile`) uses `chunk_map.river_distance_at` in a best-of-N picker and rewards `river_d ∈ 13..=16` so the initial `base_r ≈ 12` footprint fits on one bank before Chalcolithic-era bridges. `score_water` (nomad) and `nearest_water` (wild herd) prefer fresh (`is_freshwater()`) over salt. **`biome::water_kind_at` → `WaterKind::{Fresh,Brackish,Salt}`** from reservoir salinity (see Water system) — drives the thirst pipeline's drinkable-tile filter without persisting a flag on `TileData`. World-map render walks `edge_polylines` for per-pixel polyline overlay; cell-level `is_river` tint dropped. **Lakes** flood-fill via reservoir basin membership (`TileKind::Water`).
- **Cached globe (`world.bin`):** `GlobeFile { version, globe }`; bump `GLOBE_FILE_VERSION` (currently `8`) on layout changes — auto-regenerates on mismatch *or* on `globe.seed != WorldSeed.0`. Determinism via per-component seeded RNGs.
- **World re-rolling:** `WorldSeed` resource (default 42) drives both `globe::generate_globe` and `terrain::WorldGen::with_seed`. Spawn-select UI's Apply/Reroll buttons fire `RegenerateWorldRequest`; `regenerate_world_system` (Update, SpawnSelect-only) reinserts both resources. `world.bin` is only persisted on `OnExit(SpawnSelect)` so rapid re-rolls don't thrash disk.
- **Resource patch clustering (`chunk_streaming.rs`):** loose rocks, wild trees in fertile grass, and wild berry bushes use a two-tier deterministic hash — coarse `patch_hash` (`PATCH_CELL_SIZE=6` tiles, separate seeds per kind: `ROCK_PATCH_SEED`, `TREE_PATCH_SEED`, `BERRY_PATCH_SEED`) gates whether a cell is a patch; per-tile hash gates density inside. Result: discrete groves / berry patches / rock fields. Forest biome and Farmland keep uniform rates.
- **Loose rocks (`chunk_streaming.rs::spawn_chunk_loose_rocks`):** Stone GroundItems (qty 1–3); ~30% of stone-surface patch cells are rocky, ~70% of tiles inside a rocky patch get a rock (~21% overall). Per-tile hash seed `ROCK_HASH_SEED=0xDEAD_C0DE`.

## Geology & mining

- **Surface vs subsurface:** `biome_bands(biome)` returns per-biome 4-threshold + 5-`TileKind` palette; `BiomeBands::pick(v)` chooses the surface tile. Per-biome flavours: Tundra → `Snow`/`Scrub`/`Granite`; Desert → `Sand`/`Scrub`/`Sandstone`; Wetland → `Marsh`/`Grass`/`Forest`; Tropical → `Marsh`/`Grass`/`Forest`/`Basalt`; Steppe → `Scrub`/`Grass`/`Sandstone`; Badlands → `Sand`/`Scrub`/`Sandstone`/`Granite`; Temperate / Grassland → `Grass`/`Forest`/`Limestone`; Mountain core → `Granite`/`Basalt`. Below: `proc_tile` runs cave-noise → `topsoil_depth(biome)` of soil → ore vein lookup → `Wall` bedrock. Topsoil variant via `topsoil_kind(biome, river_d)`: river band (`river_d ≤ 5`) → `Silt`; Wetland/Tropical → `Clay`; Temperate/Grassland/Steppe → `Loam`; Desert/Badlands → `SandySoil`; Tundra/Taiga/Mountain/Ocean → `Dirt`. `topsoil_depth`: Mountain 1; Desert/Tundra/Badlands 2; Grassland/Steppe 3; Taiga/Tropical/Temperate/Wetland 4; Ocean 0.
- **Biome classification (`biome::classify`):** Whittaker matrix extended with `Wetland` (elev_f<0.30, rain>0.75, temp>0.30), `Steppe` (rain 0.30..0.50, temp 0.40..0.70), `Badlands` (rain<0.25, elev_f 0.45..0.80). `Biome` is 11 variants.
- **Ore veins (`ORE_BANDS`):** 6 3D Perlin fields, seeded `WORLD_SEED+2..=7`. Shallow→deep: Coal (1..6, 0.45), Copper (2..8, 0.50), Tin (5..12, 0.55), Iron (6..14, 0.52), Silver (10..18, 0.60), Gold (14..32, 0.65). Encoded as `TileKind::Ore` + `TileData.ore: u8` (`OreKind`). Surface stone never drops random ore — players must dig down.
- **Mining yields (`carve.rs::carve_tile`):** generic stone-like tiles route via `is_stone_like()`; per-variant yield from `TileKind::stone_yield_count()` (Limestone 3, Granite/Sandstone/Basalt/Stone/Wall 2). `Ore → (ore_yield_good(ore), 2)`. `gather.rs`/`dig.rs` route via `route_yield`/`Carrier::try_pick_up`. `ActivityKind` count: 13 — keep `ACTIVITY_KINDS`/`ACTIVITY_NAMES` (debug_panel.rs) and `activity_name` (tech_panel.rs) in sync. When the floor is already passable (procedural topsoil), `carve_tile` still writes a delta with the existing kind so `Chunk::set_delta` refreshes `surface_kind` off `Air`.
- **Two tile-read paths:** `chunk.rs::tile_at_local` (cache-only) returns `Wall` for any uncarved subsurface tile — passability/LOS-correct but **material-wrong**. For ore-accurate reads (mining yields, inspector display) call `terrain::tile_at_3d`.

## Streaming schedule

- **`chunk_streaming_system` runs on `FixedUpdate` (20 Hz)**, not `Update`. The per-frame variant burned cycles on the full-map unload filter even when the camera was stationary. One fixed-tick (≤50 ms) load lag is imperceptible. `update_chunk_retention_system`, `update_simulation_focus_system`, and `fog::fog_update_system` move with it; gizmos and `update_tile_z_view_system` stay on `Update`.
- **`update_tile_z_view_system`** carries a `Local<Option<i32>>` last-applied-z guard — Bevy's `is_changed()` fires false-positives (first-tick re-fire, same-value `set_changed()` writes), and the system iterates the full loaded-sprite set on every fire.
