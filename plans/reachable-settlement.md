# Reachable Settlement Placement — SHIPPED

## Context

Settlement planning, seed-time construction, starting farms, and faction
spawning picked tiles validated only for per-tile passability — never for
*walkability from the faction's connected area*. In a river/cliff-heavy world
this stranded farm plots across rivers, spawned members in isolated pockets, and
let walled houses seal their own beds. Only `doormat_reaches_home` checked
anything, and only for the door. This adds one shared reachability layer used by
every placement surface.

Key correction over the original draft: `ChunkConnectivity::tile_reachable` is
**unusable at seed time** — `OnEnter(Playing)` orders the connectivity build and
the seed systems independently, and seeded walls are stamped into `ChunkMap`
*during* the seed pass (connectivity only rebuilds at runtime from
`TileChangedEvent`). The authoritative check is therefore a self-contained
bounded A* over the live `ChunkMap` using the canonical agent step rules;
connectivity is only an optional runtime fast-reject.

## What shipped

**New module `src/simulation/placement_reachability.rs`:**
- `path_exists(chunk_map, from, to, ReachOpts)` — bounded A* over the live
  `ChunkMap` via `passable_step_3d` / `passable_diagonal_step` (same rules the
  worker uses). A found path proves walkability; within-chunk sealed pockets are
  rejected. `ReachOpts { max_expansions, blocked }` — `blocked` overlays planned
  walls / the Stone-aversion heuristic. Fails closed on budget exhaustion.
- `connectivity_prefilter` — optional O(1) runtime-only fast-reject; `None` when
  graph unbuilt so seed callers degrade to `path_exists`.
- Wrappers: `resolve3` (surface-z), `tile_reachable_from_home`,
  `rect_reachable_from_home`, `simulate_house_reachable` (doormat→home through
  finished walls + every interior bed via the door + door↔doormat z-step),
  `spawn_tiles_from` (BFS frontier — reachable-by-construction).

**Phase 0:** module + primitive. `construction::doormat_reaches_home` folded
into `path_exists` (Stone-aversion overlay + 1500 cap preserved; step model
upgraded to agent-faithful 3D). No parallel BFS remains.

**Phase 1 — seed-time gates:** `plan_building` / `plan_composite_building` /
`seed_walled_house_at` (simulated house, abort on fail); `seed_starting_farms_system`
(two-tier: terrain+reachable, then any-reachable, then (8,0) last resort);
`seed_farmstead_yard` (gained `home`; yard must reach home given just-stamped
walls); `spawn_population` member tiles + `seed_market_households` (drawn from
`spawn_tiles_from`, legacy search only as exhaustion fallback).

**Phase 2 — runtime gates:** `choose_site_for_intent` (both parcel + frontier
loops); a **single** `chief_directive_system` pre-spawn gate before
`spawn_intent` (consolidation — covers both organic-selected intents and the
`generate_candidates` fallback in one choke point instead of 16 per-push edits);
`chief_job_posting_system` validates the Farm plot rect **once** at posting time
(`FarmJobPostingParams` gained `chunk_map` to stay under the 16-param ceiling).

## Deliberate scope decisions

- **No per-tile prefilter in `find_nearest_unplanted_in_rect`.** Plot-level
  validation once at posting makes per-tile checks redundant (a contiguous
  16×16 plot is internally connected) and avoids per-dispatch churn / hot-path
  recompute. Dropped as over-engineering.
- **Single pre-spawn gate, not 16 candidate-push edits.** One choke point in
  `chief_directive_system` subsumes the `generate_candidates` fallback — DRY,
  and matches the "one shared accessor, every site" principle.
- **Doormat fold-in is more permissive (8-connected 3D), not byte-equivalent**
  to the old 4-connected 2D BFS — that is the correct direction (matches real
  agent pathing); sealed courtyards still fail because `passable_diagonal_step`
  rejects wall-corner pinches.

## Critical files

`src/simulation/placement_reachability.rs` (new), `mod.rs` (register),
`construction.rs` (fold-in + `plan_reachable_from_home` helper + house/yard/
pre-spawn gates), `organic_settlement.rs`, `farm.rs`, `person.rs`, `jobs.rs`,
`src/simulation/CLAUDE.md`, `src/pathfinding/CLAUDE.md`. Reused read-only:
`ChunkMap::{passable_step_3d,surface_z_at,nearest_standable_z}`,
`pathfinding::step::passable_diagonal_step`,
`ChunkConnectivity::tile_reachable`, `walled_house_tile_plan`/`shape_tiles`.

## Verification

`cargo test --bin civgame` → **755 passed, 0 failed** (749 baseline incl.
`onenter_era_seeding` which stamps real Neo/Chalco/Bronze walled houses — proves
the gates accept real layouts + the fold-in is behaviourally safe; + 6 new unit
tests: open path, within-chunk pocket rejection, `spawn_tiles_from` reachable-
only, isolated-rect rejection, simulated-house accept, simulated-house sealed-bed
reject). End-to-end sanity: `cargo run` a river-bisected spawn megachunk and
confirm no farm/member/house lands on the far bank and no perpetually
unreachable Farm/Build postings.
