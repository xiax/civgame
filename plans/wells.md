# Add Wells To CivGame

## Context
Neolithic public-water structure: gated on a new `WELL_DIGGING` tech, integrated with the thirst pipeline so people prefer it over walking to a river, planned by `organic_settlement` when local water is weak, and seeded into Neolithic+ starts.

## Public types

**`technology.rs`** — `pub const WELL_DIGGING: TechId = 45`; `TECH_COUNT = 46`. `TechDef`: era `Neolithic`, prereqs `[PERM_SETTLEMENT, FLINT_KNAPPING]` (sedentism + tools — Neolithic wells predate organized irrigation), triggers `[StoneMining, Farming]`, bonus `ZERO`.

**`technology_adoption.rs`** — `tech_scale()` arm `WELL_DIGGING => AdoptionScale::Institutional`.

**`construction.rs`**
- `BuildSiteKind::Well` (1-tile, **impassable** — agents drink from chebyshev-adjacent), label "Well".
- `BuildRecipeIdx::Well`: 4 stone + 2 wood, 120 work_ticks, `tech_gate = Some(WELL_DIGGING)`, refund 2 stone + 1 wood.
- `Well { faction_id }` component, `WellMap(AHashMap<(i32, i32), Entity>)` resource (registered in `SimulationPlugin::build`).
- Add `well_map` field to `FurnitureMaps`, `BuildingMapsRO`, `GenCandidatesMaps` (mirror in both `as_view`), `OrganicStructureMaps`.
- Finalize as **bare public structure** mirroring `Latrine` (no `OwnedBy`/`WorkshopKind`): `Well + StructureLabel("Well") + Transform`, insert into `WellMap`.
- Deconstruct arm in the standard match; standard refund path; no `TileChangedEvent` (kind unchanged).
- Include `WellMap` in `already_built` overlap checks.

**`typed_task.rs`** — extend `DrinkSource` with `Well { tile: (i32, i32) }`. Required because `perform_drink` checks `is_drinkable_candidate()` on `TileKind`, and wells don't mutate the tile.

## Drink pipeline (`drink.rs`)

- `perform_drink` arm `DrinkSource::Well { tile }`: verify `WellMap` still has the entity (else `SourceGone`), verify chebyshev adjacency, decrement thirst, return `Drank { raw: false }`.
- `drink_task_system`: `Well` source reads `sanitation.is_contaminated(tile)` exactly like the `Tile` arm — wells without nearby `Latrine` get contaminated by `WastePile` within `CONTAMINATION_RADIUS=6`, which is realistic.
- Both functions take `Res<WellMap>`.
- `htn_drink_dispatch_system` priority: (1) inventory `clean_water` (unchanged); (2) **new** — nearest well within `DRINK_TILE_SCAN_RADIUS`, route to chebyshev-adjacent tile, dispatch `DrinkSource::Well`; (3) `nearest_fresh_drinkable_tile` fallback (unchanged).
- Animals/wild herds keep natural-water logic for v1.

## Organic settlement planning (`organic_settlement.rs`)

- Add `SettlementPressureKind::WaterAccess` (mirrors existing `SettlementAnchorKind::WaterAccess`).
- Emit in `collect_pressures` (alongside Granary), gated on:
  - `faction.community_has(WELL_DIGGING)`
  - `count_near(&maps.well_map.0, home, 30) + pending_of(BuildSiteKind::Well) < target`
  - target: 1 if `peak_pop < 40`, 2 if `< 90`, else 3
  - first well only — suppress when River/Bridge within 6 tiles of `home_tile` *and* zero existing wells
  - urgency: `190 + 60 * (1 - clamp(min_dist_to_fresh_water_or_well/10, 0, 1))`. Lands above Granary (170), below Hearth (~250).
- Pressure→intent arm: `WaterAccess => OrganicBuildKind::Single(BuildSiteKind::Well)`.
- Site selection (extends `pick_intent_tile`): walk Civic → Storage → Residential parcels; score `+ proximity_to_home`, `+ traffic_heat`, `− proximity_to_existing_well` (≤ 10 tiles), `− proximity_to_fresh_water_tile` (≤ 10 tiles, **per-target** so 2nd/3rd wells also score down near rivers); reject roads/doormats/occupied/foreign-plot tiles via `tile_buildable_by`.
- Extend `collect_anchors` to walk `WellMap` and emit `SettlementAnchorKind::WaterAccess` so future road/parcel planning orients around wells.

## Game-start seeding

`seed_starting_buildings_system` already drives the unified intent loop with `seed_techs = techs_through_era(GameStartOptions.era)` and bypasses civic-milestone gates. Founders' `seeded_realistic_through_era` Aware bits include `WELL_DIGGING` automatically for Neo+. Per-era target: Paleo/Meso/nomadic 0; Neo 1; Chalco up to 2; Bronze up to 3. Add well counting + a well candidate to `generate_candidates`.

## Player UI / right-click menu (`ui/orders.rs`)

`MenuAction::label()` arm: `BuildSiteKind::Well => "Build Well"`. Existing `build_options` push handles tech-locked display.

## Rendering (`rendering/entity_sprites.rs`)

Sprite key `building_well` already exists in `sprite_library.rs:499` (registered at line 5243). Mirror `spawn_workbench_sprites`: add `WellVisual` marker + `spawn_well_sprites` system on `Added<Well>`, attaches a child `Sprite` from the existing key. Register in `RenderingPlugin::build`. Update `spawn_blueprint_sprites` to recognize `BuildSiteKind::Well`.

## Free-via-existing-systems
- `StructureIndex` auto-maintained by `StructureLabel` observers.
- `ConstructionObstacle` clearing handled by `populate_pending_clear_system` for the 1-tile footprint.
- Plot ownership filtered by `tile_buildable_by` in the chief candidate pipeline.

## Docs
- `src/simulation/CLAUDE.md` — one-sentence updates under "Thirst pipeline" (new `DrinkSource::Well` variant, dispatcher priority) and "Construction" (well in maps + recipe table).
- `AGENTS.md` — brief mention.

## Out of scope (v1)
Bucket / `clean_water` production at wells; animal use; well-quality variant ladder; effect on `SanitationMap` decay.

## Verification
1. `cargo check`
2. `cargo test --bin civgame`:
   - Tech def: era `Neolithic`, prereqs sorted `[PERM_SETTLEMENT, FLINT_KNAPPING]`, scale `Institutional`.
   - Recipe: 4 stone + 2 wood, 120 ticks, gate `Some(WELL_DIGGING)`, refund 2 stone + 1 wood.
   - `faction_can_build(Well, &techs_without_well) == false`.
   - `perform_drink` Well arm: success + thirst drop; `SourceGone` when removed from `WellMap`; `raw: false`; sickness rolled when `SanitationMap` marks the tile.
   - Pressure emission: Neo + WELL_DIGGING Adopted, no rivers, peak_pop=20 → exactly one `Single(Well)` intent; suppresses with river within 6.
   - OnEnter seed (mirror `onenter_era_seeding`): Neo start stamps one well into `WellMap` on a Civic/Storage/Residential parcel; Paleo/Meso/nomadic stamp none.
3. `cargo run` (Neolithic start, no `--sandbox` per memory): build well from right-click; thirsty person prefers well over river; deconstruct → fallback to river clean; place `WastePile` within 6 tiles → subsequent drinks roll `Sickness`.
