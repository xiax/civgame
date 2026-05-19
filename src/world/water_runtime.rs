//! Phase 3 — persistent runtime water layer.
//!
//! Chunks regenerate fresh from `Globe + seed` on every stream-in
//! (`chunk_streaming.rs` rebuilds caches and never re-applies deltas), so any
//! *runtime* change to a water column — a dam flooding upstream (Phase 4), an
//! aquifer cell exposed by digging, the background fluid sim's writes
//! (Phase 5) — would be destroyed the moment the player pans 12+ chunks away.
//! [`RuntimeWater`] holds that truth in a tile-keyed resource that outlives
//! the chunk; the durable entity maps (`BridgeMap`, `DamMap` in Phase 4) hold
//! the structural truth. [`restamp_runtime_water_on_chunk_load`] re-applies
//! both whenever a chunk streams back in — this also closes the Phase 0
//! bridge-reverts-on-reload gap generally (the `Bridge` tile delta was never
//! re-applied, so a bridged river silently reverted to `River` on reload
//! while the `Bridge` entity orphaned in `BridgeMap`).

use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use crate::simulation::construction::{BridgeMap, DamMap};
use crate::simulation::schedule::SimClock;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_HEIGHT, CHUNK_SIZE, Z_MAX, Z_MIN};
use crate::world::chunk_streaming::{ChunkLoadedEvent, TileCarvedEvent, TileChangedEvent};
use crate::world::globe::{EdgeCrossingKind, Globe, Reservoir, ReservoirKind};
use crate::world::seasons::Calendar;
use crate::world::terrain::{WorldGen, GLOBE_H_TO_Z};
use crate::world::tile::{TileData, TileKind};
use crate::world::water::{CellRole, WaterCell, WaterGrid};

/// One persistent runtime water cell, keyed by world tile in
/// [`RuntimeWater`]. Survives chunk unload/regeneration. `depth == 0` is
/// never stored — a drained cell is *removed* (see [`RuntimeWater::set`]) so
/// the regenerated dry terrain shows through and we never have to reconstruct
/// the original surface kind.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeWaterCell {
    /// Solid bed Z (the land/rock floor under the water).
    pub ground_z: i8,
    /// Water column depth in Z-units (sub-z `f32`). Always `> 0` when stored.
    pub depth: f32,
    /// Reservoir membership (`u32::MAX` = ad-hoc / none). Indexes
    /// [`RuntimeWater::runtime_reservoirs`] for dam pools (Phase 4).
    pub reservoir_id: u32,
    /// 0.0 fresh .. 1.0 sea-salt. Phase 6 reads this for drink gating.
    pub salinity: f32,
    /// Inflow rate (Z-units/tick) for Phase 5 sources/sinks. 0 = passive.
    pub source_rate: f32,
}

/// Persistent runtime water, keyed by **world tile** (NOT on `Chunk`, which
/// is rebuilt from `Globe + seed` on every stream-in). It is a cache of
/// derived state, not a save file: rebuildable from durable truth (`DamMap`
/// crests, dig-history exposing aquifer cells), so chunk-delta disk
/// persistence stays out of scope (see plan "Deferred").
#[derive(Resource, Default)]
pub struct RuntimeWater {
    pub cells: AHashMap<(i32, i32), RuntimeWaterCell>,
    /// Dam crest barriers (tile → crest Z). The Phase 5 fluid sim treats
    /// these cells as walls below the crest — flux through them is blocked
    /// and water pools upstream to `crest_z`. A *cache* of `DamMap` (the
    /// durable truth): `Dam` finalize registers it, deconstruct clears it,
    /// and it is rebuildable from `DamMap` alone.
    pub dam_crests: AHashMap<(i32, i32), i8>,
    /// Runtime-born reservoirs (dam pools — Phase 5 fills these by solving
    /// against `dam_crests`). Worldgen reservoirs live on
    /// `Globe.hydrology.reservoirs`; these are additive and indexed by
    /// `RuntimeWaterCell::reservoir_id`.
    pub runtime_reservoirs: Vec<Reservoir>,
}

