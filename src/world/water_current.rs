//! `WaterCurrentField` — a derived, non-persistent per-tile water-current
//! map (Phase 3 of `plans/swimming.md`).
//!
//! The field makes water a real, navigable force: rivers flow in a
//! consistent downstream direction, still lakes don't. It is **derived**
//! truth — rebuilt from `Globe` hydrology + `ChunkMap` wetness on chunk
//! load, never serialised. Deterministic: a `River` tile's flow direction
//! comes straight from the globe's D8 `HydroCell.flow_to`, so it is stable
//! across reloads.
//!
//! v1 distinguishes `RiverChannel` (flowing) from `StillWater` (lakes /
//! marsh / dam pools — near-zero). Runtime water-surface-gradient flow
//! (`RuntimeFlow`) and current-aware pathfinding cost are deferred.

use crate::collections::AHashMap;
use bevy::prelude::*;

use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::{ChunkLoadedEvent, ChunkUnloadedEvent};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::tile::TileKind;

/// Where a tile's current comes from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CurrentSource {
    /// A flowing river channel — direction from globe D8 flow.
    RiverChannel,
    /// Still water — open lake, marsh, or impounded dam pool.
    StillWater,
}

/// Per-tile water current.
#[derive(Copy, Clone, Debug)]
pub struct CurrentVector {
    /// Unit flow direction in world axes (+y = north). `Vec2::ZERO` when
    /// still.
    pub dir: Vec2,
    /// Flow speed, `0.0..=1.0`. 0 for still water.
    pub speed: f32,
    pub source: CurrentSource,
}

impl CurrentVector {
    pub const STILL: CurrentVector = CurrentVector {
        dir: Vec2::ZERO,
        speed: 0.0,
        source: CurrentSource::StillWater,
    };

    /// Flow displacement vector — `dir * speed`. `Vec2::ZERO` for still
    /// water. Consumers (swimming, rendering) scale this further.
    pub fn flow(&self) -> Vec2 {
        self.dir * self.speed
    }
}

/// Derived per-tile current map. Non-persistent; rebuilt from chunk
/// load/unload events.
#[derive(Resource, Default)]
pub struct WaterCurrentField {
    cells: AHashMap<(i32, i32), CurrentVector>,
    /// Per-chunk tile lists so a chunk unload can drop exactly its cells.
    by_chunk: AHashMap<ChunkCoord, Vec<(i32, i32)>>,
    /// Chunks (re)built since the renderer last consumed the field —
    /// drained by `take_dirty` so the streak renderer re-spawns them.
    dirty: crate::collections::AHashSet<ChunkCoord>,
}

impl WaterCurrentField {
    /// Current at a world tile, or `None` if the tile is dry / unmapped.
    pub fn at(&self, tx: i32, ty: i32) -> Option<CurrentVector> {
        self.cells.get(&(tx, ty)).copied()
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Chunks that currently carry at least one wet tile.
    pub fn chunk_keys(&self) -> impl Iterator<Item = ChunkCoord> + '_ {
        self.by_chunk.keys().copied()
    }

    /// World tiles of one chunk that carry a current cell.
    pub fn chunk_tiles(&self, coord: ChunkCoord) -> Option<&[(i32, i32)]> {
        self.by_chunk.get(&coord).map(|v| v.as_slice())
    }

    /// Drain the set of chunks (re)built since the last call — the streak
    /// renderer re-spawns each so in-place rebuilds (e.g. a neighbour
    /// load shifting edge tangents) get fresh sprites.
    pub fn take_dirty(&mut self) -> Vec<ChunkCoord> {
        self.dirty.drain().collect()
    }
}

