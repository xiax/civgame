# Plan: Realistic Tile Type Expansion

## Context

The world currently has 12 `TileKind` variants — most surfaces collapse onto `Grass`, `Forest`, `Stone`, or `Farmland`, with one generic `Dirt` for all topsoil and `Stone` for all rock. `Farmland` is a procedurally-spawned terrain band that doesn't reflect real soil — it's redundant with high-fertility Grass.

Goal: replace `Farmland` with grass-fertility-driven crop spawning, add surface variety (Sand / Snow / Marsh / Scrub), fork `Stone` into four lithologies (Granite / Limestone / Sandstone / Basalt), fork `Dirt` into four soils (Loam / Silt / Clay / Sandy), and add three new biomes (Wetland, Steppe, Badlands; reuse Taiga for Boreal Forest). Each variant gets a distinct color, pathing speed, and gameplay effect (mining yield rate, soil fertility multiplier, plant spawn bias).

A file in this repo already exists at `civgame/plans/`; per saved feedback ("Write design plans to a local file"), this same plan should be mirrored to `civgame/plans/tile_type_expansion.md` once the user approves.

## Scope summary

- **Remove**: `TileKind::Farmland` (slot freed; renumber not required since variants discriminated by discriminant value not index)
- **Add 4 surface variants**: `Sand`, `Snow`, `Marsh`, `Scrub`
- **Add 4 stone variants**: `Granite`, `Limestone`, `Sandstone`, `Basalt`. `Stone` retained as a generic fallback.
- **Add 4 soil variants**: `Loam`, `Silt`, `Clay`, `SandySoil`. `Dirt` retained as a generic fallback.
- **Add 3 biomes**: `Wetland`, `Steppe`, `Badlands` (extend `Biome` enum in `globe.rs`)
- **Wheat now spawns on high-fertility Grass** (no Farmland gate)
- **Mining yields** vary by stone lithology (hardness/yield count)
- **Soil fertility** drives a per-plant growth multiplier

---

## Implementation phases

### Phase 1 — TileKind enum + helper methods (`src/world/tile.rs`)

