# Hotspot Flow-Field Invalidation Drift

## Status: investigation needed

Skeleton plan — entry points and open questions only. Spawned from `plans/fix-sleep-stalls.md` Deferred follow-up. The Sleep-stall fix is a safety net (A* recovers); this plan addresses the root cause.

## Context

`pathfinding/worker.rs::compute_land`'s hotspot fast path can emit a path whose `cell_z` values disagree with the live `ChunkMap` enough to fail `first_invalid_step`. As of `plans/fix-sleep-stalls.md` the worker treats that as a recoverable cache miss and falls through to A*, but every fall-through means:

- One useless flow-field walk + one re-validation against live chunk state.
- A new `PathfindingDiagnostics::hotspot_fastpath_bad_steps` increment.
- A worst-case A* segment instead of the O(walk-length) flow-field shortcut.

The cache **does** have an invalidator (`pathfinding/mod.rs::invalidate_pathing_on_tile_change_system` line ~104) that drains `TileChangedEvent` and calls `HotspotFlowFields::invalidate_chunk(coord)` for the touched chunk + 8 neighbors. `invalidate_chunk` removes the entry from `entries` and pushes its `HotspotKey` onto `dirty`, so `lookup_field` returns `None` until `rebuild_dirty_hotspots_system` rebuilds. So why do bad-steps still fire?

## Open questions to resolve

1. **Do all Z-mutating events emit `TileChangedEvent` synchronously?**
   - Wall finalize: `construction.rs:5101` sends `WallConstructed`. Does it also emit `TileChangedEvent`? (Worth grepping.)
   - Wall destruction: `construction.rs:3764` sends `WallDestroyed`. Same question.
   - Excavation finalize: `carve::finalize_carved_tile` — does it emit `TileChangedEvent`?
   - Terraform: `terraform.rs:243` sends `TileCarvedEvent`. Does the upstream `terraform_system` also push `TileChangedEvent`?
   - Door open/close (`Door.is_open` toggle).
   - `restamp_runtime_water_on_chunk_load` (does it emit?).
   - Dam `register_dam` / `clear_dam`.
   - **Hypothesis to test:** find any path-relevant tile mutation that goes through `set_tile` (or equivalent) without emitting `TileChangedEvent` — that mutation skips the invalidator.

2. **Is there a tick-ordering race?**
   - `invalidate_pathing_on_tile_change_system` runs in `PostUpdate`.
   - `drain_path_requests_system` runs `PreUpdate` (per `pathfinding/CLAUDE.md`).
   - `rebuild_dirty_hotspots_system` runs in `PostUpdate` (need to verify schedule slot).
   - **Hypothesis:** a tile change in tick N's `Sequential`/`Economy` emits the event. PostUpdate N drains it and invalidates. Rebuild N drains some-but-not-all of `dirty` (budget cap = `PerfWorkBudget.hotspot_rebuilds_per_tick`). Tick N+1 PreUpdate worker requests a path — that's the new tick, so the entry **is** out of `entries`, `lookup_field` returns `None`, no bad-step. So timing alone shouldn't produce a bad-step **unless** the rebuild fires before the invalidator, restoring a stale field.
   - **Check:** are these systems explicitly ordered (`.before`/`.after`)? If not, Bevy can interleave them — could a rebuild observe a chunk in `dirty` and rebuild against a `ChunkMap` snapshot that doesn't yet include the latest mutation?

3. **Does the flow-field BFS legitimately encode a multi-z path that becomes invalid after a cell mutates without invalidating the *field's* chunk?**
   - `FlowField.cell_z` records the standable foot-Z the BFS reached at each cell — could climb via a ramp inside the chunk (per `flow_field.rs:11-15`).
   - **Hypothesis:** field built at time T includes path `(x,y,z=0) → (x+1,y,z=1)` via a ramp. At time T+k, the ramp tile mutates — but the change happens at the same chunk so `invalidate_chunk` *should* fire. Need to verify.

