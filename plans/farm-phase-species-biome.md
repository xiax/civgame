# Farm Phase: Species- and Biome-Aware Seasonal Work

## Context

`FarmSeasonPhase` (`src/simulation/farm.rs:140-160`) maps `Calendar.season` to a single global phase, and the per-plot work-availability scan in `goal_update_system` (`src/simulation/goals.rs`, the precompute block introduced ~`goal_update_system` body) hardcodes `PlantKind::Grain` (Autumn arm) and `core_ids::grain_seed()` (Spring arm). This makes the "is there farm work right now?" query model the wrong thing in three concrete ways:

1. **Tropical / equatorial biomes** see global `WinterDormant` even though their growing season is rain- not temperature-driven. A coastal-tropical household in our Winter has just as much sowable land as in Spring.
2. **Multi-species plots.** `PlantCatalog` carries per-species `lifecycle.sowing_seasons` (e.g. `BerryBush`/`Tree` = Spring + Autumn) and per-species harvest profiles (`OnFruitSeason(Autumn)`, `OnFell`, etc.). A mature BerryBush patch in Autumn is real harvest work, but the scan only ever counts `PlantKind::Grain` mature plants. Multi-yield species (oak → fruit + wood) likewise vanish from the gate.
3. **Non-grain seedable species.** A household holding flax, papyrus, or millet seed but no `grain_seed` reads as "no plantable work" even with empty prepared fields, because `household_has_grain_seed` is a single-resource probe.

These bugs predate the cadence-cache perf fix (`plans/fix-year-one-slowdown.md`, shipped). The cache made the wrong query fast; this plan makes it right.

## Approach (skeleton)

Replace the global `FarmSeasonPhase` consumer chain with a per-(plot, species) probe rooted in the existing `PlantCatalog` + `FloraRegionMap`. Two layers:

### Layer 1 — Per-plot, per-species sowable set

A pure helper `farm::sowable_species_at(catalog, flora_regions, calendar, tile) -> SmallVec<PlantSpeciesId>`:
- Read `floristic_region_at_tile(tile)` to get `(FloraRealmKind, Biome)`.
- Walk `PlantCatalog::native_pool_for(realm, biome)` — the precomputed native pool.
- Filter by `def.lifecycle.is_sowable_in(calendar.season)` *and* `def.seed.is_some()`.
- Return up to ~12 candidates (existing native pools are small).

This is the same query `htn_plant_from_storage_dispatch_system` already runs at the catalog level (`PlantCatalog` walk in `htn.rs` ~line 5160 region); lift it into one shared helper and call from both sites.

### Layer 2 — Reshape the `goal_update_system` precompute

Replace the global `match farm_season { … }` with a per-tile per-species probe:

- **Plantable check:** pick representative tile of plot, get `sowable_species` set. For each candidate, check `faction.storage.stock_of(def.seed.unwrap()) > 0` (or parent-village fallback). Any hit ⇒ plantable work exists. (Cache the per-tile sowable set across plot tiles since biome is constant within a plot.)
- **Unprepared check:** unchanged — `!is_cropland || nut < EXHAUSTED_FLOOR` is species-agnostic, still valid year-round wherever there's *any* sowable species.
- **Harvest check:** walk `PlantMap` for the plot rect; for each plant entity read `PlantSpecies` + `PlantCatalog::pick_harvest_profile(species, stage, season, has_tool=false, prefer_despawn=false)`. Profile resolves ⇒ harvest work. Drops the `PlantKind::Grain` hardcoding; oak fruit, BerryBush, flax fiber, papyrus all surface.

Drop `FarmSeasonPhase::WinterDormant` as a structural gate. The cache stays — cadence-invalidated, **and** invalidated on `Calendar.season` change. A tropical biome with year-round sowable species will simply never produce an empty set; that's the intended behavior.

### Layer 3 — Downstream consumers of `FarmSeasonPhase`

`FarmSeasonPhase` is also read by:

