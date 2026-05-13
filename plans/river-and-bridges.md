# River-Aware Camps And Bridges

## Summary
Implement river-aware settlement planning for early eras, then add player-buildable and AI-planned bridges for later settlements. Use a new explicit `Bridge Building` technology, as requested, and make bridges real pathfinding tiles, not decorative road paint.

## Key Changes
- Add `BRIDGE_BUILDING` as a Chalcolithic tech with prereqs `PERM_SETTLEMENT`, `DUGOUT_CANOE`, and `COPPER_TOOLS`; classify it as an institutional public-works tech.
- Add `TileKind::Bridge`, `BuildSiteKind::Bridge`, `Bridge` component, `BridgeMap`, recipe `"Timber Bridge"` using wood + stone, and deconstruction that restores the tile to `River`.
- Bridges are buildable only on `TileKind::River`, never lakes/ocean `Water`; they are passable, render distinctly, and use road-speed pathfinding cost.
- Update player right-click build UI so `Build Bridge` appears on river tiles when the player faction has adopted `BRIDGE_BUILDING`; construction routes workers to adjacent bank/bridge tiles.
- Keep `RoadCarveQueue` dry-only. Roads do not silently overwrite rivers. AI bridge construction goes through normal blueprints/material hauling.

## Settlement Planning
- Add a small river-context helper used by settled and nomadic camp seeding: nearest river, safe bank tiles, same-bank dry area, and local river orientation.
- Early camps prefer river-distance `3..=6`, avoid hearths/beds at `0..=1`, and keep camp structures on the same dry bank unless bridges are available.
- Adjust Paleolithic/Mesolithic hearth placement and nomadic camp seeding to project existing radial layouts onto safe same-bank candidates instead of letting deterministic offsets land in rivers or across channels.
- For Neolithic pre-bridge settlements, orient main roads parallel to nearby rivers and skip cross-river road/anchor segments that would create disconnected roads.
- Once `BRIDGE_BUILDING` is adopted, detect road segments crossing short river runs and emit bridge pressures/intents for the missing river tiles, building one bridge tile per selected project until the crossing is complete.

## Test Plan
- Unit tests for `TileKind::Bridge`: passable, floor, non-water-like, road-speed cost, render material coverage.
- Placement tests: Bridge blueprints allowed on `River`, rejected on `Water`, dry land, existing structures, and duplicate blueprint tiles.
- Player command/UI tests where a river right-click exposes only unlocked `Build Bridge`, then routes construction from adjacent passable tiles.
- Settlement tests for early camps: hearths/beds stay off river tiles and on the same bank; river-distance preference remains near water but not in the channel.
- Organic planner tests: pre-bridge road networks do not cross rivers; post-tech road crossings create bridge intents and become pathable after construction.
- Run `cargo test --bin civgame`.

## Assumptions
- V1 uses historically plausible timber/log bridge and causeway construction; no stone arches, because the current tech tree stops at Bronze Age.
- Bridge building is a public-works capability, so autonomous and player build options use community adoption, matching existing construction gates.
- Documentation should be updated in the repository guidance for changed settlement, tile, pathfinding, and construction behavior.
