# Realistic Ecology Seeding, Territories, and Migration

## Goal
Seed wild plants by ecological community + density targets driven by terrain/soil/moisture/
relief, and wild animals by habitat suitability + territories + migration — replacing
hard-coded animal counts and the flat plant lottery. Earth-analog ecology, not GIS-accurate
species ranges.

## What already exists (reuse — don't reinvent)
- **Plant seeder is NOT a naive lottery.** `chunk_streaming.rs::spawn_chunk_plants` (812-972)
  already uses per-`(realm,biome)` `native_pools`, per-tile gates (surface/fertility/river/
  coastal/patch-noise), cumulative-weight pick, and per-biome `EMPTY_BIAS`. Layer a community/
  stratum model on top; reuse every gate + the deterministic per-tile hashing.
- **`spawn_animals` and `wild_herd.rs` are two parallel systems; Horse/Cow seeded by both.**
  `spawn_animals` (626-1001) spawns **~1,490 full entities at startup** from hard-coded
  `*_COUNT` (WOLF 150/DEER 400/HORSE 200/COW 80/RABBIT 500/PIG 120/FOX 80/CAT 60).
  `wild_herd.rs` separately runs 3 Horse/Cow **aggregate** herds with bloom/collapse — the
  correct generalization target (aggregate_count, leader_tile, range_center, bloom/collapse at
  camera dist 32/48, seasonal drift, predator flee, water-seek, births).
- **Terrain data available** (no new worldgen needed): `Globe::sample_relief` (slope, TPI,
  aquifer_depth_norm = moisture, relief class), `sample_climate`, `classify_at_tile` (Biome),
  `river_distance_at`, `salinity_at`, `floristic_region_at_tile` (12 realms),
  `locality::forest_density_around`. **Aspect is NOT computed — defer it.**
- **`abstract_faction.rs`** materialize-on-`ChunkLoadedEvent` is the precedent for aggregate
  globe entities blooming into entities near the player.

## Ship order (lowest blast radius first)
Numbered by domain (Plants=1, Animals=2, Dynamics=3); shipped in risk order:
0. `IndexedKind` Rabbit/Fox fix (standalone bugfix, ~10 lines).
1. Phase 1 (plant communities) — data-driven, no entity-count change, fallback to legacy lottery.
2. Phase 2 (wildlife registry) — deletes `*_COUNT`, changes startup count, touches tests/combat/hunting.
3. Phase 3 (dynamics) — territories/migration/foraging; bundle the cadence-gating fix.

---

## Step 0 — `IndexedKind` Rabbit/Fox fix (ship FIRST)
Latent bug: Rabbit (animals.rs:851) and Fox (:930) spawn with **no `Indexed` component** —
invisible to every `SpatialIndex` scan. Add `Rabbit`+`Fox` to `IndexedKind` (spatial.rs:21) +
`is_mobile_agent()` (:41); add `Indexed::new(...)` to both spawn bundles AND their reproduction
spawn paths (~:2353/:2390). Prerequisite for Phase 3 predator-prey foraging.

---

## Phase 1 — Plant community seeding
Convert `spawn_chunk_plants` (a per-chunk free fn, called at chunk_streaming.rs:1176) to a
community/stratum model, reusing every existing gate. Gate behind a flag with legacy-lottery
fallback for first ship.

**New data (`plant_catalog.rs` + `assets/data/plants/communities.ron`):**
- `PlantCommunityId(u16)`, `PlantCommunityDef { key, realms, biomes, moisture: RangeU8
  (bucketed aquifer_depth_norm), relief_classes, strata: Vec<{ stratum, target_per_cell: u8,
  species: Vec<(species_key, weight)> }> }`.
- `Stratum::from_form(PlantForm)`: Tree→Canopy; Shrub/Vine→Understory;
  Grass/Forb/Tuber/Cactus→GroundCover; Aquatic keeps existing coastal/river gating.
- `PlantCatalog` gains `community_pools: AHashMap<(FloraRealmKind,Biome), Vec<PlantCommunityId>>`.

**Two-pass `spawn_chunk_plants`:**
- **Pass A — per coarse cell (reuse `PATCH_CELL_SIZE=6`), PURE fn of `(cell_x,cell_y,seed)`:**
  pick community by suitability + `patch_hash`; per-stratum density target =
  `target_per_cell × cell_suitability`.
