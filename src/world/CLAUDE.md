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
`GLOBE_FILE_VERSION = 9`.

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
of derived truth, rebuildable from `BridgeMap`/`DamMap` + hydrology). **No cross-process
persistence by engine design** — only `world.bin` (the Globe) serialises; everything else
regenerates live from `Globe+seed`. Instead `update_chunk_retention_system` pins the chunks under
every dam + every `RuntimeWater` cell with `depth>0 || source_rate>0`, so a player-/AI-affected
water region stays resident as the camera roams (pan-away/back stays desync-free without leaning
solely on the reload restamp).
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
recipe 6 stone+4 wood / 180 work / dedicated **`DAM_BUILDING`**-gated (Bronze Age; prereqs
`BRIDGE_BUILDING`+`MONUMENTAL_BUILDING`), `is_water_anchored()` covers `Bridge|Dam`, finalize
stamps the tile + entity + `RuntimeWater::register_dam(tile, crest_z)`, deconstruct (shared
`water_anchored_refund_tile`) restores the prior tile + `clear_dam` + bank refund. `FurnitureMaps`
bundles `dam_map`+`runtime_water`. Right-click Build Dam on River/Water **and** AI-planned by
`organic_settlement::dam_intent_emitter_system` (mirrors `bridge_intent_emitter_system`,
author-less; gated `DAM_BUILDING` + `CivicKind::Dam` Bronze+30; pure `score_dam_site` composes
irrigation / reservoir-water-access / road-crossing motives, one dam/settlement/cadence,
`DAM_MIN_SPACING` apart). `BuildRecipeIdx::Dam` is appended last (stable indices).

