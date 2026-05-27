# Biome-Native Useful Plants Expansion

## Summary

Add a data-driven plant ecology system that supports many historically useful species, strict procedural native ranges, richer wild foraging, crop transfer through exploration/trade, and new plant-based economy hooks.

The first pass should replace the hardcoded `PlantKind::{Grain,BerryBush,Tree}` model with species definitions loaded from data, then wire those definitions into spawning, vision, gathering, farming, crafting, medicine, UI, and tests.

Strict nativity will mean: wild plants only spawn and self-naturalize inside generated floristic regions where they are native; humans may cultivate non-native seeds elsewhere if climate/soil allows.

## Key API And Data Changes

- Add `PlantSpeciesId(u16)`, `PlantCatalog`, `PlantDef`, `PlantForm`, `PlantUse`, `PlantHarvestProfile`, `PlantLifecycleProfile`, and `PlantSpawnRule` in [plants.rs](/Users/xiao1/civgame/src/simulation/plants.rs), backed by `assets/data/plants/core.ron`.
- Change `Plant.kind: PlantKind` to `Plant.species: PlantSpeciesId`; replace hardcoded methods like `harvest_yield`, `sowing_seasons`, `clear_work_ticks`, and `harvest_activity` with catalog lookups.
- Keep small compatibility helpers for tests and legacy call sites: `species_by_key("emmer_wheat")`, `species_by_key("generic_berry_bush")`, `species_by_key("oak_tree")`.
- Add generated `FloraRegionId`, `FloraRealmKind`, and `Globe.flora_regions` in [globe.rs](/Users/xiao1/civgame/src/world/globe.rs); bump `GLOBE_FILE_VERSION`.
- Add `floristic_region_at_tile(globe, tx, ty)` and `plant_native_at(def, region, biome)` accessors for worldgen and scatter checks.
- Add `assets/data/resources/plants.ron` with plant-derived foods, seeds, fibers, medicines, dyes, resins, oils, bark, latex, and luxuries.
- Add appended techs only: `PLANT_LORE`, `HERBAL_MEDICINE`, `FIBER_PROCESSING`, `ORCHARD_CULTIVATION`, `OIL_PRESSING`; update `TECH_COUNT`, debug/tech UI names, and discovery skill mapping.

## Starter Species Set

Use this as the initial data target; each row has at least three biome-unique plants, with region filters per species.

| Biome | Starter Native Species | Economy Hooks |
|---|---|---|
| Tundra | cloudberry, crowberry, dwarf birch, arctic willow, Labrador tea | fruit, bark/twigs, medicine, tea/luxury |
| Taiga | paper birch, spruce, pine, cranberry, stinging nettle | wood, bark sheets, resin, fiber, fruit |
| Temperate | oak, hazel, crabapple, flax, hemp, grape, willow | acorns/nuts/fruit, fiber, medicine, wine/luxury |
| Grassland | wild barley, sunflower, prairie turnip, camas, amaranth | grain/oilseed/tubers, plantable seeds |
| Tropical | banana/plantain, taro, yam, cacao, rubber tree, oil palm | staple food, luxury, latex, oil |
| Desert | date palm, agave, yucca, prickly pear, mesquite, aloe | fruit, fiber, sugar, wood, medicine |
| Mountain | quinoa, potato, buckwheat, juniper, alpine willow | highland staples, wood, resin, medicine |
| Wetland | cattail, bulrush, papyrus, wild rice, lotus, willow | reeds, rhizomes, grain, fiber, paper/knowledge input |
| Steppe | foxtail millet, proso millet, sagebrush, wild onion, flax | drought grains, herbs, fiber, medicine |
| Badlands | saltbush, pinyon pine, prickly pear, juniper, yucca | nuts, fruit, fiber, fuelwood, medicine |
| Ocean/Coast | kelp wrack, eelgrass, sea palm, mangrove, coconut, saltwort | coastal food, fiber, wood, oil; spawn on passable shore tiles adjacent to ocean |

## Implementation Changes

