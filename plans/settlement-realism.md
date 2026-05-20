# Settlement Realism Fix Plan

## Summary
Keep seeded starts as already-in-progress societies, with productive farms and yards from tick 0. Improve realism by making roads, yards, fields, civic seeding, and fortifications read as lived-in settlement growth rather than perfectly stamped geometry.

## Key Changes
- **Door paths connect to streets, not the faction base**
  - Replace unconditional `doormat -> home` road extensions with `doormat -> nearest settlement road`.
  - Always stamp the doormat tile as `Road`.
  - Search for nearest carved `TileKind::Road` or planned `SettlementBrain.road_tiles` within a bounded radius, default `12`.
  - Prefer same-faction planned/carved road network; ignore roads blocked by walls, water, blueprints, beds, reserved doormats, or farm-protected tiles.
  - If a nearby road exists, queue a short connector from the doormat to that road.
  - If no road exists, runtime frontier builds may fall back to `doormat -> home`; seed mode should connect to the nearest planned spine endpoint or skip the long connector rather than creating a radial wagon-wheel.
  - Implement with a shared helper used by both runtime door finalization and `seed_walled_house_at`, so seeded and constructed houses obey the same rule.

- **Era start maturity**
  - Add `StartSettlementMaturity` to `GameStartOptions`: `Founder`, `Established`, `Developed`.
  - Default to `Established`.
  - `Founder`: minimal structures, obey civic milestones.
  - `Established`: current intended “society in progress” baseline: housing, productive fields/yards, water, storage, craft.
  - `Developed`: showcase start, allowing milestone-bypassed Market/Barracks/Monument for Bronze-style starts.
  - Route seeded civic decisions through `should_seed_growth_civic(kind, era, peak_pop, maturity)`.

- **Larger, more believable yards**
  - Treat yards as kitchen gardens/work yards, not main farms.
  - Replace fixed `2x2`/`3x3` yard dimensions with era- and dwelling-aware sizes.
  - Suggested tile scale: 1 tile is about 1.5m.
  - Hut yards: Neolithic `3x4`, Chalcolithic `4x4`, Bronze `4x5`.
  - Longhouse yards: Neolithic `4x5`, Chalcolithic `5x5`, Bronze `5x6`.
  - Keep yards immediately useful, but vary tiles deterministically: productive Cropland, fallow/work soil, and occasional empty edge tiles.
  - Continue rejecting overlaps with walls, roads, doors, doormats, water, and unreachable areas.

- **Productive but less perfect fields**
  - Keep startup belt farms productive and seeded.
  - Vary visual/tile state inside seeded 16x16 agricultural plots while preserving full plot protection and farm usability.
  - Use deterministic plot/faction seed variation for planted rows, fallow rows, and harvested-looking sections.
  - Keep all agricultural plot tiles in `PlotIndex.ag_tiles` so road carving still avoids them, even if some visual tiles are not `Cropland`.

- **Less geometric settlement roads**
  - Keep phase-scaled road skeletons, but make early roads feel like paths that became roads.
  - Hamlet: one main spine, traced toward water/material/field anchors when available.
  - Village: add cross streets when traffic heat or anchor demand justifies them, not purely from population.
  - Chiefdom and later: keep planned secondary streets, but jitter endpoints and prefer desire-path/least-cost connectors where possible.
  - Preserve river avoidance before bridge tech and farm protection.

- **More grounded fortifications**
  - Replace bed-bounding-box palisade siting with a defensible-core envelope.
  - Envelope includes houses, storage/granary, civic hearth/core, wells, gates, and primary roads.
  - Score wall sites by terrain, road gateways, doormat clearance, existing walls, and raid/threat direction.
  - Preserve 3-tile gateways on road axes.
  - Do not wall fields by default except for `Developed` starts or highly defensive cultures.

## Implementation Notes
- Add a shared door-road helper near construction road carving:
  - `find_door_connector_target(chunk_map, brain, doormat, home, radius) -> Option<(i32, i32)>`
  - `queue_door_connector_or_fallback(queue, faction_id, doormat, target_or_home, mode)`
  - Use carved roads first, planned roads second, home fallback last.
- Thread `Option<&SettlementBrain>` into seed-time house stamping and runtime door finalization via existing `SettlementMap`/`SettlementBrains`.
- Use a simple clear dogleg or short path for door connectors if available; otherwise keep Bresenham as fallback.
- Add `yard_dimensions(era, intent)` and `seed_yard_tile_role(layout_seed, tile)` helpers.
- No new crates.

## Tests
- Seeded doors connect to nearest planned/carved road, not always home.
- Runtime doors use the same connector logic as seeded doors.
- No dense radial road web appears on a 20- or 60-pop Neolithic/Bronze start.
- `Founder`, `Established`, and `Developed` maturity profiles seed the expected civic structures.
- Hut and Longhouse yards use era-specific dimensions and remain reachable.
- Yard/field variation is deterministic for the same faction and settlement seed.
- Agricultural plot tiles remain protected from road carving even when visually varied.
- Fortification envelope includes homes plus storage/civic anchors and preserves gateways.
- Existing `organic_settlement` and `onenter_era_seeding` tests remain green.

## Assumptions
- Seeded settlements represent societies already in progress, so productive farms and yards are correct.
- `2x2` yards are too small for household yards when a house footprint is already `3x3`; they are only plausible as tiny garden beds.
- `Established` should be the default start maturity.
- Door paths should accrete onto the nearest lane/street, with home fallback only for isolated frontier construction.
