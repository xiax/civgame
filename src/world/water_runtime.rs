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

use std::sync::Arc;

use crate::collections::{AHashMap, AHashSet};
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
use crate::world::water::{CellRole, WaterCell, WaterGrid, REST_EPS};

/// One persistent runtime water cell, keyed by world tile in
/// [`RuntimeWater`]. Survives chunk unload/regeneration. `depth == 0` is
/// never stored — a drained cell is *removed* (see [`RuntimeWater::set`]) so
/// the regenerated dry terrain shows through and we never have to reconstruct
/// the original surface kind.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
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
/// Chebyshev radius of the active region around each dam (tiles). A dam's
/// reservoir + tailwater can extend far, so dams get the wide box.
const WATER_SIM_RADIUS: i32 = 28;
/// Chebyshev radius of the active region around each persisted runtime-water
/// cell. These are only ever *disturbed* cells now (dug wells, draining
/// reservoirs — see [`poll_water_sim_task_system`]); a dug well shaft needs
/// only a tiny active area, so the old wide `WATER_SIM_RADIUS` box around
/// every runtime cell was both overkill and the lever the region-blow-up bug
/// pushed on. A draining reservoir's cells still tile the whole reservoir at
/// this radius via their union.
const WATER_CELL_RADIUS: i32 = 3;
/// Hard cap on active-region tiles processed per cadence. If the union of all
/// seed boxes exceeds this (many dams), a bounded contiguous window is
/// simulated and the cursor rotates so every tile is covered within a few
/// cadences — keeps per-cadence cost flat. Defense-in-depth; rarely hit once
/// the region is seeded only by dams + genuinely-disturbed cells.
const WATER_SIM_MAX_REGION_TILES: usize = 30_000;
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
pub const AQUIFER_SEEP_RATE: f32 = 0.004;
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
    /// True when this cell is genuinely *perturbed* from chunk-gen truth — a
    /// dam-impoundment cell, or an already-tracked runtime cell (dug well /
    /// draining reservoir). Only disturbed cells are written back to
    /// `RuntimeWater` and seed the next snapshot's region; a natural marsh /
    /// depression that merely sits inside an active region is hydraulic
    /// context only and is discarded by `poll`. This is what stops the active
    /// region from snowballing across a whole wet biome.
    disturbed: bool,
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
    /// Immutable `Globe` cached for the off-thread classify pass. Cloned once
    /// on first use (`Globe` is fixed during `Playing`); a cheap `Arc::clone`
    /// feeds every async task thereafter — so no `Res<Globe>` call site
    /// elsewhere has to change.
    globe: Option<Arc<Globe>>,
    /// Rotating cursor into the sorted active region, used only when the
    /// region exceeds [`WATER_SIM_MAX_REGION_TILES`].
    region_cursor: usize,
}