- Generate floristic regions after hydrology/relief: flood-fill landmasses, split large landmasses by latitude/moisture, assign deterministic Earth-analog realm labels, and serialize the result on `Globe`.
- Rework [chunk_streaming.rs](/Users/xiao1/civgame/src/world/chunk_streaming.rs) plant spawn to filter by native region, biome, surface kind, fertility, relief, river distance, coast adjacency, and deterministic patch noise.
- Cache spawn candidate pools by `(flora_region, biome, surface_kind)` so chunk streaming does not scan the whole plant catalog per tile.
- Replace hardcoded `AnyEdible` and `wood` plant filters in gather, memory, HTN, vision, and retargeting with `PlantDef` predicates: edible yield, primary resource, use tags, and tool requirement.
- Report mature plant sightings as both `AnyEdible` when edible and `MemoryKind::Resource(primary_yield)` for specific resources like fiber, medicine, dye, resin, reeds, wood, or latex.
- Generalize farming seed choice: choose among available seeds by sowing season, crop demand, resource deficits, plot suitability, and native/climate fit; fallback to highest-stock compatible seed.
- Change `FieldTileState.last_crop` to `Option<PlantSpeciesId>` and make nutrient debit/tillage bonus species-driven.
- Keep strict wild nativity: `spawn_chunk_plants` and wild scatter reject non-native species outside native regions; cultivated plants can grow from carried seeds if climate-compatible, but escaped scatter outside plots remains disabled by default.
- Add recipes converting species resources into existing outputs: fiber resources to `cloth`, medicinal herbs to `medicine_bundle`, dyes/aromatics/cacao to `luxury`, oils to fuel/luxury inputs, reeds/papyrus/bark to construction or knowledge recipes.
- Extend the heal pipeline so `medicine_bundle` or strong medicinal plant resources improve healing rate and sickness decay, consumed at treatment start.
- Add procedural plant sprites by form and mature variant in [sprite_library.rs](/Users/xiao1/civgame/src/rendering/sprite_library.rs); UI hover shows display name, native realm, use tags, and harvest output.
- Update [AGENTS.md](/Users/xiao1/civgame/AGENTS.md), [src/simulation/CLAUDE.md](/Users/xiao1/civgame/src/simulation/CLAUDE.md), [src/world/CLAUDE.md](/Users/xiao1/civgame/src/world/CLAUDE.md), and [src/economy/CLAUDE.md](/Users/xiao1/civgame/src/economy/CLAUDE.md).

## Test Plan

- Catalog tests: every `PlantDef` references existing resources, valid biome names, valid seasons, and at least one native realm; every plantable species has a seed resource that round-trips to its species id.
- Coverage test: every `Biome` has at least three unique native starter species; Ocean uses coastal passable spawn rules.
- Flora generation tests: same seed produces identical `flora_regions`; changed seed can differ; no land cell has missing region data.
- Spawn tests: plants spawn only on allowed surfaces, skip reserved/doormat/road/agricultural tiles, respect region nativity, and are deterministic per chunk.
- Lifecycle tests: annuals die or go overripe correctly, perennials regrow, trees remain/fell correctly, cultivated seeds skip wild sprout failure.
- Gathering tests: `AnyEdible`, `Resource(id)`, wood, fiber, medicine, and dye targets find and harvest the right plant species with the right tool gates.
- Farming tests: seed selection honors season/demand/suitability, mixed seed stocks do not overcommit postings, non-native cultivation works only from acquired seed.
- Verification commands: `cargo test --bin civgame plant`, `cargo test --bin civgame farm`, `cargo test --bin civgame resource_catalog`, then `cargo check`.

## Assumptions And Sources

- No new crates; use existing `serde`/RON patterns and `AHashMap`.
- Procedural “strict regional” means generated floristic realms, not Earth coordinates.
- Native data should be validated during data entry against Kew Plants of the World Online, USDA ethnobotany references, Crop Trust crop lists, and FAO crop pages: [Kew POWO](https://powo.science.kew.org/), [USDA plant fibers](https://www.fs.usda.gov/wildflowers/ethnobotany/fibers.shtml), [Crop Wild Relatives crops](https://cwr.croptrust.org/crops/), [FAO quinoa](https://www.fao.org/quinoa/en).
