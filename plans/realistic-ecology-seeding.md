# Realistic Ecology Seeding, Territories, and Migration

**Summary**
- Build a high-realism, data-driven ecology layer: plants seed by ecological communities and animals seed from habitat suitability, territories, and migration routes.
- Keep the existing plant catalog, but replace the current per-tile lottery with chunk-level density targets and community roles.
- Replace hard-coded animal counts in [animals.rs](/Users/xiao1/civgame/src/simulation/animals.rs) and the horse/cow-only herd registry with a general wildlife population registry that supports resident territories and world routes.

**Key Changes**
- Add `assets/data/animals/core.ron` plus `src/simulation/animal_catalog.rs`.
  Define each species with habitat ranges, social pattern, diet, territory size, birth season, aggregate counts, bloom cap, and migration strategy.
- Add `AnimalSpeciesId`, `AnimalSpecies`, `WildlifePopulation`, `AnimalTerritory`, and `WildlifeMember`.
  Existing marker components like `Wolf`, `Deer`, `Horse`, etc. remain for current systems and rendering.
- Generalize [wild_herd.rs](/Users/xiao1/civgame/src/simulation/wild_herd.rs) into a `WildlifeRegistry`.
  Horse/cow herds become catalog rows; deer, rabbits, boar, wolves, foxes, and wildcats also get aggregate populations.
- Extend `SpatialIndex` with missing `Rabbit` and `Fox` indexed kinds, and ensure every animal spawn, reproduction, bloom, and tamed-seed path inserts the correct `Indexed`.

**Implementation Plan**
- Plant seeding:
  Replace `spawn_chunk_plants` in [chunk_streaming.rs](/Users/xiao1/civgame/src/world/chunk_streaming.rs) with a two-pass seeder: first compute tile suitability and target density by stratum, then deterministically fill the best tiles by species.
  Extend `PlantSpawnRule` with temperature, rainfall, relief class, density target, and stratum fields. Keep existing fertility, river, coastal, realm, biome, surface, and patch gates.
  Preserve one visible `Plant` per tile for compatibility with `PlantMap`, gather, rendering, and obstacle clearing.
- Animal seeding:
  Add `seed_wildlife_populations_system` on `OnEnter(Playing)` after chunk/world generation. It samples globe cells and loaded tiles, scores habitat from climate, biome, relief, fertility, water, surface type, and floristic realm, then creates aggregate populations and territories.
  Remove fixed global constants such as `WOLF_COUNT`, `DEER_COUNT`, etc. Population counts come from catalog density and carrying capacity.
- Territories:
  Resident species get stable territory centers. Wolves/foxes/wildcats use mostly exclusive territories; rabbits, deer, horses, cattle, and pigs allow overlap by catalog rule.
  `animal_movement_system` keeps combat, fleeing, sleep, thirst, and tamed-follow behavior as higher priority. Normal wander chooses goals inside the current territory or seasonal range instead of random adjacent tiles.
- Migration:
  Add `MigrationStrategy::{Resident, LocalSeasonal, WorldRoute}`.
  World-route species store seasonal anchor cells and an A* route over globe cells that avoids ocean/mountain and favors water/forage corridors. Daily economy ticks advance the population along the route.
  Local-seasonal species keep one territory but shift its active center by season, water, snow, food pressure, and human disturbance.
- Materialization:
  Replace horse/cow-only bloom/collapse with `wildlife_bloom_system`.
  Camera and settled-region focus bloom visible members near the current territory or migration route. Collapse returns living members to aggregate count; deaths and taming permanently reduce the source population.
- Ecology interactions:
  Generalize `deer_graze_system` into species diet foraging. Grazers/browsers reduce hunger from grassland/plant forage; predators follow prey density and live prey sightings.
  Add lightweight `ForagePressureMap` so heavy grazing reduces local plant regrowth/scatter odds until seasonal recovery.
- Scheduling/performance:
  Use existing `SimulationSet` structure: sensing in `ParallelA`, movement/drink/graze in `Sequential`, aggregate population/migration in `Economy`.
  Add small `PerfWorkBudget` caps for wildlife population updates, route advancement, and bloom spawns. No new crates.

**Test Plan**
- Unit-test animal catalog loading, unknown resource/species rejection, and deterministic species ids.
- Unit-test habitat scoring: desert animals reject taiga; wetland species prefer marsh/river; wolves prefer prey-bearing forests/taiga; horses prefer grassland/steppe.
- Unit-test migration routes: no ocean/mountain route cells, seasonal anchors differ for migratory species, same world seed gives identical routes.
- Integration-test chunk materialization: a wildlife population blooms near camera, collapses when far, and preserves deaths/tames.
- Regression-test plant seeding: no plants on reserved/ag tiles, native plants stay in valid realms/biomes, river plants concentrate near rivers, density stays below target caps.
- Run `cargo check` and `cargo test --bin civgame`.

**Assumptions**
- “Real life” means Earth-analog ecology, not exact GIS-accurate modern species ranges.
- First implementation targets the existing animal set: wolf, deer, horse, cow/aurochs, rabbit, pig/boar, fox, wildcat, plus domestic mappings for dogs/cattle/cats.
- Documentation updates go into `AGENTS.md`, `src/simulation/CLAUDE.md`, and `src/world/CLAUDE.md` after behavior changes.
