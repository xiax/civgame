# Flowing Water, Reservoirs, Aquifers, Dams

## Status

**✅ COMPLETE — all 7 phases shipped. End-state docs: `src/world/CLAUDE.md` "Water system" +
`src/simulation/CLAUDE.md` thirst/construction. v2 deferrals at the bottom of this file.**

- **Phase 0** ✅ audit done (findings folded into `src/world/CLAUDE.md` "Water system").
- **Phase 1** ✅ hydrology truth shipped, additive. `GLOBE_FILE_VERSION` 8.
- **Phase 2** ✅ chunk water columns (`surface_ground_z`/`water_depth`/`reservoir_id`),
  `ground_z_at` accessors, reservoir basin-membership stamping (replaces lake discs), 9
  terrain-elevation callers migrated. `surface_z_at` meaning unchanged. 768 tests green, 0
  regressions. `land.rs::plot_value_factor` kept on `surface_z_at` (audit over-broad there).
- **Phase 3** ✅ persistent runtime water layer. `RuntimeWater` resource (tile-keyed
  `RuntimeWaterCell` + `runtime_reservoirs`) + `Chunk::apply_water_column` + `world/water_runtime.rs`
  `restamp_runtime_water_on_chunk_load` (FixedUpdate, after `chunk_streaming_system`): re-applies
  runtime columns + re-stamps `Bridge` from `BridgeMap` on `ChunkLoadedEvent` → **closes the Phase 0
  bridge-reverts-on-reload gap**. `salinity`/`source_rate`/`runtime_reservoirs`/`Dam` are Phase 4/5
  producers (scaffold, no live writers yet). 775 tests green, 0 regressions.
- **Phase 4** ✅ dams (mirror shipped Bridge pipeline). `TileKind::Dam=25` (26 variants; passable +
  road-speed, NOT water-like/fresh/drinkable). `Dam` entity in `DamMap` = durable truth; tile kind =
  cache projection restamped from `DamMap` (shared `stamp` closure with Bridge). `BuildSiteKind::Dam`
  (6 stone+4 wood, 180 work, `BRIDGE_BUILDING`-gated), `is_water_anchored()` covers `Bridge|Dam`,
  finalize registers `RuntimeWater.dam_crests` crest barrier, deconstruct restores + clears barrier +
  bank-refund (generalised `water_anchored_refund_tile`). Right-click Build Dam on River/Water.
  v1 player-built only (no AI emitter — v2). `FurnitureMaps` gained `dam_map`+`runtime_water`;
  `test_fixture` now inserts `RuntimeWater`. Tests: tile semantics + dam restamp.
- **Phase 5** ✅ background fluid sim. `world/water.rs` pure deterministic volume-conserving
  virtual-pipe core (`WaterGrid` Free/Pinned + dam-weir; 6 unit tests incl. dam pool→overtop→drain,
  conservation, bitwise determinism). `water_runtime.rs` async wrapper: `WaterSim` (one
  `AsyncComputeTaskPool` task, 20-tick cadence, mirrors pathfinding) — `spawn` snapshots a bounded
  region (R=28 boxes around dams + runtime cells) with ocean/lake/ghost pins + highest-boundary
  discharge inlets + weir crest = dam footing + `DAM_RISE_Z=3`; `poll` writes `RuntimeWater`
  (persistent) + `ChunkMap`, restores natural kind on drain, emits `TileChangedEvent` only on
  wet/dry flip (deadband). `GLOBE_H_TO_Z` made `pub`. Deferred v2: spring/aquifer sources, per-cell
  routing, structural breach, seasonal discharge.
- **Phase 6** ✅ gameplay integration. `WaterKind::Brackish` added; `water_kind_at` body reads
  `Globe::salinity_at` via pure `classify_salinity` (signature byte-identical); `is_drinkable()`
  (Fresh-only) is the single salt/brackish rejection rule, wired into drink + animal water-seek.
  Aquifer wells: `well_has_water`/`well_reaches` (table within `WELL_REACH_Z=4` shaft);
  `nearest_well_tile` skips dry wells; `DrinkOutcome::WellDry` graceful fail. **Re-tune audit:
  diagnosed no change needed** — settlement/nomad/herd ride `is_freshwater()`/`river_distance_at`
  (unchanged across Phases 0–5), none read salinity.
