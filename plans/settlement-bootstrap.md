# Organic Settlement Bootstrap Plan

## Summary
Build a startup-only `SettlementBootstrapPlan` as an atomic application layer over the existing organic settlement planner. It will not replace `SettlementBrain`, parcels, districts, road skeletons, or future organic planning work; it will consume the organic layout snapshot and choose the correct day-0 subset for the selected era/lifestyle.

The main architectural change is to stop treating startup as “run runtime construction growth repeatedly.” Startup should be a deterministic bootstrap transaction: reserve land, assign households, pick dwellings, place minimal roads/farms/civics, clear conflicts, then spawn people onto valid tiles.

## Key Changes
- Factor the organic survey output into a reusable layout snapshot:
  - Use the same survey logic that currently feeds `SettlementBrain`.
  - Startup reads `SettlementBrain` parcels, road segments, districts, anchors, frontage, and farm belts.
  - Runtime growth continues to use pressures and project selection; startup uses the same layout geometry but a different selection policy.
- Add `SettlementBootstrapPlan` for startup:
  - Inputs: faction, era, lifestyle, population, organic layout snapshot, terrain, existing structure maps.
  - Outputs: structures, roads to carve now, roads to reserve for future growth, farm plots/yards, founder spawn slots, household assignments, plant/obstacle cleanup tiles.
  - Validate the full plan before stamping anything: no people inside walls, no structures on roads, no roads through protected farms, no unreachable doors/beds/spawn slots.
- Split population startup:
  - First create faction shells and founder specs.
  - Run organic survey plus bootstrap plan.
  - Spawn founders onto bootstrap-assigned safe tiles after structures/roads are known.
- Replace Neolithic+ startup’s repeated `generate_candidates` loop:
  - Runtime `generate_candidates` remains for chief construction.
  - Startup uses era profiles over the organic layout snapshot.
  - Neolithic settled default: one civic hearth, household dwellings, attached yards/farms, storage, and minimal access roads.
  - Extra hearths only appear when represented by an actual starter-household/home template, not from population pressure math.
- Improve starter homes:
  - Replace default 3x3 one-bed huts as the main starter dwelling.
  - Add starter templates such as family house and longhouse with meaningful interiors, multiple beds, door/frontage, and optional yard.
  - Keep tiny huts only as fallback/emergency shelter, not the primary Neolithic identity.
- Centralize startup terrain mutation:
  - Roads, doormats, farms, and structure stamping must share one safe write path.
  - Any plant or obstacle on a stamped road/structure tile is removed or relocated immediately, including chunk-streamed plants that appear after startup.
  - `PlantMap` must never retain an entry on a road, wall, door, doormat, bed, or seeded structure tile.
- Seed initial social state:
  - Create deterministic founder households or kin groups.
  - Seed reciprocal `RelationshipMemory` affinities for household members and close faction peers.
  - Seed `HouseholdMember` where compatible with the economy/lifestyle model.
  - Preserve Market preset’s private-household behavior, but avoid one-person “everyone is alone” social startup unless the preset explicitly needs that as an economic shell.

## Interfaces / Types
- Add `SettlementBootstrapPlan` with sections for dwellings, civic structures, roads-to-carve, roads-to-reserve, farms/yards, spawn slots, households, and cleanup tiles.
- Add `SeedPlacementContext` as the shared validator/reservation surface for footprints, doormats, roads, farms, plants, beds, and spawn tiles.
- Add `StarterHouseholdSpec` and `StarterDwellingSpec` to bind founders, beds, house footprint, frontage, and social seeds.
- Keep `SettlementBrain` authoritative for organic layout. Future changes to organic parcels, districts, roads, and archetypes should affect startup automatically through the layout snapshot.

## Test Plan
- Run `cargo test --bin civgame`.
- Add OnEnter startup tests for Neolithic and Bronze starts:
  - No founder spawns on walls, doors, beds, roads, structures, impassable tiles, or reserved future footprints.
  - No `PlantMap` entry remains on roads, doormats, walls, doors, beds, or seeded structures after startup and after chunk streaming.
  - Neolithic settled starts create one civic hearth by default and do not emit extra loose campfires around the base.
  - Early roads are minimal carved access roads; future spines may be reserved but not prematurely carved.
  - Starter dwellings meet minimum interior/capacity rules and are not dominated by one-cell rooms.
  - Every founder has at least one reciprocal initial relationship.
  - Bronze still receives era-appropriate civic seed buildings through the bootstrap plan.

## Assumptions
- The organic planner remains the source of truth for settlement geometry.
- Startup selection and runtime growth selection are intentionally different consumers of the same layout surface.
- Determinism is required for the same seed, terrain, faction culture, era, lifestyle, and population.
- No new crates are needed.