/// Per-tile raw chunk data, snapshotted on the main thread so the per-tile
/// classify math can run off-thread. A flat copy — no computation.
#[derive(Clone, Copy)]
struct RawTile {
    kind: Option<TileKind>,
    /// `ChunkMap::ground_z_at` value — `< Z_MIN` when the tile is unloaded.
    ground_z: i32,
    water_depth: f32,
    /// `ChunkMap::surface_z_at` value — `< Z_MIN` when the tile is unloaded.
    surface_z: i32,
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

/// Flood-fill the cells each dam genuinely impounds: from every dam, the
/// connected `region` cells whose solid bed sits below *that dam's* crest —
/// the exact reservoir + tailwater extent. Cells outside the union are
/// free-flowing river: hydrology truth, pinned by the classify loop so the
/// virtual-pipe solver can't pile the injected discharge onto their banks.
/// `bed_at` returns the solid bed Z (`None` = unloaded ⇒ frontier stops).
fn dam_impoundment_set(
    region: &AHashSet<(i32, i32)>,
    dam_crests: &AHashMap<(i32, i32), f32>,
    bed_at: impl Fn((i32, i32)) -> Option<f32>,
) -> AHashSet<(i32, i32)> {
    const NEIGHBOURS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
    let mut impound: AHashSet<(i32, i32)> = AHashSet::default();
    for (&dam, &crest) in dam_crests.iter() {
        let mut visited: AHashSet<(i32, i32)> = AHashSet::default();
        let mut stack: Vec<(i32, i32)> = NEIGHBOURS
            .iter()
            .map(|(dx, dy)| (dam.0 + dx, dam.1 + dy))
            .collect();
        while let Some(t) = stack.pop() {
            if !region.contains(&t) || dam_crests.contains_key(&t) || !visited.insert(t) {
                continue;
            }
            // Terrain at/above the crest dams the water — frontier stops here.
            match bed_at(t) {
                Some(bed) if bed < crest => {}
                _ => continue,
            }
            impound.insert(t);
            for (dx, dy) in NEIGHBOURS {
                stack.push((t.0 + dx, t.1 + dy));
            }
        }
    }
    impound
}

/// PostUpdate. Builds the bounded active region, snapshots raw tile data into
/// a `Send` bundle, and hands the whole classify-and-simulate pass to
/// `AsyncComputeTaskPool`. The main thread does only flat `ChunkMap` reads (no
/// math) plus the region build — the per-tile classify (`sample_climate`,
/// hydrology lookups, the dam-impoundment flood-fill, river edge-crossing
/// routing) and the fluid sim itself both run off-thread against the snapshot
/// + an `Arc<Globe>`. No dams **and** no runtime water ⇒ no work
/// (self-terminating; the static Phase 2 stamp handles undammed water). The
/// main tick never blocks.
pub fn spawn_water_sim_task_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    runtime_water: Res<RuntimeWater>,
    dam_map: Res<DamMap>,
    globe: Res<Globe>,
    _gen: Res<WorldGen>,
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

    // Active-region tile set. Dams get a wide `WATER_SIM_RADIUS` box (the
    // reservoir + tailwater can extend far); every persisted runtime cell —
    // only ever a genuinely *disturbed* cell now (dug well / draining
    // reservoir; see `poll`) — gets a small `WATER_CELL_RADIUS` box. A wide
    // box around every runtime cell was the lever the region-blow-up bug
    // pushed on.
    let mut region: AHashSet<(i32, i32)> = AHashSet::default();
    let seed_box = |cx: i32, cy: i32, r: i32, region: &mut AHashSet<(i32, i32)>| {
        for ty in (cy - r)..=(cy + r) {
            for tx in (cx - r)..=(cx + r) {
                region.insert((tx, ty));
            }
        }
    };
    for &(dx, dy) in dam_map.0.keys() {
        seed_box(dx, dy, WATER_SIM_RADIUS, &mut region);
    }
    for &(cx, cy) in runtime_water.cells.keys() {
        seed_box(cx, cy, WATER_CELL_RADIUS, &mut region);
    }

    // Defense-in-depth: if the region is still pathologically large (many
    // dams), simulate a bounded contiguous window this cadence and rotate the
    // cursor so every tile is covered within a few cadences. Per-cell Pinned/
    // Free correctness holds for any subset — every snapshot re-reads truth.
    if region.len() > WATER_SIM_MAX_REGION_TILES {
        let mut sorted: Vec<(i32, i32)> = region.into_iter().collect();
        sorted.sort_unstable();
        let n = sorted.len();
        let start = sim.region_cursor % n;
        let mut window: AHashSet<(i32, i32)> =
            AHashSet::with_capacity_and_hasher(WATER_SIM_MAX_REGION_TILES, crate::collections::FixedState);
        for i in 0..WATER_SIM_MAX_REGION_TILES {
            window.insert(sorted[(start + i) % n]);
        }
        sim.region_cursor = (start + WATER_SIM_MAX_REGION_TILES) % n;
        region = window;
    } else {
        sim.region_cursor = 0;
    }

    // Raw-tile snapshot — flat `ChunkMap` reads, no math. Lets the classify
    // pass run entirely off-thread against this + the cached `Arc<Globe>`.
    let mut raw: AHashMap<(i32, i32), RawTile> = AHashMap::with_capacity_and_hasher(region.len(), crate::collections::FixedState);
    for &t in &region {
        raw.insert(
            t,
            RawTile {
                kind: chunk_map.tile_kind_at(t.0, t.1),
                ground_z: chunk_map.ground_z_at(t.0, t.1),
                water_depth: chunk_map.water_depth_at(t.0, t.1),
                surface_z: chunk_map.surface_z_at(t.0, t.1),
            },
        );
    }

