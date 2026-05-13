# Use Exact Tile Reachability Everywhere

## Summary
- Centralize the worker’s exact component solution into a cheap reusable reachability helper.
- Replace simulation target filters that still ask “is chunk X reachable at this same z?” with “is this exact source tile/component connected to this exact target tile/component?”
- Keep the old `(chunk, z)` API only for debug/coarse legacy uses.

## Implementation Changes
- In `ChunkConnectivity`, store the `(ChunkCoord, ComponentId) -> global_cc_id` map already produced during rebuild, and add:
  - `component_reachable(from_node, to_node) -> bool`
  - `tile_reachable(graph, from_tile_3d, to_tile_3d) -> bool`
- Update all concrete simulation reachability checks to use `tile_reachable`:
  - `assign_task_with_routing`, adjacent-tile fallback helpers, and higher-tile recovery in `tasks.rs`.
  - HTN `reach_from_agent` closures, `pick_explore_tile`, survival storage filtering, and deposit-chain storage selection.
  - `StorageTileMap::nearest_for_faction_reachable`, changing its signature to accept `ChunkMap`, `ChunkGraph`, `ChunkConnectivity`, and the source tile z.
  - Nomad migration dispatch and player pitch-camp reachability checks.
- For every target tile, compute the target z with `chunk_map.nearest_standable_z(...)`; for deposit chains, compute the source z from the future pickup/gather tile, not the worker’s old z.
- Update pathfinding/simulation docs to state that simulation target selection must use exact tile reachability, while `(chunk, z)` reachability is debug/coarse only.

## Tests
- Add a connectivity unit test where chunk A at z=0 connects to chunk B at z=1:
  - Old same-z chunk check for B@0 fails.
  - New `tile_reachable(A tile z=0, B tile z=1)` succeeds.
- Add a storage-selection test where a nearby raised reachable storage tile beats a farther same-z tile.
- Run `cargo test --bin civgame pathfinding`.
- Run focused tests for storage/HTN dispatch if available, otherwise `cargo test --bin civgame`.

## Assumptions
- No new crates.
- Existing dirty files unrelated to this task remain untouched.
- Debug overlays may continue using coarse band/chunk views, but gameplay routing and target ranking should use exact tile/component reachability.
