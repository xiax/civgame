# Add Wells To CivGame

## Summary
Add wells as a Neolithic public-water structure: buildable after `Well Digging`, usable by people as a clean direct drinking source, planned by organic settlements when local water access is weak, and seeded into eligible game-start settlements.

## Public Interfaces And Types
- Add `WELL_DIGGING: TechId = 45`, bump `TECH_COUNT` to `46`, and define it as a Neolithic tech in [technology.rs](/Users/xiao1/civgame/src/simulation/technology.rs).
  - Direct prerequisites: `IRRIGATION` + `FLINT_KNAPPING`.
  - Transitive precursors: `CROP_CULTIVATION`, `FIRED_POTTERY`, `PERM_SETTLEMENT`.
  - Triggers: `StoneMining` and `Farming`; bonus: `TechBonus::ZERO`.
  - Classify as `AdoptionScale::Institutional` in `technology_adoption.rs`.
- Add `BuildSiteKind::Well`, `Well { faction_id }`, and `WellMap(AHashMap<(i32, i32), Entity>)` in [construction.rs](/Users/xiao1/civgame/src/simulation/construction.rs).
  - Recipe: `4 stone + 2 wood`, `120` work ticks, gated on `WELL_DIGGING`, refund `2 stone + 1 wood`.
  - Single-tile, passable structure; does not mutate `TileKind`.
- Add `DrinkSource::Well { tile }` in `typed_task.rs`.
  - Wells are clean by default (`raw = false`) but still check `SanitationMap` at the well tile for contamination sickness.
  - Wells do not create inventory `clean_water` in v1.

## Implementation Changes
- Register wells everywhere construction structures are tracked:
  - Add `WellMap` to `SimulationPlugin`, `FurnitureMaps`, `BuildingMapsRO`, `GenCandidatesMaps`, `OrganicStructureMaps`, UI routing resources, deconstruction, seed stamping, and overlap checks.
  - Finalizing a `BuildSiteKind::Well` blueprint spawns `Well`, `StructureLabel("Well")`, `OwnedBy` only if a new `WorkshopKind` is not needed; otherwise keep it as a plain public structure.
  - Deconstruction removes from `WellMap`, despawns the entity, emits `TileChangedEvent`, and refunds the recipe.
- Update player/UI/rendering:
  - Add “Build Well” to the right-click build menu and locked/unlocked tech gating.
  - Include wells in `already_built` checks so players cannot stack structures.
  - Render wells using the existing `SpriteLibrary` key `"building_well"`; add `WellVisual` + `spawn_well_sprites`, and use that sprite for well blueprints.
- Update the thirst pipeline in [drink.rs](/Users/xiao1/civgame/src/simulation/drink.rs):
  - Dispatcher priority stays: inventory `clean_water` first, then nearest well within the existing thirst scan radius, then natural fresh-water tiles.
  - `perform_drink` verifies the well still exists in `WellMap`; missing/deconstructed wells return `SourceGone`.
  - Keep animals and wild herds on natural-water logic for v1 so wells do not lure wildlife into settlements.
- Update settlement planning in [organic_settlement.rs](/Users/xiao1/civgame/src/simulation/organic_settlement.rs):
  - Add `SettlementPressureKind::Water`.
  - Emit water pressure when `WELL_DIGGING` is adopted and built+pending wells are below target.
  - Target wells: `1` for permanent settlements, `2` at `peak_population >= 40`, `3` at `peak_population >= 90`.
  - Suppress the first well if a fresh `River`/`Bridge` is within 6 tiles of home; otherwise urgency is high enough to outrank craft/storage but not emergency hearth/shelter.
  - Choose sites from Civic, Storage, then Residential parcels; score near home, near traffic, away from existing wells, and never on planned roads/doormats/occupied structures.
  - Treat built wells as `WaterAccess` anchors so later roads/parcels can orient around them.
- Update legacy/shared candidate generation and game-start seeding:
  - Add well counting and a well candidate to `generate_candidates` so both runtime fallback and `seed_starting_buildings_system` can use it.
  - Neolithic+ starts with `WELL_DIGGING` seed one well through the unified intent loop; Chalcolithic/Bronze starts can seed additional wells by population target.
  - Paleolithic/Mesolithic and nomadic camp seeding do not seed wells.
- Update docs:
  - Root [AGENTS.md](/Users/xiao1/civgame/AGENTS.md) settlement/thirst notes.
  - [src/simulation/CLAUDE.md](/Users/xiao1/civgame/src/simulation/CLAUDE.md) for thirst, construction, and tech-adoption behavior.

## Test Plan
- Unit tests:
  - `WELL_DIGGING` is Neolithic, gated by `IRRIGATION + FLINT_KNAPPING`, included by Neolithic `seeded_through_era`, and classified as `Institutional`.
  - `BuildSiteKind::Well` has the expected recipe, label, gate, refund, and cannot be built before adoption.
  - `DrinkSource::Well` reduces thirst, is non-raw, fails if the well is gone, and applies contamination sickness when `SanitationMap` marks the well tile.
  - Settlement pressure emits a `Single(Well)` intent only when tech/adoption and water-target conditions are met.
  - Seed-mode Neolithic+ settlements stamp wells into `WellMap`; Paleo/Meso and nomadic starts do not.
- Integration/checks:
  - `cargo check`
  - `cargo test --bin civgame`
  - Manual sandbox smoke test: build a well from the right-click menu, watch a thirsty person prefer it over walking to a river, deconstruct it, and confirm later drink dispatch falls back cleanly.

## Assumptions
- Wells are public settlement infrastructure, not a new `TileKind`.
- Wells provide direct drinking only; bucket filling or `clean_water` production can be a later feature.
- Human settlement water access is the v1 focus; animal/herd/nomad migration scoring remains natural-water based.
