# Guarantee Door-To-Road Connectivity

## Summary
Fix roads as a real connectivity problem, not just a drawing issue: every household door doormat should have a cardinal, visually continuous road path into the settlement road graph. Slightly more paving around houses is acceptable.

## Key Changes
- In [construction.rs](/Users/xiao1/civgame/src/simulation/construction.rs), change `find_door_connector_target` so diagonal-adjacent roads no longer count as “already connected.” Only a cardinal-adjacent road connected to the main road graph can return `DoorConnectorTarget::None`.
- Extend `RoadCarveQueue` from raw `(faction, from, to)` triples to a small `RoadCarveJob` enum:
  - `Segment` keeps current 2-wide Bresenham carving for spines and desire paths.
  - `Connector` uses bounded cardinal A* from door doormat to target road/home fallback, avoiding structures, blueprints, beds, wells, farms, water, walls, and stone.
- Use `SettlementBrain.road_corridor_tiles` for planned-road targeting and `RoadField` seeding, so diagonal planned spines are treated like the actual 2-wide physical corridor instead of a centerline-only sketch.
- Add connector preflight to residential placement: `choose_residential_site` should reject a hut/longhouse entrance option if its doormat cannot path-carve into the connected road graph. Try another doorway/axis before rejecting the parcel.
- Update queued-road reservation helpers in [seed_reservation.rs](/Users/xiao1/civgame/src/simulation/seed_reservation.rs) and organic placement guards so connector paths are reserved by the same logic that will carve them.

## Tests
- Unit-test that a diagonal-only road beside a doormat requires a connector, while a cardinal connected road does not.
- Unit-test connector carving around a blocked tile produces a continuous cardinal road path with no skipped gaps.
- Integration-test a Neolithic/Bronze seeded settlement: every `Door.doormat_tile` is `TileKind::Road` and road-connected to that faction’s settlement road graph.
- Run `cargo test --bin civgame door_connector`, targeted road/seed tests, then `cargo check`.

## Assumptions
- “Full connectivity” means visual/cardinal road continuity, not merely 8-way movement reachability.
- Existing dirty worktree currently blocks tests with unrelated `cancel_bedless_sleep` compile errors in `construction.rs`; implementation should preserve that work and verify after that compile issue is resolved.
- Update [src/simulation/CLAUDE.md](/Users/xiao1/civgame/src/simulation/CLAUDE.md) and [AGENTS.md](/Users/xiao1/civgame/AGENTS.md) to document the stricter door-connector contract.
