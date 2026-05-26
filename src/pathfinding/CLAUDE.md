# Pathfinding (`src/pathfinding/`)

Hierarchical pathfinder: chunk-level Dijkstra over a component-typed graph picks the chunk-route, then per-segment A* fills tile-level moves. Hotspot flow fields handle popular destinations.

## Component-typed chunk graph

Graph node = **`(ChunkCoord, ComponentId)`** — `ComponentId` is a chunk-local connected-component id assigned by 3D flood-fill at graph-build time over `NEIGHBOR_DIRS_3D`.

- **`ChunkComponents { at: AHashMap<(u8,u8,i8), ComponentId>, count: u8 }`** per chunk on `ChunkGraph::components`. Sparse — only standable foot tiles classify.
- **`ChunkEdge`** carries `from_component`, `to_component` so the router can't collapse a surface band and a disconnected cave into the same Dijkstra node.
- **`ChunkGraph::component_for_tile(world_x, world_y, z)`** is the agent-side lookup. Returns `None` when not standable or chunk not built — surfaces as `component_lookup_failed_at_*` in `PathfindingDiagnostics`.

### Rebuild pipeline

- **Startup** (after `terrain::spawn_world_system`): `startup_initial_build_system` synchronously rebuilds the pre-generated 32×32 spawn area.
- **Runtime async**: `enqueue_graph_dirty_system` (PostUpdate) drains `TileChangedEvent` / `ChunkLoadedEvent` / `ChunkUnloadedEvent` into `GraphDirty { classify, unloaded }`. `spawn_rebuild_task_system` snapshots up to `PerfWorkBudget.graph_classify_chunks_per_task` (default 16) + all unloads onto `AsyncComputeTaskPool`; `poll_rebuild_task_system` (PreUpdate) merges into `ChunkGraph`, records timings in `BackgroundWorkDiagnostics`.
- **Connectivity** has its own async snapshot/poll/apply pair (`spawn_connectivity_rebuild_task_system` / `poll_connectivity_rebuild_task_system`); applies only if generation still matches, else bumps stale-drop diagnostic. `tile_reachable` treats stale connectivity as reachable so producers don't hard-fail mid-rebuild.
- **Tests** bypass streaming and call `rebuild_chunk_graph_sync` directly (`TestSim::flat_world`).
- `ChunkGraph::generation` bump invalidates every cached router tree.

## Router (`chunk_router.rs`)

Dijkstra over `RouterNode = (ChunkCoord, ComponentId)`, cached per root node (`ROUTER_CAPACITY = 256`, FIFO, wholesale-dropped on `graph.generation` bump). Four APIs:

- **`compute_route(graph, start, goal) -> Option<Vec<ChunkCoord>>`** — main worker entry; walks the tree's `next_hop` chain. No length cap; finite tree, optimal route.
- **`first_waypoint_full(graph, start, goal) -> Option<Waypoint>`** — first segment target with exact `(entry_x, entry_y, entry_z, neighbor_component)`.
- **`first_waypoint(graph, cur, dest, current_z) -> Option<(i32,i32)>`** — legacy compat for callers that don't track component identity. Tries every component at `current_z` in `cur` and `dest`, picks cheapest first hop.
- **`with_tree_from(graph, origin, f) -> Option<R>`** — build-or-cache the `origin`-rooted tree and run `f` on it under the lock. The chunk graph is **weight-symmetric**, so an origin-rooted tree's `dist[c]` is the optimal chunk-path cost between `origin` and *every* reachable `c` — one Dijkstra answers cost-to-every-candidate in O(1) per candidate. Backs the detour estimator.

Z-mismatch penalty is gone — components are exact, no "wrong z" choice to penalise.

## Detour estimator (`detour.rs`)

