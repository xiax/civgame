# Faction Center De-Crowding Plan

## Summary
Fix this as a planner-first layout issue. The current system over-prefers near-home road parcels and some consumers pick the first/closest matching zone, so faction centers get packed before outer frontage lots are used. The fix will reserve a small civic commons, protect wider road corridors, and make parcel/build placement distribute across the settlement instead of repeatedly choosing the nearest center tile.

## Key Changes
- In `src/simulation/organic_settlement.rs`, add a “commons” keepout around `FactionData.home_tile`: 5x5 for Hamlet/Village, 7x7 for Chiefdom+. No residential, crafting, storage, market, sacred, defense, or house footprint may overlap it.
- Add a planned road corridor helper or `SettlementBrain.road_corridor_tiles`: keep `road_tiles` as the road centerline/frontage network, but reject parcel/building footprints that overlap the widened corridor matching `road_carve_system`.
- Retune road radius to give villages more frontage: Hamlet `10`, Village `16`, Chiefdom `22`, ProtoUrban `30`, Urban `36`.
- Change road-driven parcel scoring from “closest to home wins” to ideal distance bands:
  - Civic/storage near but outside commons.
  - Residential just beyond commons and along road frontage.
  - Craft/market/sacred farther along spurs.
  - Defense outermost.
- Enforce a 1-tile parcel buffer between non-agricultural parcels, except road-facing frontage remains adjacent to the road.
- Update `choose_site_for_intent`, `find_footprint_at_frontage_lot`, `find_footprint_in_zone`, and `find_clear_tile_in_zone` so they score all matching parcels/zones instead of using the first/closest one.
- Keep this planner-only: no movement/collision rewrite, no new crates, no forced demolition or relocation of existing structures.

## Tests
- Add organic-settlement unit tests proving road-driven parcels do not overlap the commons or planned road corridors.
- Add tests that residential/crafting/storage parcels spread across multiple road-distance bands instead of clustering at the first center-adjacent tiles.
- Add construction tests showing frontage-lot and single-tile placement scan all matching zones and skip blocked/too-central lots.
- Add a seeded Neolithic/Bronze regression test asserting the home/storage core has open cardinal access and no house/workshop/granary footprint inside the commons.
- Run `cargo test --bin civgame organic_settlement` and the relevant construction/test-fixture filters.

## Assumptions
- Existing crowded settlements will improve on the next replan only where future construction occurs; this plan does not move or demolish already-built structures.
- Compact cultures can still be compact, but the commons, road corridors, and 1-tile parcel buffer are hard minimums.
- Update the settlement-construction notes in `AGENTS.md`; also update `src/simulation/CLAUDE.md` if the implementation changes documented settlement/farming behavior.