impl RuntimeWater {
    /// Upsert a wet cell. `depth <= 0` *removes* the entry instead, so a
    /// drained tile reverts to natural terrain on the next chunk regen
    /// rather than persisting a zero-depth ghost.
    pub fn set(&mut self, tile: (i32, i32), cell: RuntimeWaterCell) {
        if cell.depth > 0.0 {
            self.cells.insert(tile, cell);
        } else {
            self.cells.remove(&tile);
        }
    }

    /// Register a dam barrier at `tile` with crest height `crest_z`. Called
    /// from `Dam` finalize; idempotent (re-stamp on chunk reload re-asserts).
    pub fn register_dam(&mut self, tile: (i32, i32), crest_z: i8) {
        self.dam_crests.insert(tile, crest_z);
    }

    /// Drop a dam barrier (deconstruct). The impounded upstream water is no
    /// longer held — the Phase 5 sim drains it on the next solve; until
    /// then the prior tile (restored by deconstruct) shows through.
    pub fn clear_dam(&mut self, tile: (i32, i32)) {
        self.dam_crests.remove(&tile);
    }
}

/// FixedUpdate, after `chunk_streaming_system`. For every chunk that loaded
/// this tick (`ChunkLoadedEvent` fires only on a fresh `generate_chunk_from_globe`),
/// re-apply the persistent runtime water columns and re-stamp tile-replacing
/// structures (`Bridge`, `Dam`) whose deltas were lost when the chunk
/// regenerated. Emits `TileChangedEvent` so the chunk-graph + sprites rebuild
/// (mirrors the bridge/dam-finalize emit path). The crest barrier itself
/// lives on `RuntimeWater` (not the chunk), so it survives reload without
/// re-registration — only the tile projection needs restamping.
pub fn restamp_runtime_water_on_chunk_load(
    mut events: EventReader<ChunkLoadedEvent>,
    mut chunk_map: ResMut<ChunkMap>,
    runtime_water: Res<RuntimeWater>,
    bridge_map: Res<BridgeMap>,
    dam_map: Res<DamMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
) {
    let loaded: AHashSet<ChunkCoord> = events.read().map(|e| e.coord).collect();
    if loaded.is_empty() {
        return;
    }

    let in_loaded = |tx: i32, ty: i32| {
        loaded.contains(&ChunkCoord(
            tx.div_euclid(CHUNK_SIZE as i32),
            ty.div_euclid(CHUNK_SIZE as i32),
        ))
    };

    // 1. Persistent runtime water columns. `cells` is sparse (only dam-
    //    flooded / sim-touched / aquifer-exposed tiles) — a single scan is
    //    cheaper than maintaining a per-chunk index until Phase 4/5 actually
    //    populate it.
    for (&(tx, ty), cell) in runtime_water.cells.iter() {
        if !in_loaded(tx, ty) {
            continue;
        }
        if chunk_map.apply_water_column(tx, ty, cell.ground_z, cell.depth, cell.reservoir_id) {
            tile_changed.send(TileChangedEvent { tx, ty });
        }
    }

    // 2. Tile-replacing structures. The entity is durable truth in
    //    `BridgeMap`/`DamMap`; the chunk lost the `Bridge`/`Dam` delta on
    //    regen (Phase 0 gap). Re-stamp at the regenerated column's surface
    //    so a bridged/dammed cell doesn't revert to `River`/`Water` on
    //    reload. `set_tile`'s dry-invariant re-assert matches the original
    //    finalize path exactly (it also went through `set_tile`), so this
    //    preserves live behaviour.
    let mut stamp = |tx: i32, ty: i32, kind: TileKind| {
        if !in_loaded(tx, ty) || chunk_map.tile_kind_at(tx, ty) == Some(kind) {
            return;
        }
        let surf_z = chunk_map.surface_z_at(tx, ty);
        if surf_z < Z_MIN {
            return;
        }
        chunk_map.set_tile(
            tx,
            ty,
            surf_z,
            TileData {
                kind,
                elevation: 0,
                fertility: 0,
                flags: 0b0001,
                ore: 0,
            },
        );
        tile_changed.send(TileChangedEvent { tx, ty });
    };
    for &(tx, ty) in bridge_map.0.keys() {
        stamp(tx, ty, TileKind::Bridge);
    }
    for &(tx, ty) in dam_map.0.keys() {
        stamp(tx, ty, TileKind::Dam);
    }
}

