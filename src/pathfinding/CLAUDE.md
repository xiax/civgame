# Pathfinding (`src/pathfinding/`)

Hierarchical pathfinder: chunk-level Dijkstra over a component-typed graph picks the chunk-route, then per-segment A* fills tile-level moves. Hotspot flow fields handle popular destinations.

## Component-typed chunk graph

Graph node = **`(ChunkCoord, ComponentId)`** — `ComponentId` is a chunk-local connected-component id assigned by 3D flood-fill at graph-build time over `NEIGHBOR_DIRS_3D`.

- **`ChunkComponents { at: AHashMap<(u8,u8,i8), ComponentId>, count: u8 }`** per chunk on `ChunkGraph::components`. Sparse — only standable foot tiles classify.
- **`ChunkEdge`** carries `from_component`, `to_component` so the router can't collapse a surface band and a disconnected cave into the same Dijkstra node.
- **`ChunkGraph::component_for_tile(world_x, world_y, z)`** is the agent-side lookup. Returns `None` when not standable or chunk not built — surface as `component_lookup_failed_at_*` in `PathfindingDiagnostics`.
- Rebuild runs in `PostUpdate` on `TileChangedEvent` / `ChunkLoadedEvent` / `ChunkUnloadedEvent`. `ChunkGraph::generation` bump invalidates every cached router tree.

## Router (`chunk_router.rs`)

Dijkstra over `RouterNode = (ChunkCoord, ComponentId)`, cached per goal. Three APIs:

- **`compute_route(graph, start, goal) -> Option<Vec<ChunkCoord>>`** — main worker entry; walks the tree's `next_hop` chain. No length cap; finite tree, optimal route.
- **`first_waypoint_full(graph, start, goal) -> Option<Waypoint>`** — first segment target with exact `(entry_x, entry_y, entry_z, neighbor_component)`.
- **`first_waypoint(graph, cur, dest, current_z) -> Option<(i32,i32)>`** — legacy compat for callers that don't track component identity. Tries every component at `current_z` in `cur` and `dest`, picks cheapest first hop.

Z-mismatch penalty is gone — components are exact, no "wrong z" choice to penalise.

## Connectivity (`connectivity.rs`)

`ChunkConnectivity` is a self-contained reachability snapshot built by `rebuild_connectivity_system` (after `build_chunk_graph_system`). `is_reachable((c1, z1), (c2, z2)) -> bool` so callers in `simulation/` don't need to thread a `&ChunkGraph`.

Internally: per-(chunk, z) → list of inter-chunk CC ids; reachability = set intersection.

`z_band(z) = z.div_euclid(4)` survives only as a debug-overlay helper — nothing in reachability uses it.

## Worker (`worker.rs`)

`drain_path_requests_system` drains up to `PATH_BUDGET_PER_TICK = 64` per tick. Per request:

1. Reject if `goal` not standable.
2. Look up start/goal components via `graph.component_for_tile`. Misses bump `component_lookup_failed_at_{start,goal}`.
3. Reachability via `router.is_reachable`.
4. `router.compute_route` produces the chunk sequence.
5. Hotspot fast-path if the goal is a registered hotspot in the start chunk and the agent's z matches the field's `cell_z`.
6. Single-segment A* via `find_path_in` against `first_waypoint_full(...)`.

Largest `chunk_route.len()` per tick surfaces as `chunk_route_len_max_last_tick`.

## Hotspot flow fields (`hotspots.rs`)

Pre-built per-chunk flow fields for popular destinations (faction centres, storage, rally points, doors, tunnel mouths). Fast-path used when goal is in start chunk; cross-chunk routing always goes through the router. **Flow fields are reserved for hotspots — per-agent local nav is A*.**

## Conventions

- Coords: world `(i32, i32, i8)`; chunks `ChunkCoord(i32, i32)`.
- Z: `i8`, `Z_MIN=-16`, `Z_MAX=15`.
- Passability: foot tile must be `passable` with `Air` or `Ramp` headspace above. `chunk_map.passable_at(x, y, z)` is authoritative.
- Cardinal-or-diagonal step with `|Δz| ≤ 1`. Diagonal corner-cut rejected when either side blocks (`passable_diagonal_step` in `step.rs`).