`DetourEstimator { router, graph }` — river-aware distance in **chebyshev-equivalent tiles** for target-selection sites. `tiles(o_tile, o_z, c_tile, c_z) = max(chebyshev, round(with_tree_from(o_node).dist[c_node] × ROUTER_UNITS_TO_TILES))`; `ROUTER_UNITS_TO_TILES = CHUNK_SIZE / BASE_STEP_COST` (derived). Same chunk-component ⇒ plain chebyshev; any resolution failure ⇒ chebyshev fallback (never 0, never panic). `from(o_tile, o_z, z_of)` curries the origin for closure-shaped call sites. One agent-rooted Dijkstra per re-planning agent (200-tick bucketed), generation-only invalidation. Consumers: `memory.rs` vision/scavenge pickers, `shared_knowledge.rs` cluster picker (via `GatherKnowledge`), `faction.rs::nearest_for_faction_reachable`, `jobs.rs` U_bid `C_action`.

## Connectivity (`connectivity.rs`)

`ChunkConnectivity` is a self-contained reachability snapshot built at startup by `rebuild_connectivity_system` and at runtime by the async connectivity task. Three reachability APIs at different precision levels:

- **`tile_reachable(graph, from_3d, to_3d) -> bool`** — *exact* tile-to-tile reachability. Resolves each endpoint's `ComponentId` via `ChunkGraph::component_for_tile` and tests equality of inter-chunk CC ids. **Authoritative gameplay-routing API.** Storage picks, vision pickers, adjacency fallback, migration commit, player pitch-camp all gate on this. Costs one `ChunkGraph` borrow at the call site.
- **`component_reachable(from_node, to_node) -> bool`** — same precision but caller has already resolved nodes. Used by the path worker.
- **`is_reachable((chunk, z), (chunk, z)) -> bool`** — coarse overload. OR-merges every component touching `z` in `chunk`, so it can return `true` when the agent's actual cell is in a disconnected component sharing only a `z` slice with the target. **Kept for the debug overlay and a few legacy callers** — not for gameplay routing.

Internally: per-(chunk, ComponentId) → inter-chunk CC id (`tile_reachable` / `component_reachable`); per-(chunk, z) → list of CC ids (legacy overload). Both rebuilt together by `populate_connectivity_from_graph`.

**Placement-time reachability** is *not* `ChunkConnectivity`: the graph is not reliably built at `OnEnter(Playing)` and would not reflect walls seeded during that pass. `simulation::placement_reachability::path_exists` is the seed-safe authoritative check — a bounded A* over the live `ChunkMap` using these same canonical step rules (`passable_step_3d` / `passable_diagonal_step`). `connectivity_prefilter` wraps `tile_reachable` as that module's optional O(1) runtime-only fast-reject.

`z_band(z) = z.div_euclid(4)` survives only as a debug-overlay helper.

## Worker (`worker.rs`)

`drain_path_requests_system` drains up to `PATH_BUDGET_PER_TICK = 64` per tick. Per request:

1. Reject if `goal` not standable.
2. Look up start/goal components via `graph.component_for_tile`. Misses bump `component_lookup_failed_at_{start,goal}`.
3. Reachability via `router.is_reachable`.
4. `router.compute_route` produces the chunk sequence.
5. Hotspot fast-path if goal is a registered hotspot in the start chunk and agent's z matches field's `cell_z`.
6. Single-segment A* via `find_path_in` against `first_waypoint_full(...)`.

Largest `chunk_route.len()` per tick surfaces as `chunk_route_len_max_last_tick`.

## Hotspot flow fields (`hotspots.rs`)

Pre-built per-chunk flow fields for popular destinations (faction centres, storage, rally points, doors, tunnel mouths). Fast-path used when goal is in start chunk; cross-chunk routing always goes through the router. Dirty fields rebuild through a small per-tick budget (`PerfWorkBudget.hotspot_rebuilds_per_tick`) so tile-change bursts do not drain every field in one PostUpdate. **Flow fields are reserved for hotspots — per-agent local nav is A*.**

