# Fix Illogical Chunk Pathing

## Summary
Make cross-chunk movement choose locally sensible portal tiles instead of scan-order border waypoints. The worker should route to the real destination, and only use the chunk graph to constrain which neighboring chunk/component comes next.

## Key Changes
- In `assign_task_with_routing`, remove the legacy `first_waypoint` pre-route path. For cross-chunk tasks, set `ai.target_tile` to the actual `route_target`, `ai.target_z` to its standable Z, and let `PathFollow` own segment planning.
- Replace `worker::first_segment_target` with a geometry-aware first-hop selector:
  - Ask `ChunkRouter::first_waypoint_full` only for the next chunk/component.
  - Enumerate all graph edges from the start chunk that enter that exact next chunk/component.
  - Score candidate entry tiles by `chebyshev(req.start, exit_tile) + chebyshev(entry_tile, req.goal)`, with small terrain/Z costs from existing edge data.
  - Return the best candidate’s neighbor entry tile as the segment target.
- Keep router Dijkstra component-typed and cached; do not add new crates or a full portal graph rewrite.
- Update pathfinding docs to describe the split: router chooses chunk/component hop, worker chooses the best concrete portal for the current start/goal.

## Test Plan
- Add a worker/router regression test with two adjacent flat chunks where the old scan-order waypoint would choose a border corner; assert the selected first segment target is near the straight line between start and goal.
- Add a movement/task regression test or focused unit test proving cross-chunk task assignment stores the real destination in `ai.target_tile`, not a legacy waypoint.
- Run `cargo test --bin civgame pathfinding` or the closest targeted pathfinding tests, then `cargo test --bin civgame`.

## Assumptions
- Workers should prefer the shortest locally sensible route over preserving old `Routing` waypoint behavior.
- Chunk-route optimality remains coarse-grained; this fix targets the obvious bad first portal choice without replacing the whole hierarchical pathfinder.