- **Phase 7** ✅ documentation. `src/world/CLAUDE.md` "Water overhaul" phased changelog consolidated
  into a terse end-state "Water system" section (169→~80 lines; phase log/rationale/v2-deferrals
  live here in the plan, not the always-loaded doc); stale `GLOBE_FILE_VERSION 7→8` + river /
  `water_kind_at` lines in "World generation" corrected. `src/simulation/CLAUDE.md` (dam pipeline,
  aquifer wells, salinity drinking) + root `CLAUDE.md` (`Dam` palette + 26-variant count) updated
  incrementally per phase. **Overhaul complete.**

## Context

Replace "river = tile stamped one z lower" with real water columns: separate ground/bed z, water
level, depth (sub-z f32), flow, and reservoir/aquifer identity, plus deterministic worldgen
hydrology truth and a background fluid simulation over loaded chunks. Decisions: full geometry
overhaul in v1; persistent runtime water (chunks regenerate fresh on stream-in, so water lives in a
tile-keyed resource + restamp-on-load); realistic volume-conserving fluid sim run on a background
thread.

### Why the naive approach was risky (fixed here)

1. Runtime water stored "in chunks" is destroyed on unload — `Chunk` is not serialized;
   `generate_chunk_from_globe` rebuilds caches from `Globe+seed`. → Phase 3 persistent layer.
2. Discrete Z (i8, −16..15, 1 tile ≈ 1.5 m) can't express 0.5 m stream vs 2 m pond → depth is
   sub-z `f32`; integer `surface_z`/`bed_z` derived from it.
3. `surface_z_at` is read by ~60 sites/~40 files — meaning must NOT change; `ground_z_at` additive;
   every caller audited & classified (Phase 2).
4. Full hydrology rewrite moves every river → locked tests (`rivers_reach_oceans`,
   `ocean_fraction_within_band`) + settlement scoring re-baselined deliberately.
5. Dams mirror the shipped Bridge pipeline (`construction.rs`: `Bridge`/`BridgeMap`/finalize ~6226,
   `is_water_anchored`, `work_stand_for_bridge`, `restore_tile`) — reuse, don't reinvent.

---

## Phase 0 — Audit & guardrails (no behavior change)

- Find whether any system restamps `Bridge`/`Wall`/`Well` tiles after chunk regen
  (`chunk_streaming.rs`/`construction.rs`, `BridgeMap`/`WallMap` on `ChunkLoadedEvent`). Document
  the bridge-reverts-on-reload gap → Phase 3 fixes it generally.
- `surface_z_at` caller-classification table (~60 sites): each `TOP_SURFACE_UNCHANGED` or
  `NEEDS_GROUND_Z`. Into `src/world/CLAUDE.md`.
- Enumerate `water_kind_at` sites (`drink.rs`, `animals.rs`, `typed_task.rs`, `biome.rs`,
  `tile.rs`) + `tile.rs` water-helper contracts.
- Verify `cargo test --bin civgame` unchanged. Deliverable = the tables.

## Phase 1 — Worldgen hydrology truth (full geometry overhaul, pure)

Rewrite hydrology block of `generate_globe` (`globe.rs` ~352) + `hydrology.rs`. Pure, Bevy-free.

- `Globe.hydrology: HydrologyMap { cells: Vec<HydroCell>, reservoirs: Vec<Reservoir> }`.
- `HydroCell { raw_height:f32, filled_height:f32, flow_to:u32, discharge:f32,
  reservoir_id:u32, aquifer_level:f32 }` — keep `pre_fill_height` (`globe.rs:354`) → `raw_height`.
- `ReservoirKind { Ocean, Lake, Wetland, Spring, Endorheic }` (`Dam` runtime, Phase 4).
- `Reservoir { id, kind, spill_level:f32, outlet_cell:u32, salinity:f32 }`.
- Extend `RiverEdge` (keep fields): `discharge:f32, order:u8, from_level:f32, to_level:f32,
  from_depth:f32, to_depth:f32, reservoir_id:u32`.
