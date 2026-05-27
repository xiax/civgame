# Biome-Native Plants — Follow-up Work

## Status (2026-05-27 fourth pass)

Phases 1-8 shipped. Phase 6 ships the resolver + asset slots; per-species PNG
authoring is an incremental art-pipeline task — the resolver falls through to
form fallback then legacy ASCII so missing PNGs render the legacy sprite, not
a blank. **Review-flagged bugs P1a / P1b / P2a / P2b also shipped** — catalog
seeds now plant (production), catalog sightings now validate (gather), scatter
honours native range + river_distance + coastal at both wild and cultivated
sites (plants), chunk spawn enforces river_distance (chunk_streaming). All
1388 binary tests pass.

| Phase | What | Status |
|---|---|---|
| 1 | Per-species lifecycle from `PlantLifecycleProfile` | shipped |
| 2 | Multi-profile harvest dispatch (oak fruit + wood) | shipped |
| 3 | Memory tagging via `plant_memory_kinds(species, …)` | shipped |
| 4 | Farm seed picker walks catalog (multi-crop sowing) | shipped |
| 5 | Founder seed grant per native plantable | shipped |
| 6 | Per-species PNG resolver + 3 variants per (form, stage) | shipped (resolver only; PNGs incremental) |
| 7 | Catalog / spawn / flora-region determinism tests | shipped |
| 8 | `PlantCatalog.native_pools` per-(realm, biome) cache | shipped |

## Phase 6 — sprite pipeline (shipped 2026-05-27)

**Shape:**
- `PlantDef.sprite_keys: PlantSpriteKeys { folder, variants }` — RON override
  pointing the catalog at a custom art folder + variant count. Empty default
  uses species key + 3 variants.
- `PlantSpriteVariant(u8)` component stamped at spawn from `splitmix64(species,
  tile) % 3`; survives stage transitions so a Mature oak that started Seedling
  variant 1 stays variant 1.
- `rendering::plant_sprites::PlantSpriteSet` resource probes
  `assets/textures/plants/<folder>/<stage>_<variant>.png` at startup via
  `std::path::Path::exists()` — only loads handles for files actually on disk
  so the resolver can fall through cleanly.
- `rendering::entity_sprites::resolve_plant_sprite` three-tier fallback:
  species PNG → form PNG (`_form_<form>/`) → legacy ASCII.
- `spawn_plant_sprites` + `update_plant_sprites` queries take
  `Option<&PlantSpecies>` and `Option<&PlantSpriteVariant>` so legacy test
  fixtures that spawn raw `Plant` still render through the resolver.

**Authoring pipeline (incremental, not blocking):**
- 16×16 px (Grass/Forb/Shrub/Vine/Aquatic/Cactus/Tuber) or 32×32 px (Tree).
- File names: `seed.png`, `seedling_0.png`/`_1.png`/`_2.png`, `harvested.png`,
  `mature_0..2.png`, `overripe_0..2.png`. Per species: 11 PNGs.
- Form-bucket fallback at `assets/textures/plants/_form_<form>/...` lifts the
  whole bucket at once (8 forms × 11 PNGs).
- First authoring pass to maximise visual impact: form fallback PNGs for
  Cactus, Tuber, Vine, Aquatic (currently rendering as grain/bush), then
  per-species art for visually-distinct outliers (saguaro, banana, papyrus,
  coconut, mangrove, date_palm).

**Files (shipped):**
- `src/simulation/plant_catalog.rs` — `PlantSpriteKeys` field on `PlantDef` +
  `ResolvedPlantDef`; `sprite_folder()` and `sprite_variants()` helpers.
- `src/simulation/plants.rs` — `PlantSpriteVariant(u8)` component;
  `plant_sprite_variant_for(species, tile)` splitmix64; insertion at both
  `spawn_plant_at` + `spawn_plant_at_species`.
- `src/rendering/plant_sprites.rs` (new) — `PlantSpriteSet`, `PlantStageSlot`,
  `load_plant_sprites`, `setup_plant_sprites` startup system.
- `src/rendering/entity_sprites.rs` — `resolve_plant_sprite`,
  `get_plant_texture_legacy`; `spawn_plant_sprites` + `update_plant_sprites`
  rewritten to use the resolver.
- `src/rendering/mod.rs` — module declaration + resource + startup wiring.

## Phase 4 follow-up — `FieldTileState.last_crop` widening (deferred)

The plan called for widening `last_crop: Option<PlantKind>` → `Option<PlantSpeciesId>`.
Today `last_crop` is only written (in `gather_system`'s Grain branch) and never read —
fallow recovery operates on a flat per-day nutrient bump regardless of prior crop.
Widening is cosmetic until a rotation/crop-specific nutrient model lands. When it
does: add `last_crop_species: Option<PlantSpeciesId>` as a sibling rather than
breaking every `FieldTileState` literal in the test fixture (~20 sites).

## Phase 3 follow-up — dedicated HTN methods for fiber/medicine/dye/resin/latex/oilseed (optional)

`AcquireGood{resource_id}` already routes any resource id through `GatherFromKnownMethod`,
so chief postings for fiber etc. trigger gather flows via the generic path. A
dedicated method per resource would tweak utility / preconditions but isn't required
for end-to-end demand-to-gather flow.