- `chief_job_posting_system` Farm branch (`jobs.rs`) — Spring posts Prepare+Plant, Autumn posts Harvest, Winter skips.
- `fieldwork_expiry_system` (`farm.rs:1277`) — gates `FieldWork` postings on season validity.
- `compute_priority` seasonal-farm boost (`jobs.rs`).
- `seasonal_field_work_floor_share` (claim cap).
- `chief_job_posting_system` Plow Spring branch.

Each needs lifting from `FarmSeasonPhase` to a per-plot question: "is *this* plot — at *this* biome, holding *these* species' seeds — in Prepare/Plant/Harvest right now?". Mechanically this is the same pattern: replace `match calendar.season` with a per-plot probe. The chief posts are still emitted per-plot today, so the call site change is small.

Tropical-biome edge case: a plot can be in Plant *and* Harvest phase simultaneously (different species). Postings need to accommodate either both phases being open or a per-species posting tag. Probably the cleanest answer is per-species `FieldWork` postings (`JobProgress::FieldWork { phase, species: Option<PlantSpeciesId>, … }`) so a single plot can have one open Plant-flax + one open Harvest-oak.

## Critical files

- `src/simulation/farm.rs` — new `sowable_species_at` helper, retire `FarmSeasonPhase` or relabel as legacy-only convenience.
- `src/simulation/goals.rs` — precompute body (the block now wrapped in `info_span!("farm_precompute")`); replace `match farm_season` with the per-species probe. The cache wrapper stays.
- `src/simulation/jobs.rs` — `chief_job_posting_system` Farm + Plow branches; `compute_priority` seasonal lift; `seasonal_field_work_floor_share`.
- `src/simulation/farm.rs` — `fieldwork_expiry_system` validity check.
- `src/simulation/plant_catalog.rs` — possibly add `def.farm_role: PlantFarmRole` (Crop / Orchard / Fiber / Forager) to drive which species count toward "plot is being farmed" vs "plot is foraged".
- Wire / state changes: `JobProgress::FieldWork` may grow `species: Option<PlantSpeciesId>` — bumps `PROTOCOL_VERSION` and the `BootstrapSnapshot` posting field.

## Open questions

- **Does `Calendar` model latitude / hemisphere?** If not, "tropical" is biome-only; if yes, Southern hemisphere needs season inversion in the helper. (Need to check `world/seasons.rs` — out of scope for this skeleton.)
- **Per-species postings vs per-plot phase set.** Per-species is cleanest semantically; per-plot phase set (`Vec<FarmPhase>`) is cheaper but loses the "which species" needed for `WithdrawMaterial` seed picking. Lean per-species.
- **Multi-yield species harvest gating.** Oak in Autumn = fruit (bare hands) *and* wood (axe). Should "plot has harvest work" require an axe-holder in the household, or always count? Probably always count — dispatcher already resolves the right profile per worker via `prefer_despawn`.
- **Migration path.** Does `FarmSeasonPhase` survive as a legacy convenience that wraps the per-plot probe (passing `tile = home_tile`) for any test fixture that needs it, or is it deleted outright? Recommend keeping as a deprecated wrapper for one release cycle.

## Verification

- Headless fixture: plant a BerryBush patch in Autumn, assert `FarmWorkScorer` flips affected household to `AgentGoal::Farm` (currently doesn't).
- Headless fixture: spawn a household at a tropical biome tile, advance to global Winter, assert sowable species are non-empty and farm work is detected.
- Headless fixture: household with flax seed but no grain seed in Spring on prepared cropland, assert plantable work detected.
- Regression: all existing `FarmWorkScorer` / `chief_job_posting_system` Farm tests still pass with grain-only fixtures (the most common case).

## Out of scope

- Hemisphere modeling in `Calendar` (file separate plan if needed).
- Per-species nutrient debits (current `HARVEST_NUTRIENT_DEBIT` is a flat 30 regardless of species).
- Rain/wet-season modeling for true tropical agricultural realism — biome bucket is the proxy for now.

## Status

Skeleton — not yet ready to execute. Land the perf fix first (`plans/fix-year-one-slowdown.md`); revisit this when the species/biome gap surfaces in gameplay (likely once non-grain crops become first-class enough that "my flax-only household just sits there" becomes a visible bug).