4. **Cross-chunk paths.** The fast path only fires for `chunk_route.len() == 1` — same chunk. The invalidator covers the 8-neighbor ring. Is there a path through a chunk *not* in that ring that the field encodes? (Probably not, since the field is bounded to the field's own chunk.)

5. **Initial telemetry baseline.** With the new `hotspot_fastpath_bad_steps` counter shipped, how high does it run in a 1-hour `cargo run` session? If near-zero, the drift is a corner case and this plan can stay deferred indefinitely. If meaningful (≥10/min), prioritise.

## Entry points

- `src/pathfinding/hotspots.rs` — the registry, `invalidate_chunk`, `rebuild_dirty_hotspots_system`, `lookup_field`.
- `src/pathfinding/mod.rs:100` — `invalidate_pathing_on_tile_change_system`; check what schedule slot it's in and whether it's ordered against `rebuild_dirty_hotspots_system`.
- `src/pathfinding/flow_field.rs` — `build_flow_field` (per-cell `cell_z` semantics) and `walk_to_goal`.
- `src/pathfinding/worker.rs::compute_land` — the bad-step branch (where the counter increments).
- **Mutation sites to audit for `TileChangedEvent` emission:**
  - `src/simulation/construction.rs` Wall finalize + `wall_destruction_system` (Wall/Door).
  - `src/world/carve.rs::finalize_carved_tile` (excavation/dig-down).
  - `src/simulation/terraform.rs` (sloping, ramp construction).
  - `src/world/water_runtime.rs::restamp_runtime_water_on_chunk_load` + `register_dam` / `clear_dam`.
  - `src/world/chunk_streaming.rs` (chunk load/regen — chunk-scope, probably handled by `invalidate_chunk` on `ChunkLoadedEvent` already; verify).

## Investigation checklist

1. **Inventory.** `grep -rn "TileChangedEvent\s*{" src/` — list every emit site. Map to the mutation site. Identify any mutation without an emit.
2. **Schedule audit.** Read `pathfinding/mod.rs::PathfindingPlugin::build` (or wherever systems are registered) and confirm `invalidate_pathing_on_tile_change_system` runs *before* `rebuild_dirty_hotspots_system` every PostUpdate.
3. **Telemetry probe.** After this plan's safety net ships, ride a `cargo run` session for ~30 minutes at default speed and read `PathfindingDiagnostics::hotspot_fastpath_bad_steps`. Decide priority based on rate.
4. **Reproducer.** Build a `TestSim` harness: register a hotspot, mutate one path tile via the suspected emit-less path, immediately request a path through it, assert the field was rebuilt OR the path failed cleanly. Each "missed event" candidate gets its own test case.

## Likely fixes (do not ship before investigation)

- Wire missing emit sites to also send `TileChangedEvent`.
- Add `.after(invalidate_pathing_on_tile_change_system)` to `rebuild_dirty_hotspots_system` if not already explicit.
- Drop a hotspot's entry on the `cell_z` mismatch detected in `compute_land`'s bad-step branch (cheap self-heal: the worker already knows the field is stale, so push its key back onto `dirty` while falling through to A*). This is the smallest viable fix even if the deeper invalidation gap isn't found.

## Out of scope

- Flow-field algorithm changes — the BFS is fine, the cache is what drifts.
- Per-agent flow fields — flow fields stay reserved for hotspots per `pathfinding/CLAUDE.md`.
- No new diagnostics — `hotspot_fastpath_bad_steps` is the relevant signal.

## Verification once a fix ships

- The new `tests::hotspot_bad_step_falls_through_to_astar` regression in `worker.rs` should still pass (synthetic bad cache → fall through to A*).
- New test: build a real hotspot field, send a `TileChangedEvent`/`TileCarvedEvent`/`WallConstructed` for a path cell, request a path, assert `lookup_field` returns `None` (entry was invalidated). Mirror per emit type identified as missing.
- After 30-minute `cargo run`, `hotspot_fastpath_bad_steps` should be 0 or close to it.