**Background fluid sim.** `water.rs` is a pure, Bevy-free, deterministic virtual-pipe shallow-water
core: `WaterGrid` of `Free`/`Pinned` cells + `dam_crests`. `step()` builds a **sorted** transfer
list (cardinal pipe edges ∝ surface-Δ + dam-weir over-crest spill), per-giver volume-clamps so no
Free cell goes negative, applies. **Conserves volume exactly** and is **bit-for-bit deterministic**
(both unit-tested, with basin-fill / dam pool→overtop→drain). `water_runtime.rs` wraps it:
`WaterSim` runs one `AsyncComputeTaskPool` task (20-tick cadence, mirrors the pathfinding
snapshot→spawn→poll pattern). `spawn` (PostUpdate) snapshots a bounded region (R=28 around dams +
runtime cells; self-terminating) — ocean/lake/unloaded → Pinned at hydrology Z; **per-cell flow
routing** via `RiverNetwork::edge_crossings_in_bbox` places real inlets (inject the edge's
`discharge`) / outlets (Pinned) at the true channel crossings (replaced the old highest-elevation
boundary guess); a Free cell whose bed sits below `HydroCell.aquifer_level` seeps upward (springs +
pits dug below the table) **capped at the table** — `bed+depth<aquifer_z` only, so groundwater
never floods rock (snapshot-time gate ⇒ zero `water.rs` change, determinism tests stand); dam
footing → weir crest = footing + `DAM_RISE_Z=3`. All inflow follows the **snowmelt seasonal
hydrograph** `Calendar::discharge_multiplier` (Spring 1.5 / Summer 0.7 / Autumn 1.0 / Winter 0.25),
*damped* for aquifer seep (groundwater lags surface). Truth gate is the **per-tile natural table**
— `terrain::surface_height(tx,ty,...)` (chunk-gen per-tile-jittered natural surface) minus
`(filled_height - aquifer_level) · GLOBE_H_TO_Z` (per-climate-cell-stable aquifer
depth-below-local-surface, ~0.16 Z wet to ~1.6 Z dry). The classify loop applies it to **every**
interior Free cell: bed below the per-tile table ⇒ source; pool ≥ table ⇒ cap zeroes the source
(no rock flooding). This treats natural per-tile depressions and dug pits *identically* — a dug pit
filling while a deeper natural hollow next to it stayed dry would be absurd. Wet climates
auto-shallow the gate, dry climates auto-deep it; no magic margin. `aquifer_seep_emitter_system`
(PostUpdate, before `spawn`) is pure region-bootstrap: on a `TileCarvedEvent` (from `dig_system`,
NOT the broad `TileChangedEvent`) that clears the per-tile table, insert a depth-0 runtime cell so
the sim's active region covers the dig — without it an isolated well far from any dam wouldn't
make `runtime_water.cells` non-empty and the sim wouldn't run. **Truth gate (chunk-gen + runtime + wells,
single shared formula):** anchor on the per-CELL macro elevation in the same Z frame as
`surface_z` — `cell_surface_z = Z_MIN + (sample_climate(tx,ty).0/255) · CHUNK_HEIGHT` — and
subtract the per-cell aquifer-depth in Z — `aquifer_depth_z = (filled_height − aquifer_level) ·
GLOBE_H_TO_Z`. A tile's bed is below the table iff `bed_z < cell_surface_z − aquifer_depth_z`.
`aquifer_table` (`hydrology.rs`) is calibrated against real groundwater tables at our 1.5 m tile
scale: ~0.5 Z (~0.75 m) in saturated lowland to ~12 Z (~18 m) in true arid — `depth_raw = 0.0625
+ 1.4375 · (1 − rainfall_norm)`. The wet end lets per-tile jitter (~±2–4 Z amplitude in moist
biomes) genuinely dip below the table in wetland transitions; the arid end is well past max
jitter so deserts produce no spurious marshes.
Chunk-gen Pass 4.5 (in `generate_chunk_from_globe`, after the reservoir basin stamp) applies this
to every remaining dry tile: bed below ⇒ flip to `Marsh`, bed = original surface_z, water surface
= clamped `table_z`, depth ≥ `MIN_RIVER_DEPTH_Z`, `reservoir_id = u32::MAX` (no globe reservoir
owns tile-local seeps). The runtime sim (`spawn` snapshot + `aquifer_seep_emitter_system`) uses
the same per-cell table — so a natural depression and a dug pit are treated identically (both
seep when their bed falls below the gate), and a Pass-4.5 marsh pulled into an active region
keeps its aquifer source instead of draining. Anchoring on per-CELL macro (not per-tile
`surface_height`) is what makes per-tile jitter count: a per-tile bed anchor would be
structurally tautological for natural tiles (bed == anchor ⇒ never below). `poll` (PreUpdate) writes `RuntimeWater` (incl. `source_rate`) + live
`ChunkMap`, keeps a still-fed source cell alive even when drained (so an isolated dug well refills),
restores natural kind on true drain, emits `TileChangedEvent` **only on wet/dry passability flip**
(deadband). Main tick never blocks.

**Salinity + wells (`biome.rs`/`drink.rs`).** `WaterKind::{Fresh,Brackish,Salt}`; `water_kind_at`
(signature byte-identical) reads `Globe::salinity_at` via pure `classify_salinity` (Endorheic
brackish, ocean salt, rivers/open lakes fresh; River/Marsh always Fresh). `WaterKind::is_drinkable()`
(Fresh-only) is the single salt/brackish rejection rule (drink + animal water-seek). Aquifer wells:
`well_has_water`/`well_reaches` — a well yields only when the per-cell aquifer surface (computed
in the same shared `cell_surface_z − aquifer_depth_z` frame as Pass 4.5 + the fluid sim seep
gate) is within the `WELL_REACH_Z=4` shaft below `surface_z`. With the recalibrated depth
(`~0.5 Z` wet to `~12 Z` arid) the 4 Z shaft only reaches the table in moist climates; arid wells
go genuinely dry → `DrinkOutcome::WellDry`. Dry wells skipped by
`nearest_well_tile`, graceful `DrinkOutcome::WellDry`. Settlement/nomad/herd scoring rides
`TileKind::is_freshwater()` / `river_distance_at` (unchanged; `Dam`=false, `Bridge`=true) — they do
not read salinity, so no constant re-tune was needed (diagnosed, not assumed). Pathfinding is
unchanged (rides `TileChangedEvent`).

