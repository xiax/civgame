# River-Aware Detour-Cost Target Selection

## Problem

Workers picked gather/work/haul/bid/deposit targets by straight-line distance
from their tile, ignoring that a wide river forces a long walk-around. A
far-bank target (often near the faction base) was straight-line "close" but
cost a huge detour; a closer same-bank target lost. Targets were reachable
(workers got there eventually) — a path-cost ranking defect, not reachability.

## Solution (shipped)

Detour-aware distance estimator on the chunk-router layer, threaded into every
straight-line selection site.

- `chunk_router.rs`: `with_tree_from(graph, origin, f)` — origin-rooted Dijkstra
  tree; graph is weight-symmetric so `dist[c]` = cost origin↔c for every `c`
  (one build, O(1)/candidate). `ROUTER_CAPACITY` 64→256.
- `detour.rs` (new): `DetourEstimator::tiles = max(chebyshev, round(hop_cost ×
  CHUNK_SIZE/BASE_STEP_COST))`. Same-component ⇒ chebyshev; any failure ⇒
  chebyshev fallback (never 0/panic). `from(o,z,z_of)` curries for closure sites.
- Threaded into: `memory.rs` vision/scavenge pickers (`dist` closure param);
  `shared_knowledge.rs` cluster picker (`GatherKnowledge` gained router/graph/
  map; `gk.nearest_target_tile` gained `agent_z`); `nearest_for_faction_
  reachable` (gained `chunk_router`); `jobs.rs` U_bid `C_action`.
- `nearest_with_cluster_filter` speculative ring early-out removed (detour not
  monotone-by-ring); scans all rings up to `max_chunk_radius` (sparse, cheap).

## Scoped-out boundaries (stated, not silent)

- Plain `nearest_for_faction` stays Manhattan — cheap accessor for SOLO /
  landlord / storage-backend / bookkeeping; not a river-detour deposit. The
  routing-aware `nearest_for_faction_reachable` (the gather→deposit chain) is
  the one made detour-aware.
- `dist_penalty`/`full_trip_penalty` stay chebyshev — arbitrate *between
  methods for an already-detour-selected target*; live in `Method::utility`
  (no router access without trait surgery); not the bug's path.
- Drink/heal/hunt short-range scans keep chebyshev (no river detour at ≤ small
  radius).

## Verification

- `cargo test --bin civgame` — 746 pre-existing pass (no regression; htn `*_raw`
  exact-value tests green). New: `chunk_router` symmetry test; `detour` units
  (same-component / across-river / fallback) + **real-graph river-split test**
  via `rebuild_chunk_graph_sync` (far-bank chebyshev-near loses to same-bank
  chebyshev-far through the production River-impassable edge scan).
- Manual: `cargo run`, faction by a wide river — workers stop trekking around
  it to far-bank resources when nearer-bank ones exist.

Status: complete.