**Bad-step fallthrough.** When the fast path's emitted path fails `first_invalid_step` against live `ChunkMap` (cached `cell_z` drifted past a terrain mutation), the worker sets `hotspot_fastpath_bad_step`, flags `ComputeOutcome.evict_hotspot_goal = Some(goal_tile)`, and **falls through to A***, rather than returning `NoRouteStepContinuity`. Stale flow-field cells are a recoverable cache miss; A* re-routing against live state is the authoritative fallback. A* itself emitting a bad-step path still fails the request — that's a real planner/runtime inconsistency. Counter `PathfindingDiagnostics::hotspot_fastpath_bad_steps` (distinct from `hotspot_fastpath_misses`) measures invalidation drift in-game.

**Same-Update self-heal (`HotspotFlowFields::evict_field_for_goal`).** `PostUpdate`'s invalidator runs once per Update, but `FixedUpdate` (where Sequential mutations + `drain_path_requests_system` live) can fire multiple times per Update at higher speed presets or under frame-hitch catch-up. Between PostUpdate runs the cache can be genuinely stale even when every emit site is correct. After the parallel compute scope, `drain_path_requests_system` walks each outcome's `evict_hotspot_goal` and calls `HotspotFlowFields::evict_field_for_goal(goal_tile)` — drops every `HotspotKey { tile, .. }` entry across all kinds and pushes them onto `dirty`. Subsequent FixedUpdate iterations this same Update no longer re-walk the stale field; the next PostUpdate rebuilds it normally.

## Amphibious traversal (swimming)

`TraversalProfile { Land, Amphibious }` (`tile_cost.rs`). `Land` is the historical behaviour. `Amphibious` additionally treats a **water-surface cell** as standable — `ChunkMap::passable_for(x,y,z,profile)` accepts a wet column (`water_depth_at > 0`) at its surface Z with `Air` headspace; `step_cost_for(kind, profile)` gives `Water`/`River` a finite expensive cost (`SWIM_SPEED_MULT = 0.35`) instead of `IMPASSABLE`. `passable_step_for` / `passable_diagonal_step_for` are the profile-aware step checks; `astar::find_path_profile` is the profile-aware A* (`find_path_in` = `Land` wrapper).

`PathRequest`/`PathFollow` carry `profile`; `PathRequestQueue::enqueue_with_profile` (plain `enqueue` stays `Land`). `movement_system` enqueues `Amphibious` for humans on foot, `Land` for mounted humans (animals never reach it), and uses `passable_step_for(.., pf.profile)` for the boundary check so a swimmer isn't snapped back off a water tile.

**Amphibious worker path is land-first.** `compute_outcome` runs the chunk-graph route (`compute_land`) first; for `Amphibious` requests it falls back to `compute_amphibious` **only** when land routing returns `Unreachable`/`NoRoute` (banks split by water). `compute_amphibious` is a single bounded full-route A* over `passable_for(Amphibious)` packed into one segment (`chunk_route = [start_chunk]`); a swim exceeding the A* budget fails gracefully.

## Tile cost reads (`tile_cost.rs`)

A* (`astar.rs`) and flow fields (`flow_field.rs`) compute step cost from full `TileData`, not just `TileKind`. `tile_speed_multiplier_from_data` / `tile_step_cost_from_data` apply the per-level partial-excavation slowdown (levels 1..=6 multiply base mult by `1.0 − 0.08·level`, giving 0.92..0.52); levels 0 and 7 leave the base unchanged. `step_cost_for_data(data, profile)` is the profile-aware variant (Land / Amphibious water still uses kind-only fast-path for swim cost). `movement::movement_system` reads the same helper at the agent's surface tile so partial excavation actually slows agents traversing it. Flow fields are **not** re-invalidated on excavation `TileChangedEvent` — they're hotspot-anchored and rebuilt lazily; brief partial-excavation cost staleness is acceptable.

## Conventions

- Coords: world `(i32, i32, i8)`; chunks `ChunkCoord(i32, i32)`.
- Z: `i8`, `Z_MIN=-16`, `Z_MAX=15`.
- Passability: foot tile must be `passable` with `Air` or `Ramp` headspace above. `chunk_map.passable_at(x, y, z)` is authoritative.
- Cardinal-or-diagonal step with `|Δz| ≤ 1`. Diagonal corner-cut rejected when either side blocks (`passable_diagonal_step` in `step.rs`).