- Pure fns: `weighted_discharge` (rainfall-driven, not cell count), `strahler_order`,
  `classify_reservoirs` (basin clusters not discs; Endorheic = closed → evaporative salinity),
  `solve_levels` (monotone downstream), `aquifer_table` (≤ filled except wetland/spring; rock
  doesn't flood by default); estuary brackish gradient at river→ocean.
- Shared accessors `Globe::water_level_at/reservoir_at/salinity_at` — used by chunk stamping AND
  world-map overlay (no parallel formulas).
- Bump `GLOBE_FILE_VERSION` 7→8 (auto-regenerates; no migration).
- Re-baseline locked tests + re-tune `score_home_candidate`/`frontier_score`/`parcel_suitability`/
  nomad `score_water`/herd `nearest_water`, each with justifying comment.

## Phase 2 — Chunk water columns + `ground_z_at`

`surface_z_at` unchanged (= rendered top of column).

- `Chunk`: `surface_ground_z:[[i8;32];32]`, `surface_water_depth:[[f32;32];32]`,
  `surface_reservoir_id:[[u32;32];32]`. Dry: ground==surface, depth==0, id==MAX (superset).
  Back-compat `Chunk::new` derives ground=surface.
- `ChunkMap::ground_z_at/water_depth_at/water_level_at/reservoir_id_at`; `water_column_at` →
  `WaterColumn { level_z:i8, bed_z:i8, depth:f32, kind, reservoir_id, salinity, flow_dir }`.
- `generate_chunk_from_globe` passes 2/4 consult `globe.hydrology`: river `ground_z=bed`,
  `depth=edge depth`, `surface_z = bed + ceil(depth/TILE_M)` (replaces `cur-1` in `diamond_stamp`);
  reservoirs stamped by basin membership + spill_level; ocean constant sea level; banks shaped from
  river context. `tile_at_3d` reports depth.
- Migrate only `NEEDS_GROUND_Z` callers; document unchanged ones in `src/world/CLAUDE.md`.

## Phase 3 — Persistent runtime water layer

- `RuntimeWater` Resource: `AHashMap<(i32,i32), RuntimeWaterCell { ground_z:i8, depth:f32,
  reservoir_id:u32, salinity:f32, source_rate:f32 }>` + `runtime_reservoirs: Vec<Reservoir>`.
  Keyed by world tile, NOT on `Chunk`.
- `restamp_runtime_water_on_chunk_load`: on `ChunkLoadedEvent`/after regen, overlay `RuntimeWater`
  + emit `TileChangedEvent`; generalize to restamp `Bridge`/`Dam`/`Well` (fixes Phase 0 gap).
- Entities are durable truth; `RuntimeWater` rebuildable from `DamMap`/dig-history.

## Phase 4 — Dams (mirror shipped Bridge pipeline)

- `tile.rs`: `TileKind::Dam = 25` (variant-count comment + `world/CLAUDE.md`). `is_passable=true`,
  `is_floor=true`, `is_water_like=false`, `is_freshwater=false`, `is_drinkable_candidate=false`,
  `is_stone_like=false`. Tests mirror `bridge_is_passable_floor_waterlike`.
- `construction.rs`: `BuildSiteKind::Dam` + label; `is_water_anchored()` true (reuse
  `work_stand_for_bridge`); recipe stone+wood gated on existing `BRIDGE_BUILDING` (dedicated tech =
  v2); `DamMap` mirrors `BridgeMap`; `Dam { faction_id, tile, restore_tile }`. Finalize (~6226):
  set tile, spawn entity, `DamMap`, register barrier in `RuntimeWater` at crest = dam z, emit
  `TileChangedEvent`. Deconstruct restores + removes barrier + refunds.
- Right-click "Build Dam" on water tile (mirror Bridge UI in `ui/orders.rs`).

## Phase 5 — Background fluid simulation

Virtual-pipe shallow-water model: active cells exchange flux with cardinal neighbors ∝ water-surface
height diff, clamped by volume → conserves volume, fills basins to spill, overtops/breaches dams.

- Run solver on `AsyncComputeTaskPool`: snapshot active set + boundaries → task integrates N
  substeps → double-buffer → poll-apply next frame, emit `TileChangedEvent` only on
  kind/passability flip. Main tick never blocks.
- Ocean/big lakes = fixed-level equilibrium boundaries (not per-cell). Unloaded neighbors = ghost
  boundaries at hydrology truth. At-rest cells sleep.
- Sources/sinks from `HydroCell` springs/discharge; aquifer cells exposed by digging below
  `aquifer_level` = bounded sources (no rock flooding).
- Hysteresis: kind/passability flips at quantized depth thresholds with deadband.
- Writes `RuntimeWater` → persistent automatically.
- Files: `src/world/water.rs` (pure core), `src/world/water_runtime.rs` (task wrapper, new).

## Phase 6 — Gameplay integration

- `water_kind_at` (`biome.rs:21`) signature byte-identical; body consults `Globe.salinity_at`/
  reservoir salinity (fresh/brackish/salt) + biome fallback. Brackish non-drinkable.
- Wells: derive `yield_remaining`/`depth`/`aquifer_id` from `HydroCell.aquifer_level`; dry well →
  graceful drink fail + water-access pressure.
- Fertility/settlement/nomad/herd/animal read new hydrology via re-tuned constants.
- Pathfinding unchanged (rides `TileChangedEvent`); `Dam` passable, `Bridge` passable water-like.

## Phase 7 — Documentation

- `src/world/CLAUDE.md`: hydrology fields, `ground_z_at` vs `surface_z_at` audit table, water-column
  cache, `RuntimeWater`/restamp, `Dam`, version 8, background sim.
- `src/simulation/CLAUDE.md`: dam pipeline, aquifer wells, salinity drinking.
- Root `CLAUDE.md`: `Dam` palette entry + count.

---

## Critical files

`src/world/hydrology.rs` (pure truth + solver core in new `water.rs`), `src/world/globe.rs`
(`HydrologyMap`, version, shared accessors), `src/world/terrain.rs` (`generate_chunk_from_globe`,
`diamond_stamp`, `tile_at_3d`), `src/world/{chunk,chunk_streaming}.rs` (caches, accessors,
restamp), `src/world/water.rs` *(new)*, `src/world/water_runtime.rs` *(new, task wrapper)*,
`src/simulation/construction.rs` (`Dam` mirroring `Bridge`), `src/world/tile.rs`
(`TileKind::Dam`), `src/world/biome.rs` (`water_kind_at` body), `src/simulation/drink.rs`
(aquifer wells), `src/ui/orders.rs`, `src/rendering/{color_map,sprite_library}.rs`, `CLAUDE.md`
trio.

## Verification

- Every phase: `cargo test --bin civgame` green (pure hydrology/water tests need no App; only
  construction/streaming-survival use the headless `test_fixture.rs` App).
- `cargo run` (no `--sandbox`): rivers terminate ocean/lake/wetland; endorheic salty; agents drink
  fresh/spring/well, reject salt/brackish; build dam → upstream floods/downstream drains in
  background without tick spike, pathing reroutes; pan 12+ chunks away & back — dam/reservoir/bridge
  persist; deconstruct dam → drains/restores; world-map overlay matches gameplay.
- Re-baselined locked tests carry a justifying comment.

## Deferred (v2, actionable)

- Dedicated `DAM_BUILDING` tech/civic gate (v1 reuses `BRIDGE_BUILDING`) — add tech in
  `technology.rs`, swap the recipe `tech_gate` + `faction_can_build` gate in `ui/orders.rs`.
- **Spring/aquifer-from-digging sources** (Phase 5 stubbed): in `spawn_water_sim_task_system`, set
  `WaterCell.source` from `globe.hydro_cell_at(t).discharge`/spring flag, and from dug cells whose
  floor Z < `HydroCell.aquifer_level` (bounded inflow, no rock flooding). Hook point is the per-tile
  classify loop; `WaterCell::with_source` already exists.
- **Per-cell flow routing** to replace the highest-boundary-inlet heuristic — walk
  `RiverNetwork.edge_polylines` clipped to the active region to place inlet/outlet exactly.
- Drowning, structural dam **breach** + flood damage, flood-evac AI, autonomous AI dam planning.
- Sediment transport / meander migration / seasonal discharge variation.
- Chunk-delta disk persistence (orthogonal; `RuntimeWater` rebuilds from entities for now).
