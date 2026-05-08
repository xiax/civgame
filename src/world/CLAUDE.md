# World (`src/world/`)

Procedural terrain, globe generation, climate, geology, spatial index. See root `CLAUDE.md` for tile/chunk/Z conventions and `SpatialIndex` discipline.

## World generation

- **Terrain noise (`terrain.rs::surface_v`):** 4-octave FBM (continental 0.005 / base 0.02 / 2× / 4× harmonics, weights 0.35/0.40/0.18/0.07), reshaped via `sign(c)*|c|^0.65`. Lowering base freq or power = bigger features.
- **Globe pipeline (`globe.rs::generate_globe`):** 256×128 climate-cell grid. (1) Lloyd-relaxed Voronoi over `NUM_PLATES=8` → uplift/subsidence. (2) Heightmap = 70% multi-octave Perlin + 30%×1.4 plate uplift. (3) `erosion.rs` thermal(20) → hydraulic(40). (4) `hydrology.rs` pit_fill → flow_dirs (D8) → flow_accum → `extract_rivers(min_accum=80)` + `LakeMap`. (5) `climate.rs` temp from latitude+elev; rainfall = base Perlin + orographic. (6) Per-cell biome via `biome::classify`. ~50ms.
- **Grid scales:** `GLOBE_WIDTH=256, GLOBE_HEIGHT=128, GLOBE_CELL_CHUNKS=4` → climate cell = 4×4 chunks = 128×128 tiles. `MEGACHUNK_SIZE_CHUNKS=16` → mega-chunk = 16×16 chunks = 4×4 climate cells. Total world: 1024×512 chunks ≈ 32K×16K tiles.
- **Continuous climate:** `Globe::sample_climate(tile_x, tile_y)` bilinearly interpolates the four nearest cells (X-wrap, Y-clamp). `biome::classify_at_tile` runs *per-tile* — no biome stripes at cell boundaries.
- **Rivers & lakes** stamped into chunks: `bresenham_stamp` lays Water along river edges (depresses `surface_z` by 1); lakes flood-fill discs at `lake.level_z`.
- **Cached globe (`world.bin`):** `GlobeFile { version, globe }`; bump `GLOBE_FILE_VERSION` (currently `2`) on layout changes — auto-regenerates on mismatch. Determinism via per-component seeded RNGs.
- **Resource patch clustering (`chunk_streaming.rs`):** loose rocks, wild trees in fertile grass, and wild berry bushes are placed via a two-tier deterministic hash — a coarse `patch_hash` (`PATCH_CELL_SIZE=6` tiles, separate seeds per kind: `ROCK_PATCH_SEED`, `TREE_PATCH_SEED`, `BERRY_PATCH_SEED`) gates whether a cell is a patch; a per-tile hash gates density inside the patch. Result: discrete groves / berry patches / rock fields rather than a uniform carpet. Forest biome and Farmland keep their original uniform spawn rates (already curated zones).
- **Loose rocks (`chunk_streaming.rs::spawn_chunk_loose_rocks`):** Stone GroundItems (qty 1–3); ~30% of stone-surface patch cells are rocky, ~70% of tiles inside a rocky patch get a rock (~21% overall, was 35% uniform). Per-tile hash seed `ROCK_HASH_SEED=0xDEAD_C0DE`.

## Geology & mining

- **Surface vs subsurface:** `surface_kind_fn` chooses surface tile by biome thresholds. Below: `proc_tile` runs cave-noise → `topsoil_depth(biome)` of Dirt → ore vein lookup → `Wall` bedrock. Caves take precedence over ore. `topsoil_depth`: Mountain 1, Desert/Tundra 2, Grassland 3, forests 4, Ocean 0.
- **Ore veins (`ORE_BANDS`):** 6 3D Perlin fields, seeded `WORLD_SEED+2..=7`. Shallow→deep: Coal (1..6, 0.45), Copper (2..8, 0.50), Tin (5..12, 0.55), Iron (6..14, 0.52), Silver (10..18, 0.60), Gold (14..32, 0.65). Encoded as `TileKind::Ore` + `TileData.ore: u8` (`OreKind`). Surface stone never drops random ore — players must dig down.
- **Mining yields (`carve.rs::carve_tile`):** `Wall|Stone → (Stone, 2)`; `Ore → (ore_yield_good(ore), 2)`. `gather.rs`/`dig.rs` route via `route_yield`/`Carrier::try_pick_up`. `ActivityKind` count: 13 — keep `ACTIVITY_KINDS`/`ACTIVITY_NAMES` (debug_panel.rs) and `activity_name` (tech_panel.rs) in sync when adding. When the floor is already passable (procedural topsoil Dirt), `carve_tile` still writes a delta with the existing kind so `Chunk::set_delta` refreshes `surface_kind` off `Air`.
- **⚠️ Two tile-read paths:** `chunk.rs::tile_at_local` (cache-only) returns `Wall` for any uncarved subsurface tile — passability/LOS-correct but **material-wrong**. For ore-accurate reads (mining yields, inspector display) call `terrain::tile_at_3d`.

## Streaming schedule

- **`chunk_streaming_system` runs on `FixedUpdate` (20 Hz)**, not `Update` (60+ Hz). The per-frame variant burned cycles on the full-map unload filter even when the camera was stationary. One fixed-tick (≤50 ms) load lag is imperceptible. `update_chunk_retention_system`, `update_simulation_focus_system`, and `fog::fog_update_system` move with it; gizmos and `update_tile_z_view_system` stay on `Update`.
- **`update_tile_z_view_system`** carries a `Local<Option<i32>>` last-applied-z guard. Bevy's `is_changed()` fires false-positives (first-tick re-fire, same-value `set_changed()` writes) and the system iterates the full loaded-sprite set on every fire.