- **Pass B — per tile, Canopy→Understory→GroundCover:** tile must pass ALL existing gates
  (890-936); replace the `EMPTY_BIAS` lottery (940-960) with a deterministic per-tile-threshold
  fill (`fill_probability = stratum_target / cell_tile_count`, accepted from `(tx,ty,seed,
  stratum)` hash); pick species by cumulative weight; **gate every placement on
  `plant_map.get(tile).is_none()`** → one `Plant` per tile regardless of chunk load order.

**New suitability axes:** `tile_suitability(def, relief, moisture)` from `relief.slope` /
`topographic_position` (valley=mesic, ridge=xeric) / `aquifer_depth_norm`. **No temp/rain
gates — biome already encodes them.** `PlantSpawnRule` may gain optional `relief_classes` +
`moisture: RangeU8` (both `#[serde(default)]`).

**Risks:** load-order determinism (community pick pure-per-cell; PlantMap absence guard);
density regression (tune `target_per_cell` ≈ current output; before/after tile-count compare);
perf (Pass A continuous samples once per coarse cell, not per tile).

---

## Phase 2 — Wildlife population registry
Kills hard-coded counts + the 1,490-entity startup. Highest blast radius.

**New (`animal_catalog.rs` + `assets/data/animals/core.ron`)** — mirror `plant_catalog.rs`
(alphabetical key sort → stable `AnimalSpeciesId(u16)`; panic on dup/unknown):
- `AnimalSpeciesDef { key, display_name, marker (→ Wolf/Deer/…), social {Herd,Pack,Solitary}
  (replaces consts at animals.rs:559-569), diet {Grazer,Browser,Omnivore,Carnivore},
  habitat { biomes, relief_classes, moisture: RangeU8, min_forest_density, requires_water_within },
  territory { size_tiles, exclusive }, birth_season, density_per_100_tiles, bloom_cap,
  migration {Resident,LocalSeasonal,WorldRoute} }`.
- `AnimalCatalog` + precomputed `habitat_pools: AHashMap<Biome, Vec<AnimalSpeciesId>>`.
- Pure `habitat_suitability(...) -> f32` in `[0,1]` (reuse `forest_density_around`,
  `sample_relief`, `classify_at_tile`, `river_distance_at`). `load_animal_catalog()` at
  `WorldPlugin::build`.

**Generalize `wild_herd.rs` → wildlife registry (ALL wild species):** `WildHerd` →
`WildlifePopulation` (add `species_id`, `social`, `territory_center`, `territory_radius`,
`exclusive`, `migration`); `WildHerdRegistry` → `WildlifeRegistry`; keep `WildHerdMember`
(load-bearing for taming/collapse). Bloomed members stay normal Wolf/Deer/… entities.

**`seed_wildlife_populations_system`** (OnEnter, after world/chunk gen, **before**
`seed_starting_tamed_animals_system`): sample candidate centers, score `habitat_suitability`,
set `aggregate_count` from `density × suitable_area`. **Per-species RNG sub-stream from
`(world_seed, species_id)`** (the current shared `fastrand` at animals.rs:633 is order-
dependent — fix). Delete `WOLF_COUNT…CAT_COUNT` (16-23). Most populations stay aggregate; bloom
near focus.

**Shared spawn helper:** extract `spawn_one_animal(commands, species, tile, slot, …)` from the
per-species bundles (683-993); reuse in bloom + reproduction + seeding so bundles (Health,
`DeerGrazer`, HERD-only `HerdMember`, `Indexed`) can't drift.

**Perf:** add `PerfWorkBudget.wildlife_bloom_spawns_per_tick` (~32) + bloom cursor (amortize the
"up to 60 at once" spawn spike). **Net:** aggregate registry is server-only; bloomed members are
existing replicated entities → **no PROTOCOL_VERSION bump** (currently 10); clients re-derive
populations from `WorldSeed`.

**Risks:** collapse accounting (preserve the alive-only restore filter + `remove_member` on
tame, or `aggregate_count` drifts); population leak; `BucketSlot` lifetime (advance
`clock.population` per spawn); Bevy query aliasing (keep disjoint per-species queries); test
breakage (convert `*_COUNT` entity-count asserts to registry-count); hunting near settlements
(`chief_hunt_order_system` r=40 sees only bloomed — seed player-region populations as bloomed /
bloom on first tick; `SimulationFocus` covers settled regions).

---

## Phase 3 — Territories, migration, foraging
Depends on Phase 2's registry + Phase 1's plant density.

