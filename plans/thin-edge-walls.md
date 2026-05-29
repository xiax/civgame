# Thin Edge Walls For Housing

## Summary
Refactor permanent dwelling walls so huts, longhouses, and composite houses use edge-based wall and door segments instead of whole blocked tiles. Housing floor tiles remain passable and can host beds/hearths/furniture; movement and LOS are blocked only when crossing a wall-bearing edge.

Natural rock, mining walls, defensive palisades, and standalone manual walls stay full-tile `TileKind::Wall` in this pass.

## Key Changes
- Add an edge barrier model:
  - Introduce canonical `EdgeKey { axis, x, y }` for the boundary between two adjacent tiles.
  - Add edge-wall, edge-door, and dwelling-envelope maps/components.
  - Keep `WallMap` for full-tile walls; use the new edge maps only for housing.

- Change housing construction:
  - Add housing wall/door blueprint metadata carrying the target `EdgeKey`.
  - Replace the rectangular house wall-tile planner with an edge-envelope planner.
  - Keep current hut/longhouse footprints and bed counts, but use the full footprint as usable floor.
  - Deconstruction of housing walls targets edge structures, not the floor tile.

- Update movement, pathfinding, and LOS:
  - `passable_step_3d`, diagonal checks, A*, flow fields, and chunk graph classification reject blocked edge crossings.
  - Diagonal movement cannot slip through two blocked wall edges at a corner.
  - Closed edge doors block LOS like current doors; open doors are transparent.
  - Faction vision keeps the existing own-wall transparency behavior.

- Preserve placement safety:
  - Roads, doormats, plants, wells, and unrelated structures cannot occupy dwelling floor envelopes.
  - Furniture may occupy dwelling floor tiles.
  - Kitchen-garden detection uses dwelling envelopes instead of old wall-tile rings.

- Render edge walls correctly, including tilted view:
  - Add `EdgeWallVisual` and `EdgeDoorVisual` with orientation-aware sprites.
  - Spawn edge walls at the logical midpoint of their edge, with `ProjectedAnchor::Static { z: dwelling_floor_z }`.
  - Use existing projection y-sort: north/back walls sort behind interior furniture, south/front walls sort in front, and east/west wall segments sort by their edge midpoint.
  - Swap or size sprites by `MapViewMode`: thin line strips in top-down, low upright wall/door strips in tilted view.
  - Blueprint ghosts for housing walls/doors render on the edge, not centered in the tile.

- Update docs:
  - Update root `AGENTS.md` plus simulation/pathfinding/rendering local notes for the new housing-wall contract.

## Test Plan
- Unit-test `EdgeKey` canonicalization and both adjacent-tile lookups.
- Verify dwelling floor tiles stay passable and are not `TileKind::Wall`.
- Verify crossing an edge wall fails, while moving within the same floor tile succeeds.
- Verify diagonal corner-cutting fails around edge walls.
- Verify A*/chunk graph route around edge barriers and through door openings.
- Verify LOS blocks on edge walls and closed edge doors.
- Verify seeded huts/longhouses produce edge envelopes, edge walls, one edge door, and usable interior furniture tiles.
- Verify top-down and tilted rendering:
  - walls align to tile edges,
  - no wall sprite appears centered in the room,
  - front/south tilted walls draw in front of interior furniture,
  - back/north tilted walls draw behind interior furniture,
  - doors align with doormats/roads.
- Run `cargo test --bin civgame` and `cargo check`; visually smoke-test `cargo run -- --sandbox` in both view modes.

## Assumptions
- Scope is permanent housing: huts, longhouses, and composite houses.
- Defensive palisades remain full-tile blockers for now.
- Manual standalone wall placement remains full-tile unless a later pass adds an explicit “house wall edge” build tool.
- No new crates are needed.