## World generation

- **Terrain noise (`terrain.rs::surface_v`):** Globe-anchored. Macro signal = bilinearly-sampled `Globe::sample_climate(tx,ty)` elevation; per-tile detail = 3-octave Perlin perturbation at biome-conditional amplitude `local_detail_amp(biome)` (range `0.035..0.075` v-units, i.e. ±1.1..±2.4 Z ≈ ±1.7..±3.6 m at our 1.5 m tile scale): coastlands/wetlands/deserts/steppes hug macro at 0.035; mountains/badlands at 0.075 so ridges still read rugged; vegetated belts at 0.05. Halved from the legacy 0.07/0.10/0.15 values, which produced unrealistic ±3–7 m per-tile bumps. In-game tile z follows the world map's elevation field — Ocean low, Mountain peaks tall, Grassland near sea level — replacing the old purely-local-noise decoupling.
- **Globe pipeline (`globe.rs::generate_globe`):** 512×256 climate-cell grid. (1) Lloyd-relaxed Voronoi over `NUM_PLATES=8` with **domain-warped assignment** (`plates::assign_nearest` offsets the lookup point by a two-octave Perlin warp at `WARP_FREQ=0.04`, `WARP_AMP=8` cells) → wavy/fingered plate boundaries. Uplift/subsidence stamped along boundaries; post-smooth uplift gets ±12% Perlin jitter (`JITTER_FREQ=0.05`). Bump `GLOBE_FILE_VERSION` when retuning warp/jitter constants. (2) Heightmap: macro-dominated multi-octave Perlin (52% on two low-freq octaves) + 30%×1.4 plate uplift. Noise frequencies scaled by `nscale = 256 / GLOBE_WIDTH` so doubling the grid keeps continents the same world-size. (3) `erosion.rs` thermal(20) → hydraulic(40). (3.5) **Sea-level alignment shift**: subtract the 30th-percentile so `h<=0` means ocean for both hydrology and biome classification — without this, rivers terminated mid-continent. (4) `hydrology.rs` pit_fill → flow_dirs (D8) → flow_accum → `extract_rivers(min_accum=80)` + `LakeMap` (lakes gated on `height > 0`; sub-sea basins become ocean). (5) **Three-segment elevation remap** anchored at `h_min → 0.0`, `h=0 → 0.22` (ocean line), `h_peak (90th-pct) → 0.82` (mountain line), `h_max → 1.0`. Guarantees ~30% ocean / ~10% mountain regardless of distribution (verified by `ocean_fraction_within_band` test). `climate.rs` temp from latitude+elev; rainfall = base Perlin + orographic. (6) Per-cell biome via `biome::classify`.
- **Grid scales:** `GLOBE_WIDTH=512, GLOBE_HEIGHT=256, GLOBE_CELL_CHUNKS=2` → climate cell = 2×2 chunks = 64×64 tiles. `MEGACHUNK_SIZE_CHUNKS=16` → mega-chunk = 16×16 chunks = 8×8 climate cells. Total world: 1024×512 chunks ≈ 32K×16K tiles.
- **Continuous climate:** `Globe::sample_climate(tile_x, tile_y)` bilinearly interpolates the four nearest cells (X-wrap, Y-clamp). `biome::classify_at_tile` runs *per-tile* — no biome stripes at cell boundaries.
- **Surface-biome layer (`biome.rs::classify_surface_at_tile` / `surface_biome_sample_at_tile`):** Separate *visual/terrain* biome decision so borders feather organically without touching canonical climate/hydrology. Land branch (`classify_land`, split out of `classify`) runs on temp/rain sampled at a **domain-warped** offset (128-tile wavelength × ±24-tile amplitude, hash value-noise from `globe.seed` — no `Perlin::set_seed` per call, deterministic and `&Globe`-only so preview matches terrain). Ocean (`elev_f < 0.22`) and Mountain (`elev_f > 0.82`) gates stay on the tile's **true** elevation — structural guarantee: no inland oceans, no random inland peaks, coasts/water columns/salinity untouched. `SurfaceBiomeSample { base, accent, accent_weight }` adds O(1) ecotone (the secondary warp's biome is `accent`; weight is high-freq-dithered `MAX_ACCENT_WEIGHT=0.35` when base ≠ accent, else 0). Chunk-gen Pass 1 picks the surface kind from `accent`'s palette when `surface_band_dither < accent_weight`, otherwise from `base` — transitional materials (Scrub between Grassland/Desert etc.) emerge from existing `biome_bands` palettes, no new TileKinds. `biome_cache` / `local_detail_amp` / `topsoil_kind` / Pass-3 riparian all read `base` (relief/soil tracks the dominant biome, only material reads dither). **Callers**: `terrain::generate_chunk_from_globe` Pass 1, `terrain::surface_v`, `terrain::tile_at_3d`, `terrain::climate_fertility_estimate_at`, world-map / spawn-select previews. **Canonical keepers** (still on `classify_at_tile`/`classify`): stored `cell.biome` at worldgen (`globe.rs`), nomad camp scoring (`nomad.rs`), `water_kind_at` defensive ocean check. Does **not** bump `GLOBE_FILE_VERSION` — purely post-classification, no serialized schema change.
- **Bridges**: `TileKind::Bridge` is a constructed tile that overlays a former `TileKind::River` cell. Pathfinding treats it as `Road`-speed (`tile_speed_multiplier = 1.4`); semantic helpers `is_water_like` / `is_freshwater` / `is_drinkable_candidate` all return `true` because the water still flows under the decking — nomad water-search, herd water-seek and the thirst pipeline keep treating the cell as a freshwater source. `is_passable` / `is_floor` return `true`. `is_stone_like` / `is_soil_like` return `false`. Bridges are spawned only via `BuildSiteKind::Bridge` (see root `CLAUDE.md` → Settlement construction → Rivers and bridges); world-gen never produces them. `chunk_map.river_distance_at` is not invalidated when a river is bridged — the cached cache reflects the underlying geography.
- **Fertility model (`terrain::surface_fertility_of`):** Per-tile fertility is `kind_fertility_factor(kind) × elevation_fertility_curve(v)` clamped to `u8`. Productive surface kinds — `Grass` (1.0×), `Marsh` (0.9×), `Forest` (0.7×), `Scrub` (0.3×) — all get non-zero fertility; everything else (Sand/Snow/Stone/Water/Sandstone/Granite/...) is 0. The elevation curve is a tent `(1 - |v - 0.45| * 2.0).max(0) * 255` (peaks at v=0.45, support roughly `[-0.05, 0.95]`), so vegetated tiles keep a sensible baseline even at biome-band edges. Pass 3 of chunk-gen multiplies this by `river_fertility_mult` (1.6× / 1.3× in the 2..=3 / 4..=5 riparian band), so Forest/Marsh/Scrub also receive the riverside boost. Consumers: wild plant spawn (`chunk_streaming.rs`), plot scoring (`land.rs`), settlement scoring (`organic_settlement.rs`), hover UI. **`TileKind::Cropland`** is a *runtime-stamped* (never world-gen) tilled-farm tile written over Grass/soil by `land::carve_plots_system` / seed-farm paths for Agricultural plots; it is `is_soil_like` (`soil_fertility_mult` 1.3, plant/fertility plumbing accepts it), pathing speed 0.9, and protected from road carving (`simulation::land::tile_is_farm_protected`).
- **Climate-only fertility estimate (`terrain::climate_fertility_estimate_at`):** Pure-climate counterpart to the chunk-gen fertility formula. Uses `Globe::sample_climate` + `biome::classify_surface_at_tile` + `biome_bands(...).pick(v)` to pick a surface kind, then calls `surface_fertility_of(kind, v)` and applies `river_fertility_mult` via `Globe::nearest_river_chebyshev(tx, ty)` (O(total river polyline points)). Uses the surface-biome layer's `base` (not the ecotone-dithered kind) so the *expected* fertility stays an average over the zero-mean accent dither. Used by `region::average_fertility_in_megachunk` (8×8 sample grid) to drive the world-map fertility overlay and the spawn-select / world-map hover tooltips without requiring loaded chunks.
- **Rivers**: `extract_rivers` produces `RiverEdge` between climate-cell centers; `chaikin_river_path` turns each into a deterministic curving tile polyline (3 perpendicular-jittered control points + two Chaikin corner-cut passes). Polylines stored on `RiverNetwork.edge_polylines` (parallel to `edges`). `terrain.rs::diamond_stamp` rasterises with a Manhattan-clamped diamond + width tapering, writes `TileKind::River`, sets the water column (`bed = water_surf - ceil(depth)`, see Water system), and populates `Chunk.surface_river_distance` for the feather ring (radius `RIVER_FEATHER_DIST=16` outside the channel). The riparian band shifts `BiomeBands.thresholds` toward greener slots (ranked by `greenness_rank`: Forest>Marsh>Grass>Scrub) and multiplies fertility ×1.6 / ×1.3 at distance 2-3 / 4-5 — biome/fertility/topsoil effects still hard-gate on `river_d ≤ 5` regardless of feather radius. Topsoil within `river_d ≤ 5` overrides to `Silt`. Settlement spawn (both `score_home_candidate` and `score_tile`) uses `chunk_map.river_distance_at` in a best-of-N picker and rewards `river_d ∈ 13..=16` so the initial `base_r ≈ 12` footprint fits on one bank before Chalcolithic-era bridges. `score_water` (nomad) and `nearest_water` (wild herd) prefer fresh (`is_freshwater()`) over salt. **`biome::water_kind_at` → `WaterKind::{Fresh,Brackish,Salt}`** from reservoir salinity (see Water system) — drives the thirst pipeline's drinkable-tile filter without persisting a flag on `TileData`. World-map render walks `edge_polylines` for per-pixel polyline overlay; cell-level `is_river` tint dropped. **Lakes** flood-fill via reservoir basin membership (`TileKind::Water`).
- **Cached globe (`world.bin`):** `GlobeFile { version, globe }`; bump `GLOBE_FILE_VERSION` (currently `9`) on layout changes — auto-regenerates on mismatch *or* on `globe.seed != WorldSeed.0`. Determinism via per-component seeded RNGs.
- **World re-rolling:** `WorldSeed` resource (default 42) drives both `globe::generate_globe` and `terrain::WorldGen::with_seed`. Spawn-select UI's Apply/Reroll buttons fire `RegenerateWorldRequest`; `regenerate_world_system` (Update, SpawnSelect-only) reinserts both resources. `world.bin` is only persisted on `OnExit(SpawnSelect)` so rapid re-rolls don't thrash disk.
- **Resource patch clustering (`chunk_streaming.rs`):** loose rocks, wild trees in fertile grass, and wild berry bushes use a two-tier deterministic hash — coarse `patch_hash` (`PATCH_CELL_SIZE=6` tiles, separate seeds per kind: `ROCK_PATCH_SEED`, `TREE_PATCH_SEED`, `BERRY_PATCH_SEED`) gates whether a cell is a patch; per-tile hash gates density inside. Result: discrete groves / berry patches / rock fields. Forest biome and Farmland keep uniform rates.
- **Loose rocks (`chunk_streaming.rs::spawn_chunk_loose_rocks`):** Stone GroundItems (qty 1–3); ~30% of stone-surface patch cells are rocky, ~70% of tiles inside a rocky patch get a rock (~21% overall). Per-tile hash seed `ROCK_HASH_SEED=0xDEAD_C0DE`.

