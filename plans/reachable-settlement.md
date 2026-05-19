# Reachable Settlement Placement Plan

## Summary
Add a shared placement-reachability layer so settlement planning, seed-time construction, farms, and initial faction worker spawning only choose tiles that are actually walkable from the faction’s connected area. Use live `ChunkConnectivity::tile_reachable` for current-world checks, and a small simulated-build path check for houses before walls/doors/beds are emitted or stamped.

## Key Changes
- Add `src/simulation/placement_reachability.rs` with helpers for:
  - current tile reachability from faction `home_tile`
  - reachable interactable tiles for single furniture
  - reachable farm rectangles / yards
  - simulated walled-house reachability after full build
- Wire current-world checks into:
  - `organic_settlement::choose_site_for_intent`
  - `construction::generate_candidates`
  - final `chief_directive_system` pre-spawn gate to catch stale organic intents
- Treat “reachable furniture” as all non-wall structures agents interact with: beds, campfires, workshops, granary, shrine, market, barracks, monument, table/chair, latrine, well access, and bridge/work-stand access.
- For houses, simulate the final footprint before accepting:
  - perimeter walls block
  - door tile stays passable
  - doormat is passable and must connect to home
  - every interior bed tile must be reachable from the doormat/door path
  - target-z / doormat-z transitions must satisfy normal path step rules
- Apply the same house simulation in `plan_building`, `plan_composite_building`, and `seed_walled_house_at`.

## Farms And Workers
- Farm placement must reject unreachable plots:
  - organic Agricultural parcels
  - `seed_starting_farms_system` 16x16 startup plots
  - `seed_farmstead_yard` attached yards
- Farm job dispatch should prefer reachable unplanted tiles inside the assigned plot, so old or partially bad plots do not create unreachable planting tasks.
- Update `spawn_population` so every spawned faction member is placed on a tile reachable from that faction’s `home_tile`.
- Update Market-preset `seed_market_households` so household storage/home tiles are also reachable from the village home and the member’s spawn area.

## Tests
- Add unit tests for simulated house reachability:
  - accepted house with reachable interior bed
  - rejected house whose completed walls seal the bed
  - rejected door/doormat with invalid z step
- Add farm tests:
  - startup farm rejects a river/wall-isolated rectangle
  - farm tile picker skips unreachable plantable tiles
- Add spawn tests:
  - faction workers never spawn across an impassable barrier from home
  - Market household seed storage is reachable
- Run `cargo test --bin civgame`.

## Assumptions
- “Spawned workers” means all initial `Person` entities created by `spawn_population`, plus their Market-preset household storage/home tiles.
- This does not change save data or add crates.
- Update simulation docs after implementation to record the new reachability guarantees.