Append new variants (don't reorder; keeps serialization stable; Farmland=4 stays a hole or gets repurposed for `Sand`):

```rust
pub enum TileKind {
    Grass = 0, Water = 1, Stone = 2, Forest = 3,
    Sand = 4,                       // reuse Farmland slot
    Road = 5, Air = 6, Wall = 7, Ramp = 8, Dirt = 9,
    Ore = 10, River = 11,
    // New surfaces
    Snow = 12, Marsh = 13, Scrub = 14,
    // Stone variants
    Granite = 15, Limestone = 16, Sandstone = 17, Basalt = 18,
    // Soil variants
    Loam = 19, Silt = 20, Clay = 21, SandySoil = 22,
}
```

Add helpers:
- `is_stone_like()` → matches `Stone | Granite | Limestone | Sandstone | Basalt | Wall | Ore`
- `is_soil_like()` → matches `Dirt | Loam | Silt | Clay | SandySoil`
- `is_passable()` extended: `Sand | Snow | Marsh | Scrub | Loam | Silt | Clay | SandySoil | Granite | Limestone | Sandstone | Basalt` all passable; surface stones walkable like current `Stone`.
- `is_floor()` extended likewise.
- `stone_yield_count()` → 2 for Granite/Sandstone/Basalt, 3 for Limestone (soft).
- `soil_fertility_mult()` → Loam 1.5, Silt 1.4, Clay 1.0, SandySoil 0.6, Dirt 1.0.

### Phase 2 — Biome additions (`src/world/biome.rs`, `src/world/globe.rs`)

Extend `Biome` enum (`globe.rs:31-41`):
```rust
Ocean=0, Tundra=1, Taiga=2, Temperate=3, Grassland=4, Tropical=5,
Desert=6, Mountain=7, Wetland=8, Steppe=9, Badlands=10,
```

`biome::classify` (Whittaker-style; `biome.rs:12-27`):
- **Wetland**: low elevation (< 0.30) + rainfall > 0.75 + temp_f > 0.30 (warm/wet lowland)
- **Steppe**: rainfall 0.30–0.50, temp 0.40–0.70 (the dry-grassland gap between Grassland and Desert)
- **Badlands**: rainfall < 0.25, elevation 0.45–0.80 (eroded dry uplands between Desert and Mountain)

Keep existing branches; insert the new ones as guards before the catch-all.

Bump `GLOBE_FILE_VERSION` to **7** in `globe.rs` so cached worlds regenerate.

### Phase 3 — Surface tile assignment (`src/world/terrain.rs`)

Replace `surface_kind_fn(v, water_t, grass_t, farm_t, forest_t)` with `surface_kind_for_biome(biome, v) -> TileKind`. Each biome maps noise-band thresholds to its native palette:

| Biome | Bands (low→high `v`) |
|---|---|
| Ocean | Water → Sand (beach) → Granite |
| Tundra | Water → Snow → Scrub → Granite |
| Taiga | Water → Grass → Forest → Granite |
| Temperate | Water → Grass → Forest → Limestone |
| Grassland | Water → Grass → Forest → Limestone |
| Tropical | Water → Marsh → Grass → Forest → Basalt |
| Desert | Water → Sand → Scrub → Sandstone |
| Mountain | Granite → Granite → Basalt (uplift core) |
| Wetland | Water → Marsh → Grass → Forest |
| Steppe | Water → Scrub → Grass → Sandstone |
| Badlands | Sand → Scrub → Sandstone → Granite |

Keep `biome_thresholds` per-biome but its return shape changes — refactor to `BiomeBands { v0, v1, v2, v3 }` plus 4 `TileKind` slots. `proc_tile` plumbs it through.

**Topsoil variant** (`topsoil_depth` neighbor): add `topsoil_kind(biome, river_d)`:
- River band (`river_d <= 5`) → `Silt` (overrides biome)
- Wetland / Tropical → `Clay`
- Temperate / Grassland / Steppe → `Loam`
- Desert / Badlands → `SandySoil`
- Tundra / Taiga / Mountain → `Dirt` (fallback; permafrost flavor)

Replace the `TileKind::Dirt` literal in `proc_tile` (line 243) with `topsoil_kind(biome, river_d)`. River-distance lookup needs threading through; for now use the chunk's `surface_river_distance` cache (already populated pre-`proc_tile`).

**Fertility computation** (`terrain.rs:204` and `:441`): drop the `Farmland | Grass` match; fertility now applies to `Grass` only (it's the cropable surface). Multiplied by riverbank `river_fertility_mult` (existing).

**Riparian re-classification** (`terrain.rs:436` `greenness_rank`): drop `Farmland=3`. Adjust ranks to `Forest=4, Grass=2, Scrub=1, Sand=0, Snow=0, ...`.

### Phase 4 — Color map + sprite/material registry

`src/rendering/color_map.rs` — add a sRGB color per new variant (use natural-color references):
- Sand: `(0.86, 0.77, 0.55)`
- Snow: `(0.93, 0.94, 0.97)`
- Marsh: `(0.30, 0.45, 0.30)`
- Scrub: `(0.55, 0.62, 0.35)`
- Granite: `(0.55, 0.52, 0.50)`
- Limestone: `(0.78, 0.74, 0.62)`
- Sandstone: `(0.80, 0.62, 0.40)`
- Basalt: `(0.25, 0.22, 0.22)`
- Loam: `(0.42, 0.30, 0.18)` (dark brown)
- Silt: `(0.55, 0.45, 0.30)`
- Clay: `(0.60, 0.40, 0.30)`
- SandySoil: `(0.72, 0.60, 0.40)`

Remove `TileKind::Farmland` arm.

`src/world/chunk_streaming.rs::RENDERABLE_KINDS` (line 260): drop `Farmland`, add the 12 new variants. `setup_tile_materials` walks this constant — adding to it is enough.

### Phase 5 — Pathfinding cost (`src/pathfinding/tile_cost.rs`)

Drop the `Farmland => 0.85` arm. Add:
- Sand: 0.75 (slow loose footing)
- Snow: 0.6
- Marsh: 0.4 (slowest passable)
- Scrub: 0.9
- Granite/Limestone/Sandstone/Basalt: 1.0 (matches `Stone`)
- Loam/Silt/Clay: 0.9 (matches `Dirt`)
- SandySoil: 0.85

### Phase 6 — Mining yields (`src/simulation/carve.rs::carve_tile`)

Generalize the `Wall|Stone → (Stone, 2)` arm. New rule: any `is_stone_like()` tile yields `(Stone resource, kind.stone_yield_count())`. Wall + Ore retain their current paths (Wall yields raw stone count 2; Ore via `ore_yield_good`).

Soil variants (`Loam | Silt | Clay | SandySoil | Dirt`) when carved at floor level: keep the existing pass-through (already passable; `carve_tile` writes a delta in the same kind for sprite refresh). No yield change.

### Phase 7 — Plant spawning (replace Farmland-gated branch)

`src/world/chunk_streaming.rs:486` Farmland match arm becomes Grass-fertility-gated:

```rust
TileKind::Grass if tile.fertility > 180 => { /* same Grain/Berry roll as before, slightly reduced rates */ }
TileKind::Grass if tile.fertility > 100 => { /* current Grass branch (berry patch) */ }
TileKind::Marsh => { /* sparse BerryBush, no Grain */ }
TileKind::Scrub | TileKind::Sand | TileKind::Snow => { /* skip; barren */ }
```

`src/simulation/plants.rs:104` — extend `Grass | Farmland` fertility match to `Grass | Loam | Silt | Clay | SandySoil` so plants growing on exposed soil (e.g. river silt banks) participate; multiply the per-tick growth bonus by `kind.soil_fertility_mult()`.

`src/simulation/tasks.rs:477` (`Farmland` check) — generalize the planting-tile validity check to `Grass | Loam | Silt | Clay | SandySoil`. This is what lets wheat planting work on grasslands without a Farmland tile underneath.

### Phase 8 — Construction yard (remove Farmland writing)

`src/simulation/construction.rs:5461` `seed_farmstead_yard`:
- Stop writing `TileKind::Farmland`. Either:
  1. Leave the surface kind as-is (Grass/Loam) and just bump fertility to 200, or
  2. Delete the tile-write entirely and let the high-fertility natural soil handle it.

Pick option (1) — preserves the visual "yard" intent and the fertility boost without a synthetic tile type.

`construction.rs:2801` writability check: replace `Some(TileKind::Farmland)` with `Some(k) if k.is_soil_like()` (covers Loam/Silt/Clay/SandySoil/Dirt) plus the existing `Grass | Dirt` arms.

### Phase 9 — Doc updates (per saved feedback `update_claudemd.md`)

- Update root `CLAUDE.md` "Spatial / tile / rendering conventions" to list the new variants, mention `is_stone_like()` / `is_soil_like()` helpers.
- Update `src/world/CLAUDE.md`'s "Geology & mining" section: surface palette per biome; topsoil variant table; Wetland/Steppe/Badlands biome additions; bump `GLOBE_FILE_VERSION=7`.
- Note: wheat now spawns on high-fertility Grass, not Farmland.

---

## Critical files to modify

- `src/world/tile.rs` — enum + helpers
- `src/world/biome.rs` + `src/world/globe.rs` — biome enum, classification, version bump
- `src/world/terrain.rs` — `surface_kind_fn` rewrite, `topsoil_kind`, riparian rank
- `src/world/chunk_streaming.rs` — RENDERABLE_KINDS, plant spawn branches
- `src/rendering/color_map.rs` — colors
- `src/pathfinding/tile_cost.rs` — speeds
- `src/simulation/carve.rs` — stone yields
- `src/simulation/plants.rs` — soil fertility plumbing
- `src/simulation/tasks.rs` — planting validity
- `src/simulation/construction.rs` — drop Farmland write, soil-aware yard
- `CLAUDE.md`, `src/world/CLAUDE.md` — doc updates

## Verification

1. `cargo check` — must compile after enum churn (this is the canary; expect ~30 unhandled match arms across the codebase initially).
2. `cargo test --bin civgame` — existing 440 tests should still pass; pay attention to:
   - `tile.rs` passability tests
   - `terrain.rs` ocean_fraction_within_band test
   - any biome-shape tests
3. `cargo run` — visual verification (per saved feedback, never `--sandbox`):
   - Re-roll until each new biome appears: Wetland (lowland near rivers), Steppe (dry plains), Badlands (eroded uplands), Tundra (poles).
   - Verify Sand/Snow/Marsh/Scrub render with distinct colors.
   - Mine into a Limestone band and confirm yield count = 3 (vs. Granite = 2).
   - Wheat plants spawn on high-fertility Grass (no Farmland required); verify chunk-streaming pop-in shows wheat on grasslands near rivers.
   - Walk an agent through Marsh — confirm visibly slower (path cost 0.4).
4. Spot-check the inspector hover on a Loam vs. SandySoil tile — fertility multiplier should match the catalog values.

## Out of scope (defer)

- New harvested resources (Lime, Clay-as-resource, raw Sand crafting input). Stone variants all yield generic `Stone` for now.
- Recipe gates that require specific stone (e.g. Limestone-fired Lime for mortar) — future tech.
- Sprite-library pixel-art textures for new variants (using flat colors via `color_map.rs`; PNG textures defer to a sprite pass).
- Snow/permafrost subsurface (treat tundra subsurface as `Dirt` for now).