## Geology & mining

- **Surface vs subsurface:** `biome_bands(biome)` returns per-biome 4-threshold + 5-`TileKind` palette; `BiomeBands::pick(v)` chooses the surface tile. Per-biome flavours: Tundra → `Snow`/`Scrub`/`Granite`; Desert → `Sand`/`Scrub`/`Sandstone`; Wetland → `Marsh`/`Grass`/`Forest`; Tropical → `Marsh`/`Grass`/`Forest`/`Basalt`; Steppe → `Scrub`/`Grass`/`Sandstone`; Badlands → `Sand`/`Scrub`/`Sandstone`/`Granite`; Temperate / Grassland → `Grass`/`Forest`/`Limestone`; Mountain core → `Granite`/`Basalt`. Below: `proc_tile` runs cave-noise → `topsoil_depth(biome)` of soil → ore vein lookup → `Wall` bedrock. Topsoil variant via `topsoil_kind(biome, river_d)`: river band (`river_d ≤ 5`) → `Silt`; Wetland/Tropical → `Clay`; Temperate/Grassland/Steppe → `Loam`; Desert/Badlands → `SandySoil`; Tundra/Taiga/Mountain/Ocean → `Dirt`. `topsoil_depth`: Mountain 1; Desert/Tundra/Badlands 2; Grassland/Steppe 3; Taiga/Tropical/Temperate/Wetland 4; Ocean 0.
- **Biome classification (`biome::classify`):** Whittaker matrix extended with `Wetland` (elev_f<0.30, rain>0.75, temp>0.30), `Steppe` (rain 0.30..0.50, temp 0.40..0.70), `Badlands` (rain<0.25, elev_f 0.45..0.80). `Biome` is 11 variants. The land branch is factored into `classify_land(elev,temp,rain)` so the surface-biome layer can call it with warped temp/rain while keeping the Ocean (`<OCEAN_ELEV_GATE=0.22`) / Mountain (`>MOUNTAIN_ELEV_GATE=0.82`) elevation gates on true elevation.
- **Ore veins (`ORE_BANDS`):** 6 3D Perlin fields, seeded `WORLD_SEED+2..=7`. Shallow→deep: Coal (1..6, 0.45), Copper (2..8, 0.50), Tin (5..12, 0.55), Iron (6..14, 0.52), Silver (10..18, 0.60), Gold (14..32, 0.65). Encoded as `TileKind::Ore` + `TileData.ore: u8` (`OreKind`). Surface stone never drops random ore — players must dig down.
- **Mining yields (`carve.rs::carve_tile`):** generic stone-like tiles route via `is_stone_like()`; per-variant yield from `TileKind::stone_yield_count()` (Limestone 3, Granite/Sandstone/Basalt/Stone/Wall 2). `Ore → (ore_yield_good(ore), 2)`. `gather.rs`/`dig.rs` route via `route_yield`/`Carrier::try_pick_up`. `ActivityKind` count: 13 — keep `ACTIVITY_KINDS`/`ACTIVITY_NAMES` (debug_panel.rs) and `activity_name` (tech_panel.rs) in sync. When the floor is already passable (procedural topsoil), `carve_tile` still writes a delta with the existing kind so `Chunk::set_delta` refreshes `surface_kind` off `Air`.
- **Two tile-read paths:** `chunk.rs::tile_at_local` (cache-only) returns `Wall` for any uncarved subsurface tile — passability/LOS-correct but **material-wrong**. For ore-accurate reads (mining yields, inspector display) call `terrain::tile_at_3d`.

## Streaming schedule

- **`chunk_streaming_system` runs on `FixedUpdate` (20 Hz)**, not `Update`. The per-frame variant burned cycles on the full-map unload filter even when the camera was stationary. One fixed-tick (≤50 ms) load lag is imperceptible. `update_chunk_retention_system`, `update_simulation_focus_system`, and `fog::fog_update_system` move with it; gizmos and `update_tile_z_view_system` stay on `Update`.
- **`update_tile_z_view_system`** carries a `Local<Option<i32>>` last-applied-z guard — Bevy's `is_changed()` fires false-positives (first-tick re-fire, same-value `set_changed()` writes), and the system iterates the full loaded-sprite set on every fire.