/// Downstream flow direction + speed for a `River` tile, from the globe's
/// D8 hydrology. `None` at a sink (no downstream cell).
pub fn river_flow_at(globe: &Globe, tx: i32, ty: i32) -> Option<(Vec2, f32)> {
    let hc = globe.hydro_cell_at(tx, ty)?;
    let tiles_per_cell = (GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;
    let gx = (tx as f32 / tiles_per_cell)
        .floor()
        .rem_euclid(GLOBE_WIDTH as f32) as i32;
    let gy = ((ty as f32 / tiles_per_cell).floor() as i32).clamp(0, GLOBE_HEIGHT - 1);
    let fidx = hc.flow_to as i32;
    let fx = fidx.rem_euclid(GLOBE_WIDTH);
    let fy = fidx.div_euclid(GLOBE_WIDTH);
    let mut dx = fx - gx;
    let dy = fy - gy;
    // D8 step — undo the X-axis wrap so |dx| ≤ 1.
    if dx > 1 {
        dx -= GLOBE_WIDTH;
    } else if dx < -1 {
        dx += GLOBE_WIDTH;
    }
    if dx == 0 && dy == 0 {
        return None; // drainage sink — no flow direction
    }
    let dir = Vec2::new(dx as f32, dy as f32).normalize_or_zero();
    if dir == Vec2::ZERO {
        return None;
    }
    // `discharge` is rainfall-weighted upstream accumulation; saturate it
    // into a 0.1..1.0 speed so even a small creek shows a faint current.
    let speed = (hc.discharge.max(0.0) / DISCHARGE_FULL_SPEED).clamp(0.1, 1.0);
    Some((dir, speed))
}

/// `discharge` at or above which a river runs at full current speed.
const DISCHARGE_FULL_SPEED: f32 = 500.0;

/// Chebyshev radius of the river-tile window the local-tangent PCA reads.
const RIVER_AXIS_RADIUS: i32 = 4;

/// Local flow tangent of the river *channel* at `(tx, ty)`, recovered by
/// principal-component analysis of the nearby `River` tiles — the long
/// axis of the local river band **is** the flow line, so this tracks the
/// river's curves and bends (unlike the climate-cell-coarse D8 drainage
/// direction). Returns an **unsigned** axis (`None` when the local river
/// patch is too small or too blob-like to have a clear axis); the caller
/// orients it downstream with the coarse drainage direction.
fn local_river_axis(chunk_map: &ChunkMap, tx: i32, ty: i32) -> Option<Vec2> {
    let mut pts: Vec<(f32, f32)> = Vec::new();
    let mut sx = 0.0f32;
    let mut sy = 0.0f32;
    for dy in -RIVER_AXIS_RADIUS..=RIVER_AXIS_RADIUS {
        for dx in -RIVER_AXIS_RADIUS..=RIVER_AXIS_RADIUS {
            if chunk_map.tile_kind_at(tx + dx, ty + dy) == Some(TileKind::River) {
                pts.push((dx as f32, dy as f32));
                sx += dx as f32;
                sy += dy as f32;
            }
        }
    }
    let n = pts.len() as f32;
    if n < 3.0 {
        return None;
    }
    let (mx, my) = (sx / n, sy / n);
    let (mut cxx, mut cxy, mut cyy) = (0.0f32, 0.0f32, 0.0f32);
    for (px, py) in pts {
        let (ex, ey) = (px - mx, py - my);
        cxx += ex * ex;
        cxy += ex * ey;
        cyy += ey * ey;
    }
    // Eigenvalues of the 2×2 covariance — reject a near-isotropic blob
    // (a river junction / wide pool) where no single axis dominates.
    let trace = cxx + cyy;
    let det = cxx * cyy - cxy * cxy;
    let disc = (trace * trace / 4.0 - det).max(0.0).sqrt();
    let l1 = trace / 2.0 + disc;
    let l2 = trace / 2.0 - disc;
    if l1 <= 1e-3 || l2 / l1 > 0.7 {
        return None;
    }
    // Dominant-eigenvector angle of the symmetric covariance matrix.
    let angle = 0.5 * (2.0 * cxy).atan2(cxx - cyy);
    Some(Vec2::new(angle.cos(), angle.sin()))
}

/// Classify one wet tile. `River` tiles flow along the **local channel
/// tangent** (PCA of nearby river tiles — follows curves), oriented
/// downstream by the coarse D8 drainage direction. Every other wet tile
/// (open `Water`, `Marsh`, dam pool) reads still.
fn current_for_tile(globe: &Globe, chunk_map: &ChunkMap, tx: i32, ty: i32) -> Option<CurrentVector> {
    if chunk_map.water_depth_at(tx, ty) <= 0.0 {
        return None;
    }
    match chunk_map.tile_kind_at(tx, ty) {
        Some(TileKind::River) => {
            let coarse = river_flow_at(globe, tx, ty);
            let local = local_river_axis(chunk_map, tx, ty);
            let resolved = match (local, coarse) {
                // Local tangent oriented downstream by the coarse drainage.
                (Some(axis), Some((cd, speed))) => {
                    let dir = if axis.dot(cd) < 0.0 { -axis } else { axis };
                    Some((dir, speed))
                }
                // No coarse sense (drainage sink) — use the unsigned axis.
                (Some(axis), None) => Some((axis, 0.3)),
                // No clear local channel — fall back to coarse drainage.
                (None, c) => c,
            };
            match resolved {
                Some((dir, speed)) => Some(CurrentVector {
                    dir,
                    speed,
                    source: CurrentSource::RiverChannel,
                }),
                None => Some(CurrentVector::STILL),
            }
        }
        Some(_) => Some(CurrentVector::STILL),
        None => None,
    }
}

/// Rebuild the current field for chunks that just loaded / unloaded.
/// Deterministic — a chunk produces the same cells every time it loads.
pub fn water_current_field_system(
    globe: Res<Globe>,
    chunk_map: Res<ChunkMap>,
    mut field: ResMut<WaterCurrentField>,
    mut loaded: EventReader<ChunkLoadedEvent>,
    mut unloaded: EventReader<ChunkUnloadedEvent>,
    mut bootstrapped: Local<bool>,
) {
    // First run: the spawn-area chunks were pre-generated into `ChunkMap`
    // at Startup and never emitted a `ChunkLoadedEvent`, so build them all
    // once here. Subsequent chunks come through the event readers.
    if !*bootstrapped {
        *bootstrapped = true;
        let coords: Vec<ChunkCoord> = chunk_map.0.keys().copied().collect();
        for coord in coords {
            build_chunk(&globe, &chunk_map, &mut field, coord);
        }
    }
    for ev in unloaded.read() {
        field.dirty.remove(&ev.coord);
        if let Some(tiles) = field.by_chunk.remove(&ev.coord) {
            for tile in tiles {
                field.cells.remove(&tile);
            }
        }
    }
    for ev in loaded.read() {
        // Rebuild the loaded chunk plus its cardinal neighbours: the
        // local-tangent PCA reads a window that crosses chunk borders, so
        // a neighbour's edge tangents shift once this chunk's tiles exist.
        build_chunk(&globe, &chunk_map, &mut field, ev.coord);
        for nb in [
            ChunkCoord(ev.coord.0 - 1, ev.coord.1),
            ChunkCoord(ev.coord.0 + 1, ev.coord.1),
            ChunkCoord(ev.coord.0, ev.coord.1 - 1),
            ChunkCoord(ev.coord.0, ev.coord.1 + 1),
        ] {
            if chunk_map.0.contains_key(&nb) {
                build_chunk(&globe, &chunk_map, &mut field, nb);
            }
        }
    }
}

/// (Re)build the current cells for one chunk — idempotent (drops any stale
/// cells for the chunk first).
fn build_chunk(
    globe: &Globe,
    chunk_map: &ChunkMap,
    field: &mut WaterCurrentField,
    coord: ChunkCoord,
) {
    field.dirty.insert(coord);
    if let Some(prev) = field.by_chunk.remove(&coord) {
        for tile in prev {
            field.cells.remove(&tile);
        }
    }
    let mut tiles: Vec<(i32, i32)> = Vec::new();
    for ly in 0..CHUNK_SIZE as i32 {
        for lx in 0..CHUNK_SIZE as i32 {
            let tx = coord.0 * CHUNK_SIZE as i32 + lx;
            let ty = coord.1 * CHUNK_SIZE as i32 + ly;
            if let Some(cur) = current_for_tile(globe, chunk_map, tx, ty) {
                field.cells.insert((tx, ty), cur);
                tiles.push((tx, ty));
            }
        }
    }
    if !tiles.is_empty() {
        field.by_chunk.insert(coord, tiles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_water_has_zero_flow() {
        let s = CurrentVector::STILL;
        assert_eq!(s.flow(), Vec2::ZERO);
        assert_eq!(s.source, CurrentSource::StillWater);
    }

    #[test]
    fn river_flow_is_deterministic_for_a_seed() {
        // Two builds of the same globe yield identical flow at a tile.
        let g = crate::world::globe::generate_globe(12345);
        let a = river_flow_at(&g, 4000, 3000);
        let b = river_flow_at(&g, 4000, 3000);
        assert_eq!(a.map(|(d, s)| (d.x, d.y, s)), b.map(|(d, s)| (d.x, d.y, s)));
    }
}