    // Region-restricted runtime-cell snapshot (bounded by region size).
    let mut runtime_cells: AHashMap<(i32, i32), RuntimeWaterCell> = AHashMap::default();
    for &t in &region {
        if let Some(rc) = runtime_water.cells.get(&t) {
            runtime_cells.insert(t, *rc);
        }
    }

    let dam_tiles: Vec<(i32, i32)> = dam_map.0.keys().copied().collect();

    // Snowmelt hydrograph: river inlets follow it in full; aquifer/spring
    // seep is *damped* (groundwater lags and buffers surface seasonality).
    let season_full = calendar.discharge_multiplier();
    let season_aq = 0.5 + 0.5 * season_full;

    // Cache the immutable `Globe` once for off-thread use (cheap clone after).
    let globe_arc = sim
        .globe
        .get_or_insert_with(|| Arc::new(globe.clone()))
        .clone();

    sim.last_spawn_tick = clock.tick;
    let pool = AsyncComputeTaskPool::get();
    sim.task = Some(pool.spawn(async move {
        classify_and_simulate(
            &globe_arc,
            &region,
            &raw,
            &runtime_cells,
            &dam_tiles,
            season_full,
            season_aq,
        )
    }));
}

/// Off-thread: classify every region tile into a [`WaterGrid`] cell, run the
/// fluid sim, and collect the per-tile output. Reads only the `Send` snapshot
/// (`raw` + `Arc<Globe>` + cloned runtime cells / dam tiles) — no Bevy world
/// access — so the whole pass runs on `AsyncComputeTaskPool`.
fn classify_and_simulate(
    globe: &Globe,
    region: &AHashSet<(i32, i32)>,
    raw: &AHashMap<(i32, i32), RawTile>,
    runtime_cells: &AHashMap<(i32, i32), RuntimeWaterCell>,
    dam_tiles: &[(i32, i32)],
    season_full: f32,
    season_aq: f32,
) -> WaterSimResult {
    let on_boundary = |t: (i32, i32)| -> bool {
        // A region cell is on the outer ring iff a cardinal neighbour fell
        // outside the union.
        [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .iter()
            .any(|(ddx, ddy)| !region.contains(&(t.0 + ddx, t.1 + ddy)))
    };

    let mut grid = WaterGrid::default();
    // Dam footing → weir crest = footing + rise.
    for &t in dam_tiles {
        let Some(rt) = raw.get(&t) else {
            continue; // dam outside this cadence's window slice
        };
        let crest = if rt.surface_z >= Z_MIN {
            rt.surface_z as f32 + DAM_RISE_Z
        } else {
            DAM_RISE_Z
        };
        grid.dam_crests.insert(t, crest);
    }

    // Per-cell flow routing: classify every place a real river polyline
    // crosses the active region's bbox. An `Inlet` is where the channel
    // enters (inject that edge's discharge); an `Outlet` is where it leaves
    // (pin a stable outflow level).
    let (mut mnx, mut mny, mut mxx, mut mxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for &(x, y) in region {
        mnx = mnx.min(x);
        mny = mny.min(y);
        mxx = mxx.max(x);
        mxy = mxy.max(y);
    }
    let mut inlet: AHashMap<(i32, i32), (f32, f32)> = AHashMap::default(); // (discharge, level)
    let mut outlet: AHashMap<(i32, i32), f32> = AHashMap::default(); // level
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

    // Solid bed Z of a tile — `RuntimeWater` is truth, else the snapshot
    // (`None` when unloaded / out of region). Shared by the dam-impoundment
    // flood-fill and the classify loop so both read the same floor.
    let bed_at = |t: (i32, i32)| -> Option<f32> {
        if let Some(rc) = runtime_cells.get(&t) {
            Some(rc.ground_z as f32)
        } else if let Some(rt) = raw.get(&t) {
            (rt.ground_z >= Z_MIN).then_some(rt.ground_z as f32)
        } else {
            None
        }
    };

    // Dam-impoundment flood-fill. A free-flowing river is hydrology truth (a
    // fixed-level boundary, pinned below); a river cell free-evolves ONLY
    // where a dam genuinely backs water into it.
    let dam_impoundment = dam_impoundment_set(region, &grid.dam_crests, &bed_at);

    for &t in region {
        if grid.dam_crests.contains_key(&t) {
            continue; // dam tile is a barrier, not a cell
        }
        // Bed + current depth: RuntimeWater is truth; else the snapshot;
        // else skip (unloaded → closed wall, acceptable: a player-built dam
        // and its neighbourhood are loaded).
        let (bed, depth, orig_kind, loaded) = if let Some(rc) = runtime_cells.get(&t) {
            let k = raw.get(&t).and_then(|r| r.kind).unwrap_or(TileKind::Water);
            (rc.ground_z as f32, rc.depth, k, true)
        } else if let Some(rt) = raw.get(&t).copied() {
            match rt.kind {
                Some(k) => (
                    rt.ground_z as f32,
                    rt.water_depth,
                    k,
                    rt.ground_z >= Z_MIN,
                ),
                None => (0.0, 0.0, TileKind::Air, false),
            }
        } else {
            (0.0, 0.0, TileKind::Air, false)
        };
        if !loaded {
            continue;
        }

        let is_watercourse = depth > 0.0
            || matches!(
                orig_kind,
                TileKind::Water | TileKind::River | TileKind::Marsh
            );

        // Ocean / large standing water → fixed-level sink/source.
        let pinned_ocean = globe
            .reservoir_at(t.0, t.1)
            .map(|r| matches!(r.kind, ReservoirKind::Ocean | ReservoirKind::Lake))
            .unwrap_or(false);

        if pinned_ocean {
            let lvl = hydro_surface_z(globe, t, bed + depth);
            grid.cells.insert(t, WaterCell::pinned(lvl));
            continue;
        }

        // A free-flowing river's surface is hydrology truth — a fixed-level
        // boundary, like the ocean. Free-evolve a river cell ONLY where a
        // dam's impoundment backs water into it; elsewhere pin it at its
        // chunk-gen column surface so the virtual-pipe solver can't force an
        // unnatural surface gradient and pile water onto the banks. `orig_kind
        // == River` cleanly excludes well shafts / dug pits — they project as
        // `Water`, never `River` — so well seep is untouched.
        if orig_kind == TileKind::River && !dam_impoundment.contains(&t) {
            // Keep a cell Free while it still holds runtime water above its
            // natural column (a reservoir draining after dam removal) so it
            // drains through `poll` instead of orphaning a deep runtime cell.
            let natural_surf = (bed + depth).max(bed);
            let draining = runtime_cells
                .get(&t)
                .map(|rc| rc.ground_z as f32 + rc.depth > natural_surf + REST_EPS)
                .unwrap_or(false);
            if !draining {
                grid.cells.insert(t, WaterCell::pinned(natural_surf));
                continue;
            }
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
            let rate = INLET_BASE_RATE * (1.0 + (discharge / 256.0).min(2.0)) * season_full;
            grid.cells.insert(
                t,
                WaterCell::free(bed, (surf - bed).max(0.0)).with_source(rate),
            );
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
        // per-tile natural water table. Treats natural per-tile depressions
        // and dug pits identically; caps at the same per-tile table so the
        // pool never rises above local groundwater (no rock flooding).
        let mut cell = WaterCell::free(bed, depth);
        if let Some(h) = globe.hydro_cell_at(t.0, t.1) {
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
        .map(|&t| (t, raw.get(&t).and_then(|r| r.kind).unwrap_or(TileKind::Water)))
        .collect();

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
            // A cell is *disturbed* — genuinely perturbed from chunk-gen
            // truth — iff it is an already-tracked runtime cell (dug well /
            // draining reservoir) or it sits inside a dam's impoundment.
            // Only these are written back + re-seed the region; a natural
            // marsh that merely sits in an active region is discarded.
            disturbed: runtime_cells.contains_key(&t) || dam_impoundment.contains(&t),
        });
    }
    out.sort_by_key(|o| o.tile);
    WaterSimResult { out }
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
    deterministic: Option<Res<crate::simulation::perf::DeterministicCompute>>,
) {
    if sim.task.is_none() {
        return;
    }
    // Test mode: fully drain the task so its result lands this tick regardless
    // of compute-pool contention. Production keeps the non-blocking `poll_once`.
    let result = if deterministic.is_some() {
        block_on(sim.task.take().expect("task present"))
    } else {
        let t = sim.task.as_mut().expect("task present");
        let Some(result) = block_on(future::poll_once(t)) else {
            return; // still running
        };
        result
    };
    sim.task = None;

    for o in result.out {
        let (tx, ty) = o.tile;
        if !o.disturbed {
            // Hydraulic context only — a natural marsh / depression that
            // merely sits inside an active region. Chunk-gen owns its column
            // (Pass 4.5 restamps it on reload); persisting it here would make
            // it a fresh region seed next cadence and balloon the active
            // region across the whole wet biome. Discard the result.
            continue;
        }
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
            // cap). Update an *already-tracked* cell in place — a dug well
            // bootstrapped by `aquifer_seep_emitter_system`, or a draining
            // reservoir — so it keeps accumulating depth and stays a region
            // seed. **Never create a cell here:** a brand-new persistent cell
            // would become a fresh region seed next cadence and balloon the
            // active region without bound (the region-blow-up bug). A dug
            // well's cell always pre-exists via the carve-event bootstrap, so
            // `get_mut` finds it; an undisturbed natural tile correctly does
            // not persist. No passability flip ⇒ no event.
            if let Some(existing) = runtime_water.cells.get_mut(&(tx, ty)) {
                *existing = RuntimeWaterCell {
                    ground_z: o.ground_z,
                    depth: o.depth.max(0.0),
                    reservoir_id: u32::MAX,
                    salinity: 0.0,
                    source_rate: o.source,
                };
            }
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

    /// A `WATER_SIM_RADIUS`-style box of tiles around the origin.
    fn box_region(half: i32) -> AHashSet<(i32, i32)> {
        let mut r = AHashSet::default();
        for y in -half..=half {
            for x in -half..=half {
                r.insert((x, y));
            }
        }
        r
    }

    #[test]
    fn no_dam_means_empty_impoundment() {
        // With no dam, every river tile is left out of the impoundment ⇒ the
        // classify loop pins it at hydrology truth (no flooding).
        let region = box_region(5);
        let crests: AHashMap<(i32, i32), f32> = AHashMap::default();
        let impound = dam_impoundment_set(&region, &crests, |_| Some(0.0));
        assert!(impound.is_empty(), "no dam ⇒ nothing free-simulated");
    }

    #[test]
    fn dam_impoundment_floods_below_crest_and_stops_at_high_ground() {
        // A flat channel at bed 0, crest 3. A wall of bed-5 tiles at x=3
        // dams the fill — cells at x>=3 must be excluded.
        let region = box_region(6);
        let mut crests: AHashMap<(i32, i32), f32> = AHashMap::default();
        crests.insert((0, 0), 3.0);
        let bed_at = |t: (i32, i32)| -> Option<f32> {
            if t.0 == 3 {
                Some(5.0) // ridge at/above crest — frontier stops
            } else {
                Some(0.0)
            }
        };
        let impound = dam_impoundment_set(&region, &crests, bed_at);

        assert!(impound.contains(&(1, 0)), "cell below crest is impounded");
        assert!(impound.contains(&(2, 0)), "connected below-crest cell");
        assert!(impound.contains(&(-4, 2)), "fill reaches the far low ground");
        assert!(
            !impound.contains(&(3, 0)),
            "ridge at/above crest is not impounded"
        );
        assert!(
            !impound.contains(&(4, 0)),
            "low ground beyond the ridge is unreachable"
        );
        assert!(!impound.contains(&(0, 0)), "the dam tile itself is a barrier");
    }

    #[test]
    fn impoundment_is_bounded_by_the_region() {
        // A below-crest cell outside the active region is never pulled in.
        let region = box_region(2);
        let mut crests: AHashMap<(i32, i32), f32> = AHashMap::default();
        crests.insert((0, 0), 9.0);
        let impound = dam_impoundment_set(&region, &crests, |_| Some(0.0));
        assert!(impound.contains(&(2, 0)), "in-region below-crest cell");
        assert!(
            !impound.contains(&(3, 0)),
            "out-of-region cell must not be impounded"
        );
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
