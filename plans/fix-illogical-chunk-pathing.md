# Fix Illogical Chunk Pathing — SHIPPED

## Goal
Stop cross-chunk movement routing workers to nonsensical border waypoints
(chunk corners) before heading to the real destination.

## Two root causes (both fixed)
1. **Destination truncation.** `assign_task_with_routing` (`tasks.rs`) pre-set
   `ai.target_tile` to a legacy scan-order `first_waypoint` for cross-chunk
   tasks; `movement_system` feeds `ai.target_tile` verbatim as the
   `PathRequest.goal`, so the PathFollow worker never saw the true destination.
2. **Scan-order portal.** The graph stores one `ChunkEdge` per border tile;
   `first_waypoint_full`'s Dijkstra `next_hop` returns the first-relaxed edge
   (a corner), and `worker::first_segment_target` used it verbatim.

## Changes shipped
- **`tasks.rs::assign_task_with_routing`** — cross-chunk branch removed; now
  unconditionally `ai.state = AiState::Seeking; ai.target_tile = route_target`
  (the real stand tile/destination). The worker owns all segment planning.
  `_chunk_router` param retained (underscored) for signature stability across
  ~100 callers. `AiState::Routing` kept — still used by nomad migration
  checkpoints (`nomad.rs` `begin_routing`).
- **`worker.rs::first_segment_target`** — uses `first_waypoint_full` only for
  next chunk + component *identity*, then enumerates edges from the start chunk
  into that `(neighbor, neighbor_component)` and picks the portal minimising
  `chebyshev(start, exit) + chebyshev(entry, goal)` (traverse_cost rescaled to
  tiles as tie-break). Re-runs per hop. Falls back to the router's pick if no
  matching edge enumerable.
- **Docs** — `pathfinding/CLAUDE.md` (Worker) + `simulation/CLAUDE.md`
  (encapsulation note) updated.

## Tests (both green; suite 1445 passed / 0 failed)
- `worker::tests::first_segment_target_picks_portal_near_straight_line` — 2 flat
  chunks; asserts entry tile is across the border tracking the goal direction
  (chebyshev plateau y∈[17,25]), not corner 0.
- `tasks::tests::cross_chunk_task_stores_real_destination_not_waypoint` —
  asserts `ai.target_tile == target` (not a waypoint) + `AiState::Seeking`.

## Optional follow-up (not done)
- `goals.rs:1219` still calls legacy `first_waypoint` as a boolean reachability
  probe; could migrate to `ChunkConnectivity::tile_reachable` and delete
  `first_waypoint`. Out of scope.
