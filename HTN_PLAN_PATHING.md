# HPA* (Hierarchical Pathfinding A*) Overhaul Plan

## Motivation
The current chunk router suffers from a critical 3D topology flaw: it lumps all Z-levels of a chunk into a single node (`ChunkCoord`). This causes the router to falsely merge surface paths and underground caves, leading to infinite Z-penalty loops (A -> B -> A) and `FailSubReason::NoRouteRouter` errors when the agent reaches the 64-chunk route limit.

## Architecture: Portals
We will transition from a "Chunk Graph" to a "Portal Graph". 
- **Nodes:** A Portal (a contiguous line of passable border tiles between two chunks).
- **Edges:** 
  1. *Inter-chunk:* Connecting a Portal on Chunk A's border to its exact mirror Portal on Chunk B's border (Cost = 1).
  2. *Intra-chunk:* Connecting Portal A to Portal B *within the same chunk* (Cost = precise distance calculated via local BFS/Dijkstra).

## Implementation Phases

### Phase 1: Portal Generation (`chunk_graph.rs`)
1. **Identify Contiguous Borders:** Modify the border scanning logic to group adjacent passable tiles (that share |Δz| ≤ 1) into a single `Portal`.
2. **Assign Waypoints:** Select the median tile of the portal as its canonical `(x, y, z)` waypoint.
3. **Portal Struct:**
   ```rust
   pub struct PortalId(pub u32);
   
   pub struct Portal {
       pub id: PortalId,
       pub chunk: ChunkCoord,
       pub center_local: (u8, u8, i8), // lx, ly, z
       pub partner_portal: PortalId,   // The mirror portal in the adjacent chunk
   }
   ```

### Phase 2: Intra-Chunk Edges (`chunk_graph.rs`)
1. **Local BFS:** For every chunk, after identifying its portals, run a bounded BFS/Dijkstra starting from each portal to all other portals in the same chunk.
2. **Graph Structure:** The graph maps `PortalId -> Vec<PortalEdge>`.
   ```rust
   pub struct PortalEdge {
       pub target: PortalId,
       pub cost: u16,
   }
   ```

### Phase 3: Router Overhaul (`chunk_router.rs`)
1. **Dijkstra on Portals:** The cached shortest-path trees will map `PortalId -> u32` distance to the goal.
2. **Waypoints:** `first_waypoint` will query the graph to find the sequence of Portals, returning the exact `(x, y, z)` of the next Portal rather than a vague chunk coordinate.

### Phase 4: Path Request Integration (`worker.rs`)
1. **Virtual Start/Goal Portals:** When planning a path, the worker calculates the distance from the Start `(x, y, z)` to all Portals in the Start chunk, and from the Goal `(x, y, z)` to all Portals in the Goal chunk.
2. **High-Level Route:** The router Dijkstra connects these virtual portals to yield an iron-clad list of waypoints.
3. **Segment Execution:** The existing A* worker connects the dots between Portals, guaranteed to succeed because intra-chunk reachability was proven during graph generation.

## Migration & Rollback
- The old `ChunkGraph` logic will be entirely replaced. 
- `ChunkConnectivity` can remain as a fast early-reject filter, or it can be derived directly from the connected components of the new Portal Graph.
- We will implement this in a separate PR/branch to allow testing of the pathfinding diagnostic counters.