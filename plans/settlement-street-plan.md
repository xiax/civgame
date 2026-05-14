**Detailed Settlement Street Circulation Plan**

**Summary**
Make streets a first-class constraint for both normal settlement growth and large-population starting settlements. Dense settlements will get a connected block-scale road graph before housing is allocated, and walled houses will only be placed from road-fronted lots.

**Organic Road Network**
- Update `src/simulation/organic_settlement.rs::build_road_network` to create phase-scaled circulation:
  - Hamlet: keep the current primary east-west spine, adding north-south only for larger hamlets.
  - Village: add a connected 3x3 cross-grid around the home tile.
  - Chiefdom: add secondary streets at roughly 8-tile intervals within `road_network_radius`.
  - ProtoUrban/Urban: use tighter 6-tile spacing, producing multiple through-streets in both axes.
- Keep existing anchor/desire-path segments, but append them after the block grid so water, fields, gates, markets, and high-traffic paths connect into the base graph.
- Remove the current Chiefdom-only two-branch special case once the general grid offsets cover it.
- Keep road generation deterministic from phase, home tile, culture seed, and anchors; no random layout decisions.

**Parcel Allocation**
- Replace the current “frontier point near road, then hope the parcel has frontage” flow for permanent settlements with explicit road-frontage parcel candidates:
  - Iterate planned road tiles.
  - For each cardinal side, derive a parcel rect whose edge sits adjacent to the road tile.
  - Generate candidates per needed district kind using `parcel_size`.
  - Score by existing `ParcelSuitability`, district target deficit, distance to home, and deterministic tie-breaks.
  - Greedily accept candidates that do not overlap accepted parcels or `brain.road_tiles`.
- Keep the existing frontier-based fallback only for camps / non-permanent settlements.
- Preserve the invariant that permanent-settlement residential parcels must have `frontage_edge` and `access_tile`.
- Keep `rect_clear_for_parcel` rejecting planned roads, carved roads, impassable terrain, water, and walls.

**Compatibility Projection And Legacy Fallback**
- Change `compat_plan_from_brain` so parcel-backed zones are emitted as individual zones instead of unioning all parcels of one kind into one large rectangle.
- Only add broad district/fallback zones for a kind when the organic planner produced no parcels for that kind.
- In `chief_directive_system`, prevent legacy `generate_candidates` from placing `Hut`, `Longhouse`, or `CompositeHouse` for permanent settlements once a full-settlement record exists.
- If the organic brain has not surveyed yet, skip permanent shelter fallback for that tick instead of placing broad-zone housing before roads exist.
- Leave non-shelter fallback candidates available, while still rejecting anything touching `SettlementBrain.road_tiles`.

**Game-Start Seeding**
- In `seed_starting_buildings_system`, stamp the starting plan’s `StreetSpine` into the map before seeding Neolithic+ houses.
- Track a local `protected_roads: AHashSet<(i32, i32)>` from the plan spine; use it even when a tile could not be physically written as `TileKind::Road`.
- Update seeded placement helpers:
  - `pick_seed_tile` rejects `TileKind::Road` and `protected_roads`.
  - `pick_seed_house_anchor` scans all Residential zones, not only the first.
  - `pick_seed_house_anchor` prefers and requires road-fronted house sites for permanent starts, returning both centre tile and `door_dir`.
  - `seed_walled_house_at` receives that `door_dir`; seeded frontage sites should only be selected when the preferred doormat is clear and road-connected.
  - `seed_farmstead_yard` rejects carved/planned roads.
- Change seeded door road extension behavior to match grown houses: write the doormat road, but only queue a doormat-to-home extension if `road_within(..., 4)` is false.

**Tests**
- Add organic planner tests in `organic_settlement.rs`:
  - Village+ road generation includes multiple connected through-streets.
  - ProtoUrban/Urban road spacing leaves no oversized residential block between adjacent streets.
  - Permanent-settlement parcels all have frontage and no parcel rect overlaps `brain.road_tiles`.
  - `compat_plan_from_brain` preserves parcel-shaped residential zones instead of one giant union rectangle.
- Add construction/seeding tests in `construction.rs`:
  - Seeded house selection uses a later Residential zone when the first is blocked.
  - Seeded houses return/use the frontage `TileEdge`.
  - Seeded structures and yards reject roads/protected roads.
  - A large Chalcolithic/Bronze start leaves the stamped spine clear of house footprints.
- Run:
  - `cargo test --bin civgame organic_settlement`
  - targeted construction helper tests
  - `cargo test --bin civgame` if the targeted suite is green.

**Docs And Acceptance**
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` to state that dense settlements use phase-scaled block streets and game-start seeding protects roads before placing houses.
- Acceptance criteria:
  - Large settlements should show continuous streets through residential growth areas.
  - Walled houses may form blocks, but not settlement-scale barriers.
  - Doors face frontage roads when a road-fronted lot is selected.
  - No residential footprint, yard, civic seed, or fallback build lands on planned/carved roads.