// ───────────────────────── Phase 5: background fluid sim ─────────────────

/// How high a dam holds water above its footing tile Z. `DamMap` stores the
/// footing (`bp.target_z` ≈ river surface — Phase 4's "crest = dam z"); the
/// barrier *rises* this much above it, which is what actually impounds a
/// visible reservoir. ≈ 4.5 m at 1.5 m/tile.
const DAM_RISE_Z: f32 = 3.0;
/// Chebyshev radius of the active region around each dam (tiles). Bounds
/// the off-thread grid so the cost stays flat regardless of map size.
const WATER_SIM_RADIUS: i32 = 28;
/// Ticks between task spawns (~1 game-second at 20 Hz). Mirrors the
/// pathfinding "one task in flight, accumulate between" cadence.
const WATER_SIM_CADENCE: u64 = 20;
const WATER_SUBSTEPS: u32 = 30;
const WATER_DT: f32 = 1.0;
/// Z-units/substep injected where a real river polyline enters the active
/// region (`RiverNetwork::edge_crossings_in_bbox` — true channel topology,
/// not the old highest-boundary-elevation guess). Scaled by the edge's
/// `discharge` and the seasonal snowmelt hydrograph.
const INLET_BASE_RATE: f32 = 0.035;
/// Z-units/substep seeped upward by a cell whose solid bed sits below the
/// local water table (`HydroCell.aquifer_level`) — covers both natural
/// springs and pits dug below the table. **Much** slower than a river inlet
/// (groundwater is not surface runoff), and only emitted while the pool is
/// still below the table (`bed + depth < aquifer_z`) so it can never flood
/// rock above the water table. `WATER_SUBSTEPS · this ≪ 1` keeps the
/// per-task overshoot negligible (re-clamped every snapshot) — no `water.rs`
/// core change, so the conservation/determinism tests stand.
const AQUIFER_SEEP_RATE: f32 = 0.004;
/// Wet/dry passability hysteresis (deadband) so a cell hovering near the
/// threshold doesn't flip kind — and spam pathfinding — every cadence.
const WET_ON: f32 = 0.5;
const WET_OFF: f32 = 0.2;

/// Per-tile sim output merged back on the main thread.
struct WaterTileOut {
    tile: (i32, i32),
    ground_z: i8,
    depth: f32,
    /// The snapshot's source rate for this cell (river inlet / aquifer
    /// seep). Persisted into `RuntimeWater.source_rate` so a spring-fed pool
    /// keeps re-seeding the bounded region across snapshots and survives
    /// chunk reload, and so chunk-retention can pin player-affected water.
    source: f32,
    /// Surface kind the chunk had *before* the sim — restored when the cell
    /// drains dry (so a temporarily-flooded land tile reverts correctly on
    /// the live chunk, not only on reload).
    orig_kind: TileKind,
}

struct WaterSimResult {
    out: Vec<WaterTileOut>,
}

/// In-flight fluid-sim future + cadence bookkeeping. One task at a time;
/// disturbances between spawns are picked up by the next snapshot (the
/// snapshot always reads current truth, so nothing is "missed").
#[derive(Resource, Default)]
pub struct WaterSim {
    task: Option<Task<WaterSimResult>>,
    last_spawn_tick: u64,
}

fn z_clampf(z: f32) -> f32 {
    z.clamp(Z_MIN as f32, Z_MAX as f32)
}

/// Hydrology-truth water-surface Z at a tile (globe units → Z via the single
/// `GLOBE_H_TO_Z` factor), else the bed.
fn hydro_surface_z(globe: &Globe, t: (i32, i32), bed: f32) -> f32 {
    match globe.water_level_at(t.0, t.1) {
        Some(h) => z_clampf(h * GLOBE_H_TO_Z),
        None => bed,
    }
}

