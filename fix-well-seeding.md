# Fix Seeded Well Footprints

## Summary
- Make wells use their real 5x5 stepwell footprint everywhere placement/seeding checks space.
- Update seeded wells to use the aquifer-derived `WellSpec` instead of the old fixed `surf - 3` shortcut.
- Prevent wells from overlapping roads, planned road corridors, doormats, houses, and other well footprints.

## Implementation Changes
- Add canonical well placement helpers around `well::well_footprint`, including existing-well overlap checks and full-footprint clearance checks.
- Special-case `BuildSiteKind::Well` in organic placement and construction candidate footprints so `Single(Well)` is treated as 5x5, not 1x1.
- Special-case seed-time well stamping:
  - search for a valid full 5x5 site near the selected anchor;
  - require `well_spec_at(&globe, &chunk_map, center) == Ok`;
  - reserve all 25 footprint tiles in `used`;
  - stamp the finished `Well` using the resolved `surf_z` / `bottom_z`;
  - charge runtime water with `WELL_INITIAL_CHARGE_Z` + `AQUIFER_SEEP_RATE`;
  - stamp matching outer lining `Wall`s except the existing north gateway, using `best_wall_material(seed_techs)`.
- Harden runtime/manual well conversion so a `BuildSiteKind::Well` blueprint is rejected if any footprint tile crosses roads, doormats, structures, blueprints, or another well footprint.
- Update seed reservation to reserve every seeded well’s full footprint, not only the center tile.
- Make `carve_seeded_wells_system` run after the synchronous seed road-carve pass so roads and well geometry are applied in a deterministic order.
- Update stale docs/comments saying wells are 1-tile or seed with fixed depth.

## Test Plan
- Add unit coverage that well footprint overlap uses the full 5x5 area, including overlap at center distance 3-4.
- Add organic-placement tests that `Single(Well)` rejects any footprint intersection with `road_tiles` / `road_corridor_tiles`.
- Extend Neolithic OnEnter fixture tests:
  - at least one well still seeds;
  - every seeded well footprint is reserved in `SeedReservation`;
  - no seeded well footprint intersects `TileKind::Road` or a doormat;
  - seeded `Well` depth matches `well_spec_at` from pre-carve terrain instead of fixed depth 3.
- Run `cargo test --bin civgame well` and targeted seed fixture tests, then `cargo test --bin civgame`.

## Assumptions
- Keep the current north gateway for now, matching `gateway_tile(center)`.
- Do not change drinking/pathfinding semantics; this fix is placement, seeding, and reservation only.
- No new crates.