**Territories:** add `territory_radius` + `exclusive` (catalog). Seed centers with deterministic
rejection sampling (exclusive species can't overlap same-species territory). `animal_movement_
system` **Wander fallback only** biases goals within `territory_radius` of `range_center`
(combat/flee/thirst/tamed-follow stay higher priority).

**Migration (`MigrationStrategy`):**
- **Fix the forbidden cadence pattern here:** `wild_herd_migration_system` uses
  `if clock.tick % TICKS_PER_DAY != 0 { return }` (wild_herd.rs:242) — convert to a
  `WildlifeMigrationCursor` draining `budget.wildlife_pop_updates_per_tick`, each gated by
  `(tick + pop_id) % TICKS_PER_DAY` (per-faction-stagger idiom).
- **LocalSeasonal**: existing season/water/snow/camp-avoid drift; extend to read `food_pressure`
  (ForagePressureMap) + human disturbance.
- **WorldRoute** (new): `seasonal_anchors` + A* over globe cells (512×256) avoiding
  ocean/mountain, favoring water/forage; computed once per population per season (cached, seeded
  from `world_seed`); advance `leader_tile` on the daily Economy tick; bloom near focus.

**Foraging + `ForagePressureMap`:** generalize `deer_graze_system` → `wildlife_forage_system`
(diet-based). `ForagePressureMap`: sparse `AHashMap<(i32,i32), ForageCell { pressure,
last_recover }>`, consulted by `spawn_chunk_plants` fill prob + `plant_lifecycle_system` scatter.
Off-chunk Resource (survives unload like `RuntimeWater`, no chunk-retention pin); **season-edge
recovery** (mirror `fallow_recovery_system`'s `Local<Option<Season>>`); drop `pressure==0`
cells. **Never `tick % N`.**

**Scheduling:** sensing→ParallelA; forage+territory wander→Sequential; aggregate-pop+WorldRoute→
Economy daily; ForagePressure recovery→Economy season-edge. `PerfWorkBudget` caps (cursor/stagger
only). **Risk:** plant↔animal feedback can oscillate — tune recovery so populations don't go extinct.

---

## Critical files
- Plant: `world/chunk_streaming.rs` (`spawn_chunk_plants` 812-972), `simulation/plant_catalog.rs`
  (`PlantSpawnRule` 280-315, `from_defs`/`native_pools`), `assets/data/plants/communities.ron` (new).
- Animal: `simulation/animal_catalog.rs` (new), `assets/data/animals/core.ron` (new),
  `simulation/wild_herd.rs` (generalize), `simulation/animals.rs` (delete `*_COUNT` 16-23,
  `spawn_animals` 626-1001, `IndexedKind`), `world/spatial.rs` (`IndexedKind`).
- Dynamics: `simulation/animals.rs`, `simulation/perf.rs` (budgets), `simulation/mod.rs` (registration).
- Mirror: `simulation/abstract_faction.rs`, `simulation/farm.rs` (`fallow_recovery_system`),
  `world/water_runtime.rs` (`RuntimeWater`).

## Determinism checklist
Per-species/community RNG sub-streams from `world_seed` + stable id (never a shared sequential
stream); catalog ids from alphabetical sort; never `AHasher::default()`; plant seeding pure-per-
tile + community pick pure-per-cell.

## Verification
- Unit: catalog load + unknown rejection + deterministic ids; `habitat_suitability` (desert
  rejects taiga; wetland prefers marsh/river; wolves prefer prey forest/taiga; horses prefer
  grassland); WorldRoute no ocean/mountain + identical per seed; `tile_suitability` monotonicity.
- Integration: population blooms near camera / collapses far / preserves deaths+tames; two chunks
  sharing a community cell → identical plant tiles regardless of load order; no plants on
  reserved/ag tiles; density within caps.
- Regression: convert `*_COUNT` entity-count tests to registry-count.
- Manual: `cargo run` (NOT `--sandbox`) — forest shows sparse canopy + understory + ground cover;
  animals bloom near camera; hunting finds prey near settlements.
- `cargo check` + `cargo test --bin civgame`.

## Docs to update (after behaviour changes)
`simulation/CLAUDE.md` (Animal spawn distribution / Wild herds / Animal domestication; new
wildlife registry+territory+migration+foraging section; Plant lifecycle / Biome-native catalog),
`world/CLAUDE.md` (Floristic regions & native-plant spawn — two-pass community seeder),
root `CLAUDE.md` (new `PerfWorkBudget` fields), `net/CLAUDE.md` (only if aggregate wildlife is
added to `BootstrapSnapshot` — deferred; clients re-derive from `WorldSeed`).
