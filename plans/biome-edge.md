# Organic Biome Edge

Make biome borders feathered/patchy in **terrain + previews** without touching canonical climate/hydrology. Mirrors the proven `plates::assign_nearest` domain-warp pattern, but as a separate *surface-biome* layer.

## Approach

1. **Stateless hash value-noise** (`src/world/biome.rs`) — pure fn of `(seed, x, y)` smoothstep-bilerped over an integer lattice. Two decorrelated channels for the domain-warp X/Y offsets, a third for ecotone dither. No new crate, no per-call Perlin construction (preview path only has `&Globe`; per-tile/per-pixel call volume forbids `Perlin::set_seed` here).

2. **Surface-biome API** (`src/world/biome.rs`) — keep `classify` / `classify_at_tile` untouched (canonical). Add:
   - `surface_warp_offset(seed, tx, ty) -> (f32, f32)` — tiles.
   - `classify_land(elev_f, temp_f, rain_f) -> Biome` — split out of `classify`, runs the Wetland/Badlands/Steppe/Whittaker logic without the Ocean/Mountain gates.
   - `classify_surface_at_tile(globe, tx, ty) -> Biome` — true-elev gates (Ocean<0.22, Mountain>0.82) return identical to canonical; otherwise call `classify_land` on **warped** temp/rain (true elev). Structural guarantee: no inland oceans, coasts/water columns/salinity unchanged.
   - `SurfaceBiomeSample { base, accent, accent_weight }` + `surface_biome_sample_at_tile(globe, tx, ty)`.

3. **O(1) ecotone** (replaces 18-tile spatial probe): `base` from primary warp, `accent` from a second decorrelated warp, `accent_weight = smoothstep(distance-to-nearest-classify_land-threshold)` dithered by a fine hash-noise, capped at **0.35**, 0 deep inside a biome. Band width follows climate-gradient magnitude → naturally lands in ~12–36 tiles. Kind selection picks `accent`'s `biome_bands` vs `base`'s when a per-tile hash sample < `accent_weight` (deterministic, not `fastrand` speckle). Transitional materials emerge from existing palettes (Scrub between Grassland/Desert, etc.) — no new TileKinds.

4. **Single source of truth** (`src/world/terrain.rs`):
   - `generate_chunk_from_globe` Pass 1: one `surface_biome_sample_at_tile` per tile → `biome_cache` (base); pick `surface_kind` via the two palettes; pass biome into `surface_v`.
   - `surface_v` takes biome as a param (no more internal `classify` call) so `local_detail_amp` matches the visible biome.
   - Pass 3 (riparian), Pass 4 (reservoirs), Pass 4.5 (aquifer) unchanged — water still authoritative and later.
   - `climate_fertility_estimate_at` and `tile_at_3d` swap to `classify_surface_at_tile`.
   - **Keep canonical** (do not convert): `globe.rs:721` (stored `cell.biome`), `nomad.rs:2979` (camp scoring), `biome.rs:65` (`water_kind_at` salinity defence).

5. **Previews** (`src/ui/world_map.rs`, `src/ui/spawn_select.rs`):
   - `WORLD_MAP_OVERSAMPLE 2→4`; update doc comment.
   - `egui::TextureOptions::NEAREST → LINEAR` on both uploads (accept slight softening of 1px grid/river overlays).
   - `build_globe_image` + world-map hover tooltip → `classify_surface_at_tile`.
   - `sample_dominant_biome` → small `classify_surface_at_tile` tile-grid sample over the megachunk.

6. **Docs**: terse bullets in `src/world/CLAUDE.md` + `src/ui/CLAUDE.md`. `GLOBE_FILE_VERSION` not bumped by this layer (no serialized change).

## Tests (`cargo test --bin civgame`)

- Warp determinism + seed-sensitivity + purity.
- Gate preservation: tiles inside Ocean/Mountain elevation gates classify identically to canonical `classify` (no inland oceans).
- Preview↔terrain parity: `surface_biome_sample_at_tile(...).base` == `biome_cache` entry for the same `(seed, tx, ty)`.
- Ecotone: `accent_weight ∈ [0, 0.35]`; 0 deep interior; >0 near a `classify_land` threshold; synthetic sweep shows feathered (not isoline) transition.
- World-map image dims scale with oversample 4; multi-biome variation on a continental sample.
- Manual: `cargo run`, open Tab world map + spawn select — borders organic, coasts unchanged, no inland water.
