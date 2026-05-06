# World (`src/world/`)

Procedural terrain, globe generation, climate, geology, and the spatial index. See the root `CLAUDE.md` for tile/chunk/Z conventions and `SpatialIndex` discipline.

## World generation

- **Terrain noise (`terrain.rs::surface_v`):** 4-octave FBM (continental 0.005 / base 0.02 / 2× / 4× harmonics, weights 0.35/0.40/0.18/0.07), reshaped via `sign(c)*|c|^0.65` to push to Z extremes. Lowering base freq or power = bigger / more dramatic features.
- **Globe pipeline (`globe.rs::generate_globe`):** 256×128 climate-cell grid. (1) `plates.rs` Lloyd-relaxed Voronoi over `NUM_PLATES=8` with motion vectors → uplift/subsidence. (2) Heightmap = 70% multi-octave Perlin + 30%×1.4 plate uplift. (3) `erosion.rs` thermal(20) → hydraulic(40). (4) `hydrology.rs` pit_fill → flow_dirs (D8) → flow_accum → `extract_rivers(min_accum=80)` + `LakeMap`. (5) `climate.rs` temp from latitude+elev; rainfall = base Perlin + orographic along latitude-banded prevailing winds. (6) Per-cell biome via `biome::classify`. ~50ms.
- **Climate-grid vs mega-chunk decoupling:** `GLOBE_WIDTH=256, GLOBE_HEIGHT=128, GLOBE_CELL_CHUNKS=4` → climate cell = 4×4 chunks = 128×128 tiles. `MEGACHUNK_SIZE_CHUNKS=16` → mega-chunk = 16×16 chunks = 512×512 tiles = 4×4 climate cells. Total world: 1024×512 chunks ≈ 32K×16K tiles.
- **Continuous climate:** `Globe::sample_climate(tile_x, tile_y)` bilinearly interpolates the four nearest cells (X-wrap, Y-clamp). `biome::classify_at_tile` runs *per-tile* in `generate_chunk_from_globe` and `tile_at_3d` — no biome stripes at cell boundaries.
- **Rivers & lakes** stamped into chunks: `bresenham_stamp`s a Water swath along river edges (depresses `surface_z` by 1); lakes flood-fill discs with Water at `lake.level_z`. Both pre-computed in `globe`.
- **Cached globe (`world.bin`):** Versioned `GlobeFile { version, globe }`; bump `GLOBE_FILE_VERSION` (currently `2`) on serialised-layout changes — auto-regenerates on mismatch. Determinism via per-component seeded RNGs.
- **Loose rocks (`chunk_streaming.rs::spawn_chunk_loose_rocks`):** Scatters Stone GroundItems (qty 1–3) on ~35% of surface Stone tiles using `ROCK_HASH_SEED=0xDEAD_C0DE`. Immediately scavengeable.

## Geology & mining

- **Surface vs subsurface:** `surface_kind_fn` chooses surface tile by biome thresholds. Below: `proc_tile` runs cave-noise → `topsoil_depth(biome)` of Dirt → ore vein lookup → `Wall` bedrock. Caves take precedence over ore. `topsoil_depth`: Mountain 1, Desert/Tundra 2, Grassland 3, forests 4, Ocean 0.
- **Ore veins (`ORE_BANDS`):** Six 3D Perlin fields, seeded `WORLD_SEED+2..=7`. Shallow→deep: Coal (1..6, 0.45), Copper (2..8, 0.50), Tin (5..12, 0.55), Iron (6..14, 0.52), Silver (10..18, 0.60), Gold (14..32, 0.65). Encoded as `TileKind::Ore` + `TileData.ore: u8` (`OreKind`). Surface stone never drops random ore (no `bonus_yields`) — players must dig down.
- **Mining yields (`carve.rs::carve_tile`):** `Vec<(Good, u32)>` per block. `Wall|Stone → (Stone, 2)`; `Ore → (ore_yield_good(ore), 2)`. `gather.rs`/`dig.rs` route via `route_yield`/`Carrier::try_pick_up` and credit the matching `ActivityKind`. `ActivityKind` count: 13 (incl. CopperMining=9, TinMining=10, GoldMining=11, SilverMining=12; update `ACTIVITY_KINDS`/`ACTIVITY_NAMES` in debug_panel.rs and `activity_name` in tech_panel.rs when adding). When the floor is already passable (procedural topsoil Dirt), `carve_tile` still writes a delta with the existing kind so that `Chunk::set_delta` refreshes `surface_kind` off its `Air` placeholder — without this the right-click menu hides Dig Down on subsequent clicks within the topsoil column.
- **⚠️ Two tile-read paths:** `chunk.rs::tile_at_local` (cache-only) returns `Wall` for any uncarved subsurface tile — passability/LOS-correct but **material-wrong**. For ore-accurate reads (mining yields, inspector display) call `terrain::tile_at_3d`.