/// PostUpdate. Snapshots the active region (union of `WATER_SIM_RADIUS`
/// boxes around every dam **and every persisted runtime/seep cell**) into a
/// [`WaterGrid`] and hands it to `AsyncComputeTaskPool`. River inflow is
/// placed at the true channel crossings via
/// `RiverNetwork::edge_crossings_in_bbox` (not the old highest-elevation
/// boundary guess); cells dug below the water table seep upward (capped at
/// the table — no rock flooding); all inflow follows the seasonal snowmelt
/// hydrograph. No dams **and** no runtime water ⇒ no work (self-terminating;
/// the static Phase 2 stamp handles undammed water). The main tick never
/// blocks.
pub fn spawn_water_sim_task_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    runtime_water: Res<RuntimeWater>,
    dam_map: Res<DamMap>,
    globe: Res<Globe>,
    gen: Res<WorldGen>,
    calendar: Res<Calendar>,
    mut sim: ResMut<WaterSim>,
) {
    if sim.task.is_some() {
        return;
    }
    if clock.tick < sim.last_spawn_tick.saturating_add(WATER_SIM_CADENCE) {
        return;
    }
    // Run while there are dams (impounding) OR leftover runtime water to
    // drain (e.g. the last dam was just deconstructed — the reservoir must
    // still drain down). Both empty ⇒ nothing to do; self-terminating.
    if dam_map.0.is_empty() && runtime_water.cells.is_empty() {
        return;
    }

    // Active-region tile set: union of boxes around each dam and around
    // every persisted runtime-water cell (so a draining impoundment with
    // no dam is still covered).
    let mut region: AHashSet<(i32, i32)> = AHashSet::new();
    let mut seed_box = |cx: i32, cy: i32, region: &mut AHashSet<(i32, i32)>| {
        for ty in (cy - WATER_SIM_RADIUS)..=(cy + WATER_SIM_RADIUS) {
            for tx in (cx - WATER_SIM_RADIUS)..=(cx + WATER_SIM_RADIUS) {
                region.insert((tx, ty));
            }
        }
    };
    for &(dx, dy) in dam_map.0.keys() {
        seed_box(dx, dy, &mut region);
    }
    for &(cx, cy) in runtime_water.cells.keys() {
        seed_box(cx, cy, &mut region);
    }

    let on_boundary = |t: (i32, i32)| -> bool {
        // A region cell is on the outer ring iff a cardinal neighbour fell
        // outside the union.
        [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .iter()
            .any(|(ddx, ddy)| !region.contains(&(t.0 + ddx, t.1 + ddy)))
    };

    let mut grid = WaterGrid::default();
    // Dam footing → weir crest = footing + rise.
    for (&t, &fid_e) in dam_map.0.iter() {
        let _ = fid_e;
        let footing = chunk_map.surface_z_at(t.0, t.1);
        let crest = if footing >= Z_MIN {
            footing as f32 + DAM_RISE_Z
        } else {
            DAM_RISE_Z
        };
        grid.dam_crests.insert(t, crest);
    }

    // Per-cell flow routing: classify every place a real river polyline
    // crosses the active region's bbox. An `Inlet` is where the channel
    // enters (inject that edge's discharge); an `Outlet` is where it leaves
    // (pin a stable outflow level). This replaces the old "highest boundary
    // watercourse = inlet" elevation heuristic with true topology.
    let (mut mnx, mut mny, mut mxx, mut mxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for &(x, y) in &region {
        mnx = mnx.min(x);
        mny = mny.min(y);
        mxx = mxx.max(x);
        mxy = mxy.max(y);
    }
    let mut inlet: AHashMap<(i32, i32), (f32, f32)> = AHashMap::new(); // (discharge, level)
    let mut outlet: AHashMap<(i32, i32), f32> = AHashMap::new(); // level
    for c in globe.rivers.edge_crossings_in_bbox((mnx, mny), (mxx, mxy)) {
        match c.kind {
            EdgeCrossingKind::Inlet => {
                let e = inlet.entry(c.tile).or_insert((0.0, c.level));
                e.0 = e.0.max(c.discharge);
                e.1 = e.1.max(c.level);
            }
            EdgeCrossingKind::Outlet => {
                let e = outlet.entry(c.tile).or_insert(c.level);
                *e = e.max(c.level);
            }
        }
    }

    // Snowmelt hydrograph: river inlets follow it in full; aquifer/spring
    // seep is *damped* (groundwater lags and buffers surface seasonality).
    let season_full = calendar.discharge_multiplier();
    let season_aq = 0.5 + 0.5 * season_full;

    for &t in &region {
        if grid.dam_crests.contains_key(&t) {
            continue; // dam tile is a barrier, not a cell
        }
        // Bed + current depth: RuntimeWater is truth; else the loaded chunk;
        // else skip (unloaded → closed wall, acceptable: a player-built dam
        // and its neighbourhood are loaded).
        let (bed, depth, orig_kind, loaded) =
            if let Some(rc) = runtime_water.cells.get(&t) {
                let k = chunk_map
                    .tile_kind_at(t.0, t.1)
                    .unwrap_or(TileKind::Water);
                (rc.ground_z as f32, rc.depth, k, true)
            } else if let Some(k) = chunk_map.tile_kind_at(t.0, t.1) {
                let g = chunk_map.ground_z_at(t.0, t.1);
                (g as f32, chunk_map.water_depth_at(t.0, t.1), k, g >= Z_MIN)
            } else {
                (0.0, 0.0, TileKind::Air, false)
            };
        if !loaded {
            continue;
        }

        let is_watercourse = depth > 0.0
            || matches!(orig_kind, TileKind::Water | TileKind::River | TileKind::Marsh);

        // Ocean / large standing water → fixed-level sink/source.
        let pinned_ocean = globe
            .reservoir_at(t.0, t.1)
            .map(|r| matches!(r.kind, ReservoirKind::Ocean | ReservoirKind::Lake))
            .unwrap_or(false);

        if pinned_ocean {
            let lvl = hydro_surface_z(&globe, t, bed + depth);
            grid.cells.insert(t, WaterCell::pinned(lvl));
            continue;
        }

        // River leaves the region here → stable pinned outflow.
        if let Some(&lvl) = outlet.get(&t) {
            let z = z_clampf(lvl * GLOBE_H_TO_Z).max(bed);
            grid.cells.insert(t, WaterCell::pinned(z));
            continue;
        }

        // River enters the region here → inject the edge's discharge
        // (seasonally scaled) at the channel level so a dam downstream
        // actually pools it.
        if let Some(&(discharge, lvl)) = inlet.get(&t) {
            let surf = z_clampf(lvl * GLOBE_H_TO_Z).max(bed + depth);
            let rate =
                INLET_BASE_RATE * (1.0 + (discharge / 256.0).min(2.0)) * season_full;
            grid.cells
                .insert(t, WaterCell::free(bed, (surf - bed).max(0.0)).with_source(rate));
            continue;
        }

        // Non-river boundary watercourse (marsh/lake fringe with no mapped
        // edge) → pin at its current surface so the interior stays stable.
        if on_boundary(t) && is_watercourse {
            let surf = bed + depth;
            grid.cells.insert(t, WaterCell::pinned(surf.max(bed)));
            continue;
        }

        // Interior free cell. Seep on **any** tile whose bed sits below the
        // per-tile natural water table — `natural_surface_z(tx,ty)` (jittered
        // identically to chunk-gen) minus `(filled - aquifer) · GLOBE_H_TO_Z`
        // (per-cell aquifer-depth-below-surface). Treats natural per-tile
        // depressions and dug pits identically (both are bed-below-table —
        // it'd be absurd for a dug pit to fill while a deeper natural hollow
        // next to it stays dry). Cap with the same per-tile table so the
        // pool never rises above its real local groundwater level (no rock
        // flooding). Damped seasonal (groundwater lags surface). Note: the
        // sim only sees tiles inside the active region (around dams + bootstrap
        // seed cells from `aquifer_seep_emitter_system`); natural depressions
        // outside any region are a chunk-gen-time static-stamping gap (the
        // hydrology classifier is per-climate-cell, blind to per-tile jitter),
        // unrelated to v2 and not addressed here.
        let mut cell = WaterCell::free(bed, depth);
        if let Some(h) = globe.hydro_cell_at(t.0, t.1) {
            // Per-cell water table: anchor on the jitter-free macro elevation
            // (same frame as surface_z). Per-tile lows below this gate are
            // genuinely below the table — natural depressions and dug pits both
            // seep through this single check, no asymmetry.
            let (elev_u, _, _) = globe.sample_climate(t.0, t.1);
            let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
            let cell_surface_z = Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32;
            let aquifer_depth_z = (h.filled_height - h.aquifer_level) * GLOBE_H_TO_Z;
            let cell_table_z = cell_surface_z - aquifer_depth_z;
            if bed < cell_table_z && bed + depth < cell_table_z {
                cell = cell.with_source(AQUIFER_SEEP_RATE * season_aq);
            }
        }
        grid.cells.insert(t, cell);
    }

    // Snapshot the pre-sim kinds for dry-back restoration.
    let orig: AHashMap<(i32, i32), TileKind> = grid
        .cells
        .keys()
        .filter(|t| grid.cells[t].role == CellRole::Free)
        .map(|&t| {
            (
                t,
                chunk_map.tile_kind_at(t.0, t.1).unwrap_or(TileKind::Water),
            )
        })
        .collect();

    sim.last_spawn_tick = clock.tick;
    let pool = AsyncComputeTaskPool::get();
    sim.task = Some(pool.spawn(async move {
        grid.simulate(WATER_SUBSTEPS, WATER_DT);
        let mut out = Vec::new();
        for (&t, c) in grid.cells.iter() {
            if c.role != CellRole::Free {
                continue;
            }
            out.push(WaterTileOut {
                tile: t,
                ground_z: z_clampf(c.bed).round() as i8,
                depth: c.depth,
                source: c.source,
                orig_kind: orig.get(&t).copied().unwrap_or(TileKind::Water),
            });
        }
        out.sort_by_key(|o| o.tile);
        WaterSimResult { out }
    }));
}

/// PreUpdate. Drains the finished sim into `RuntimeWater` (persistent — the
/// Phase 3 restamp carries it across chunk reloads automatically) and the
/// live `ChunkMap`. Emits `TileChangedEvent` **only when wet/dry
/// passability flips** (deadband `WET_ON`/`WET_OFF`) so pathfinding/sprites
/// rebuild on a real change, not on every depth jiggle.
pub fn poll_water_sim_task_system(
    mut sim: ResMut<WaterSim>,
    mut runtime_water: ResMut<RuntimeWater>,
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
) {
    let Some(t) = sim.task.as_mut() else {
        return;
    };
    let Some(result) = block_on(future::poll_once(t)) else {
        return; // still running
    };
    sim.task = None;

    for o in result.out {
        let (tx, ty) = o.tile;
        let before = chunk_map.tile_kind_at(tx, ty);
        let was_wet = before.map(|k| !k.is_passable()).unwrap_or(false);

        if o.depth >= WET_ON || (was_wet && o.depth > WET_OFF) {
            // Wet: persist + project. RuntimeWater is the durable truth.
            // `o.source` is this snapshot's classify verdict (re-derived per
            // tile from the natural table); no preservation needed since the
            // next snapshot re-derives independently from `Globe + seed`.
            runtime_water.set(
                (tx, ty),
                RuntimeWaterCell {
                    ground_z: o.ground_z,
                    depth: o.depth,
                    reservoir_id: u32::MAX,
                    salinity: 0.0,
                    source_rate: o.source,
                },
            );
            chunk_map.apply_water_column(tx, ty, o.ground_z, o.depth, u32::MAX);
            if !was_wet {
                tile_changed.send(TileChangedEvent { tx, ty });
            }
        } else if o.source > 0.0 {
            // Not visually wet, but classify decided this cell is still
            // source-fed (bed below the local table, pool not yet at the
            // cap). Keep it as a depth-carrying runtime cell — bypassing
            // `set`, which would drop a `depth <= 0` cell — so it survives
            // reload AND keeps the sim region covering it (otherwise an
            // isolated dug well would empty `runtime_water.cells`, the sim
            // would self-terminate, and the well would never refill). No
            // passability flip ⇒ no event.
            runtime_water.cells.insert(
                (tx, ty),
                RuntimeWaterCell {
                    ground_z: o.ground_z,
                    depth: o.depth.max(0.0),
                    reservoir_id: u32::MAX,
                    salinity: 0.0,
                    source_rate: o.source,
                },
            );
            if was_wet {
                // It just fell out of the wet band — restore the dry tile.
                if o.orig_kind.is_passable() {
                    let sz = chunk_map.surface_z_at(tx, ty);
                    if sz >= Z_MIN {
                        chunk_map.set_tile(
                            tx,
                            ty,
                            sz,
                            TileData {
                                kind: o.orig_kind,
                                ..Default::default()
                            },
                        );
                    }
                }
                tile_changed.send(TileChangedEvent { tx, ty });
            }
        } else {
            // Drained, no source: drop the runtime cell and restore the
            // natural tile on the live chunk (reload would also fix it via
            // the restamp gap, but the player is looking at it now).
            let had_runtime = runtime_water.cells.remove(&(tx, ty)).is_some();
            if was_wet && o.orig_kind.is_passable() {
                let sz = chunk_map.surface_z_at(tx, ty);
                if sz >= Z_MIN {
                    chunk_map.set_tile(
                        tx,
                        ty,
                        sz,
                        TileData {
                            kind: o.orig_kind,
                            ..Default::default()
                        },
                    );
                }
                tile_changed.send(TileChangedEvent { tx, ty });
            } else if had_runtime {
                tile_changed.send(TileChangedEvent { tx, ty });
            }
        }
    }
}

/// PostUpdate, before [`spawn_water_sim_task_system`]. Pure region-bootstrap:
/// on a real dig event ([`TileCarvedEvent`] from `dig_system`) that clears
/// the per-tile natural water table, insert a depth-0 runtime cell so the
/// sim's active region covers this excavation (without it, an isolated dug
/// well far from any dam would never make `runtime_water.cells` non-empty,
/// and the sim wouldn't run). The **source decision is not made here** —
/// `spawn_water_sim_task_system`'s classify loop re-derives it per-tile from
/// the natural table every snapshot, so natural depressions and dug pits
/// both seep uniformly inside any active region.
pub fn aquifer_seep_emitter_system(
    mut carved: EventReader<TileCarvedEvent>,
    globe: Res<Globe>,
    gen: Res<WorldGen>,
    mut runtime_water: ResMut<RuntimeWater>,
) {
    for ev in carved.read() {
        let t = (ev.tx, ev.ty);
        if let Some(existing) = runtime_water.cells.get(&t) {
            if existing.source_rate > 0.0 {
                continue; // already a tracked seep
            }
        }
        let Some(h) = globe.hydro_cell_at(t.0, t.1) else {
            continue;
        };
        // Per-cell water table (same gate as snapshot + Pass 4.5).
        let (elev_u, _, _) = globe.sample_climate(t.0, t.1);
        let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
        let cell_surface_z = Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32;
        let aquifer_depth_z = (h.filled_height - h.aquifer_level) * GLOBE_H_TO_Z;
        let cell_table_z = cell_surface_z - aquifer_depth_z;
        if (ev.new_floor_z as f32) < cell_table_z {
            runtime_water.cells.insert(
                t,
                RuntimeWaterCell {
                    ground_z: ev.new_floor_z as i8,
                    depth: 0.0,
                    reservoir_id: u32::MAX,
                    salinity: 0.0,
                    source_rate: AQUIFER_SEEP_RATE,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::Chunk;

    /// Build an `App` with just the restamp system + the resources/events it
    /// touches, plus a single freshly-regenerated chunk at (0,0).
    fn harness(surf_kind: TileKind, surf_z: i8) -> App {
        let mut app = App::new();
        app.add_event::<ChunkLoadedEvent>()
            .add_event::<TileChangedEvent>()
            .insert_resource(RuntimeWater::default())
            .insert_resource(BridgeMap::default())
            .insert_resource(DamMap::default());

        let mut chunk_map = ChunkMap::default();
        let z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let kind = Box::new([[surf_kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let fert = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        chunk_map
            .0
            .insert(ChunkCoord(0, 0), Chunk::new(z, kind, fert));
        app.insert_resource(chunk_map);

        app.add_systems(Update, restamp_runtime_water_on_chunk_load);
        app
    }

    fn drain_changed(app: &mut App) -> Vec<(i32, i32)> {
        app.world_mut()
            .resource_mut::<Events<TileChangedEvent>>()
            .drain()
            .map(|e| (e.tx, e.ty))
            .collect()
    }

    #[test]
    fn bridge_restamped_on_chunk_reload() {
        // Regenerated chunk shows the natural River; the durable Bridge
        // entity persisted in BridgeMap (Phase 0 gap = the tile reverted).
        let mut app = harness(TileKind::River, 3);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<BridgeMap>()
            .0
            .insert((5, 3), e);

        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();

        assert_eq!(
            app.world().resource::<ChunkMap>().tile_kind_at(5, 3),
            Some(TileKind::Bridge),
            "bridge tile must be re-stamped from BridgeMap on reload"
        );
        assert!(
            drain_changed(&mut app).contains(&(5, 3)),
            "restamp must emit TileChangedEvent so pathing/sprites rebuild"
        );

        // Second load (chunk already correct) is a no-op — no event churn.
        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();
        assert!(drain_changed(&mut app).is_empty());
    }

    #[test]
    fn runtime_water_overlay_on_chunk_reload() {
        // A dam-flooded land tile (Phase 4 will write these). The chunk
        // regenerates dry Grass; RuntimeWater must re-flood it.
        let mut app = harness(TileKind::Grass, 2);
        app.world_mut().resource_mut::<RuntimeWater>().set(
            (5, 3),
            RuntimeWaterCell {
                ground_z: 2,
                depth: 3.0,
                reservoir_id: 9,
                salinity: 0.0,
                source_rate: 0.0,
            },
        );

        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();

        let cm = app.world().resource::<ChunkMap>();
        assert_eq!(cm.tile_kind_at(5, 3), Some(TileKind::Water));
        assert_eq!(cm.ground_z_at(5, 3), 2);
        assert_eq!(cm.water_depth_at(5, 3), 3.0);
        assert_eq!(cm.reservoir_id_at(5, 3), 9);
        assert!(drain_changed(&mut app).contains(&(5, 3)));
    }

    #[test]
    fn dam_restamped_on_chunk_reload() {
        // Mirror of the bridge case: a dammed river regenerates as River;
        // the durable Dam entity in DamMap must re-stamp the cell to Dam.
        let mut app = harness(TileKind::River, 3);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<DamMap>().0.insert((7, 4), e);

        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();

        assert_eq!(
            app.world().resource::<ChunkMap>().tile_kind_at(7, 4),
            Some(TileKind::Dam),
            "dam tile must be re-stamped from DamMap on reload"
        );
        assert!(drain_changed(&mut app).contains(&(7, 4)));

        // Idempotent: re-load with the chunk already correct → no churn.
        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();
        assert!(drain_changed(&mut app).is_empty());
    }

    #[test]
    fn restamp_skips_chunks_that_did_not_load() {
        // Bridge entity in a chunk that *didn't* fire ChunkLoadedEvent must
        // not be touched (the chunk isn't even loaded here).
        let mut app = harness(TileKind::River, 3);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<BridgeMap>()
            .0
            .insert((900, 900), e); // far chunk, not loaded, not in event

        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();

        assert!(drain_changed(&mut app).is_empty());
    }
}
