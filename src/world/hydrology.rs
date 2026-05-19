//! Hydrology: pit-fill, D8 flow, flow accumulation, river extraction.
//!
//! Operates on the climate-cell heightmap. Sea level is implicit at 0.0;
//! cells at-or-below sea level are treated as drainage sinks (the ocean
//! absorbs all flow without backing up).

use super::globe::{
    HydroCell, HydrologyMap, Reservoir, ReservoirKind, RiverEdge, RiverNetwork, GLOBE_HEIGHT,
    GLOBE_WIDTH,
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

const W: usize = GLOBE_WIDTH as usize;
const H: usize = GLOBE_HEIGHT as usize;

#[inline]
fn idx(gx: usize, gy: usize) -> usize {
    gy * W + gx
}

/// 8-connected neighbours with X-wrap, Y-clamp. Returns up to 8 (gx, gy).
fn neighbours_8(gx: usize, gy: usize, out: &mut [(usize, usize); 8]) -> usize {
    let xm = (gx + W - 1) % W;
    let xp = (gx + 1) % W;
    let mut n = 0;
    if gy > 0 {
        out[n] = (xm, gy - 1);
        n += 1;
        out[n] = (gx, gy - 1);
        n += 1;
        out[n] = (xp, gy - 1);
        n += 1;
    }
    out[n] = (xm, gy);
    n += 1;
    out[n] = (xp, gy);
    n += 1;
    if gy + 1 < H {
        out[n] = (xm, gy + 1);
        n += 1;
        out[n] = (gx, gy + 1);
        n += 1;
        out[n] = (xp, gy + 1);
        n += 1;
    }
    n
}

#[derive(Copy, Clone, PartialEq)]
struct PqEntry {
    h: f32,
    pos: (u16, u16),
}

impl Eq for PqEntry {}

impl PartialOrd for PqEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PqEntry {
    // Min-heap: smaller h first. BinaryHeap is max-heap by default, so reverse.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .h
            .partial_cmp(&self.h)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.pos.cmp(&other.pos))
    }
}

/// Priority-flood pit fill. After this, every land cell drains monotonically
/// to the ocean (or off the Y-clamped poles); no more closed basins.
pub fn pit_fill(height: &mut [f32]) {
    debug_assert_eq!(height.len(), W * H);

    let sea_level = 0.0f32;
    let mut visited = vec![false; W * H];
    let mut pq = BinaryHeap::<PqEntry>::with_capacity(W * 2 + H * 2);

    // Seed with all sea cells (h <= sea_level) and the top/bottom Y edges
    // (which act as drains since Y clamps).
    for gy in 0..H {
        for gx in 0..W {
            let h = height[idx(gx, gy)];
            let on_pole = gy == 0 || gy == H - 1;
            if h <= sea_level || on_pole {
                visited[idx(gx, gy)] = true;
                pq.push(PqEntry {
                    h,
                    pos: (gx as u16, gy as u16),
                });
            }
        }
    }

    let mut buf = [(0usize, 0usize); 8];
    while let Some(PqEntry {
        h: h_self,
        pos: (gx, gy),
    }) = pq.pop()
    {
        let n = neighbours_8(gx as usize, gy as usize, &mut buf);
        for &(nx, ny) in &buf[..n] {
            let j = idx(nx, ny);
            if visited[j] {
                continue;
            }
            visited[j] = true;
            // Raise neighbour to at least our height (plus epsilon) so it can
            // drain through us.
            let new_h = height[j].max(h_self + 1e-4);
            height[j] = new_h;
            pq.push(PqEntry {
                h: new_h,
                pos: (nx as u16, ny as u16),
            });
        }
    }
}

/// D8 flow direction: for each land cell, store the index of its steepest
/// downhill 8-neighbour (or itself if it's a sink — sea or pole).
pub fn flow_dirs(height: &[f32]) -> Vec<u32> {
    let mut dirs = vec![0u32; W * H];
    let mut buf = [(0usize, 0usize); 8];
    for gy in 0..H {
        for gx in 0..W {
            let i = idx(gx, gy);
            let h_self = height[i];
            if h_self <= 0.0 || gy == 0 || gy == H - 1 {
                dirs[i] = i as u32;
                continue;
            }
            let n = neighbours_8(gx, gy, &mut buf);
            let mut best = i;
            let mut best_drop = 0.0f32;
            for &(nx, ny) in &buf[..n] {
                let j = idx(nx, ny);
                let drop = h_self - height[j];
                if drop > best_drop {
                    best_drop = drop;
                    best = j;
                }
            }
            dirs[i] = best as u32;
        }
    }
    dirs
}

/// Flow accumulation: number of upstream cells whose drainage path passes
/// through this cell (inclusive). Counts itself as 1.
pub fn flow_accum(dirs: &[u32]) -> Vec<u32> {
    // Build in-degree (how many cells flow INTO each cell).
    let mut indeg = vec![0u32; W * H];
    for (i, &d) in dirs.iter().enumerate() {
        if d as usize != i {
            indeg[d as usize] += 1;
        }
    }
    // Topological order: start with leaves (indeg = 0) and propagate downstream.
    let mut accum = vec![1u32; W * H];
    let mut stack: Vec<u32> = (0..(W * H) as u32)
        .filter(|&i| indeg[i as usize] == 0)
        .collect();
    while let Some(i) = stack.pop() {
        let d = dirs[i as usize] as usize;
        if d == i as usize {
            continue;
        }
        accum[d] += accum[i as usize];
        indeg[d] -= 1;
        if indeg[d] == 0 {
            stack.push(d as u32);
        }
    }
    accum
}

/// Build river polylines by tracing flow paths from cells whose accumulation
/// crosses `min_accum`, downhill until they hit the sea or another river.
/// Each emitted edge carries `from_width` / `to_width` derived from the log
/// of flow accumulation at its endpoints, so the rasteriser can taper.
/// Polylines (curved tile paths) are populated separately at globe-gen time
/// — see `chaikin_river_path` below.
pub fn extract_rivers(height: &[f32], dirs: &[u32], accum: &[u32], min_accum: u32) -> RiverNetwork {
    let mut edges = Vec::new();
    let mut visited_edge = vec![false; W * H];
    for start in 0..(W * H) {
        if accum[start] < min_accum {
            continue;
        }
        if height[start] <= 0.0 {
            continue;
        }
        // Only emit edges starting at a cell whose upstream (any neighbour
        // that flows INTO it) is below threshold — i.e. we're at a river head.
        // Cheaper alternative: skip if any neighbour has higher accum AND
        // flows here. We fold both checks together by gating on visited_edge
        // and walking downstream until we re-enter visited territory.
        if visited_edge[start] {
            continue;
        }
        let mut cur = start;
        loop {
            visited_edge[cur] = true;
            let next = dirs[cur] as usize;
            if next == cur {
                break;
            }
            let from = ((cur % W) as u32, (cur / W) as u32);
            let to = ((next % W) as u32, (next / W) as u32);
            // Skip seam-wrap steps. The climate grid wraps in X for flow
            // routing, but the in-game tile grid does not — so a river edge
            // that crosses the X seam would rasterise as a giant horizontal
            // line across the entire map. Treat the seam as a sink (the
            // river just terminates here) instead of emitting the wrap edge.
            let dx = (to.0 as i32 - from.0 as i32).abs();
            if dx as usize > W / 2 {
                break;
            }
            let from_width = width_for_accum(accum[cur]);
            let to_width = width_for_accum(accum[next]);
            edges.push(RiverEdge {
                from,
                to,
                from_width,
                to_width,
                ..Default::default()
            });
            if visited_edge[next] {
                break;
            }
            cur = next;
        }
    }
    RiverNetwork {
        edges,
        edge_polylines: Vec::new(),
    }
}

pub fn width_for_accum(accum: u32) -> u8 {
    // log-scale: 100→1, 1000→2, 10000→3, 100000→4
    let l = (accum as f32).max(1.0).log10();
    (l - 1.5).max(1.0).min(5.0) as u8
}

/// Deterministic meandering tile polyline between two endpoints.
///
/// Inserts three perpendicular-jittered control points at t = 0.25 / 0.5 /
/// 0.75, then runs two passes of Chaikin corner-cutting to soften the
/// resulting kinks. The endpoints stay exact so successive river edges meet
/// at defined join cells. Jitter is hashed off `(world_seed, edge_idx, k)`
/// so the same seed always produces the same river paths.
///
/// Output: a polyline of tile coordinates suitable for piecewise Bresenham
/// rasterisation. ~17 points per edge after two Chaikin passes.
pub fn chaikin_river_path(
    ax: i32,
    ay: i32,
    bx: i32,
    by: i32,
    world_seed: u64,
    edge_idx: usize,
) -> Vec<(i32, i32)> {
    let dx = (bx - ax) as f32;
    let dy = (by - ay) as f32;
    let len = (dx * dx + dy * dy).sqrt();
    // Degenerate / very short edges: skip the meander entirely. A straight
    // segment 0..3 tiles long can't usefully bend without overshooting.
    if len < 4.0 {
        return vec![(ax, ay), (bx, by)];
    }
    // Perpendicular unit vector (rotate the tangent 90°).
    let inv_len = 1.0 / len;
    let px = -dy * inv_len;
    let py = dx * inv_len;
    let amplitude = (len * 0.18).clamp(2.0, 16.0);

    let hash = |k: u64| -> f32 {
        // Splitmix64-style hash → fold to f32 in [-1, 1].
        let mut x = world_seed
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (edge_idx as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9)
            ^ k.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let frac = (x as u32 as f32) / (u32::MAX as f32);
        frac * 2.0 - 1.0
    };

    // 5 control points: A, P25, P50, P75, B, with perpendicular offsets.
    let mut pts: Vec<(f32, f32)> = vec![(ax as f32, ay as f32)];
    for (k, t) in [(1u64, 0.25_f32), (2, 0.5), (3, 0.75)] {
        let cx = ax as f32 + dx * t;
        let cy = ay as f32 + dy * t;
        let off = hash(k) * amplitude;
        pts.push((cx + px * off, cy + py * off));
    }
    pts.push((bx as f32, by as f32));

    // Two passes of Chaikin corner-cutting. Endpoints preserved.
    for _ in 0..2 {
        let mut next: Vec<(f32, f32)> = Vec::with_capacity(pts.len() * 2);
        next.push(pts[0]);
        for i in 0..pts.len() - 1 {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[i + 1];
            // Q at 1/4, R at 3/4 along the segment.
            let qx = 0.75 * x0 + 0.25 * x1;
            let qy = 0.75 * y0 + 0.25 * y1;
            let rx = 0.25 * x0 + 0.75 * x1;
            let ry = 0.25 * y0 + 0.75 * y1;
            // Skip Q for the first segment (anchored at A).
            if i > 0 {
                next.push((qx, qy));
            }
            // Skip R for the last segment (anchored at B).
            if i + 1 < pts.len() - 1 {
                next.push((rx, ry));
            }
        }
        next.push(*pts.last().unwrap());
        pts = next;
    }

    pts.into_iter()
        .map(|(x, y)| (x.round() as i32, y.round() as i32))
        .collect()
}

/// Quantise flow accumulation into the u16 field stored on `WorldCell`.
/// Saturates at u16::MAX. Divide-by-16 keeps small streams visible while
/// huge rivers don't overflow.
pub fn quantise_accum(accum: u32) -> u16 {
    (accum / 16).min(u16::MAX as u32) as u16
}

// ───────────────────────── Hydrology truth layer ──────────────────────────
//
// Pure, deterministic, Bevy-free. `build_hydrology` is the orchestrator;
// `generate_globe` calls it after the existing extraction and derives the
// extended `RiverEdge` fields from the returned map. No river geometry moves
// in this layer — it only computes truth (discharge/order/levels/reservoirs/
// aquifer). Phase 2 stamps chunks from it.

/// Rainfall-weighted upstream accumulation (runoff proxy) — same topological
/// propagation as `flow_accum`, but each cell contributes its normalised
/// rainfall plus a small base so dry headwaters still trickle.
pub fn weighted_discharge(dirs: &[u32], rainfall_norm: &[f32]) -> Vec<f32> {
    let n = W * H;
    debug_assert_eq!(dirs.len(), n);
    debug_assert_eq!(rainfall_norm.len(), n);
    let mut indeg = vec![0u32; n];
    for (i, &d) in dirs.iter().enumerate() {
        if d as usize != i {
            indeg[d as usize] += 1;
        }
    }
    let mut q = vec![0.0f32; n];
    for i in 0..n {
        q[i] = rainfall_norm[i].clamp(0.0, 1.0) + 0.05;
    }
    let mut stack: Vec<u32> = (0..n as u32).filter(|&i| indeg[i as usize] == 0).collect();
    while let Some(i) = stack.pop() {
        let d = dirs[i as usize] as usize;
        if d == i as usize {
            continue;
        }
        q[d] += q[i as usize];
        indeg[d] -= 1;
        if indeg[d] == 0 {
            stack.push(d as u32);
        }
    }
    q
}

/// Strahler stream order. Processed in increasing-accumulation order
/// (guaranteed upstream-before-downstream since accumulation is monotone
/// non-decreasing downstream). A confluence of two equal-max tributaries
/// increments the order; otherwise the max is carried.
pub fn strahler_order(dirs: &[u32], accum: &[u32], min_accum: u32) -> Vec<u8> {
    let n = W * H;
    let mut order = vec![0u8; n];
    let mut river_cells: Vec<usize> = (0..n).filter(|&i| accum[i] >= min_accum).collect();
    river_cells.sort_by_key(|&i| accum[i]);
    let mut inflow: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &i in &river_cells {
        let d = dirs[i] as usize;
        if d != i && accum[d] >= min_accum {
            inflow[d].push(i);
        }
    }
    for &i in &river_cells {
        let ins: Vec<u8> = inflow[i]
            .iter()
            .map(|&u| order[u])
            .filter(|&o| o > 0)
            .collect();
        order[i] = if ins.is_empty() {
            1
        } else {
            let mx = *ins.iter().max().unwrap();
            let cnt = ins.iter().filter(|&&o| o == mx).count();
            if cnt >= 2 {
                mx.saturating_add(1)
            } else {
                mx
            }
        };
    }
    order
}

#[inline]
fn cluster_neighbours(i: usize, out: &mut [usize; 4]) -> usize {
    let gx = i % W;
    let gy = i / W;
    let xm = (gx + W - 1) % W;
    let xp = (gx + 1) % W;
    let mut k = 0;
    out[k] = gy * W + xm;
    k += 1;
    out[k] = gy * W + xp;
    k += 1;
    if gy > 0 {
        out[k] = (gy - 1) * W + gx;
        k += 1;
    }
    if gy + 1 < H {
        out[k] = (gy + 1) * W + gx;
        k += 1;
    }
    k
}

/// Classify standing water into reservoirs from the pit-fill delta.
///
/// Reservoir 0 is always the Ocean (every `filled <= 0` cell). Pit-filled
/// land cells (`filled - raw > eps`, `filled > 0`) cluster into basins; a
/// basin that spills to a lower outside neighbour is a `Lake` (or `Wetland`
/// if very shallow), one with no lower escape is `Endorheic` (evaporative →
/// brackish). Returns the reservoir table and a per-cell `reservoir_id`
/// (`u32::MAX` = dry / open drainage).
pub fn classify_reservoirs(raw: &[f32], filled: &[f32], dirs: &[u32]) -> (Vec<Reservoir>, Vec<u32>) {
    let n = W * H;
    debug_assert_eq!(raw.len(), n);
    debug_assert_eq!(filled.len(), n);
    let mut rid = vec![u32::MAX; n];
    let mut reservoirs: Vec<Reservoir> = Vec::new();

    // Reservoir 0 — the ocean.
    reservoirs.push(Reservoir {
        id: 0,
        kind: ReservoirKind::Ocean,
        spill_level: 0.0,
        outlet_cell: u32::MAX,
        salinity: 1.0,
    });
    for i in 0..n {
        if filled[i] <= 0.0 {
            rid[i] = 0;
        }
    }

    const EPS: f32 = 0.02;
    const SHALLOW: f32 = 0.05;
    let mut is_water = vec![false; n];
    for i in 0..n {
        if filled[i] > 0.0 && filled[i] - raw[i] > EPS {
            is_water[i] = true;
        }
    }

    let mut visited = vec![false; n];
    // Reusable membership stamp (cluster sequence number); avoids a per-cluster
    // HashSet alloc. `0` = not in current cluster.
    let mut stamp = vec![0u32; n];
    let mut seq = 0u32;
    let mut nbuf = [0usize; 4];
    for start in 0..n {
        if !is_water[start] || visited[start] {
            continue;
        }
        let mut stack = vec![start];
        let mut cluster = Vec::new();
        while let Some(i) = stack.pop() {
            if visited[i] {
                continue;
            }
            visited[i] = true;
            if !is_water[i] {
                continue;
            }
            cluster.push(i);
            let k = cluster_neighbours(i, &mut nbuf);
            for &nb in &nbuf[..k] {
                if !visited[nb] {
                    stack.push(nb);
                }
            }
        }
        if cluster.is_empty() {
            continue;
        }
        seq += 1;
        for &i in &cluster {
            stamp[i] = seq;
        }
        let mut spill = f32::MIN;
        let mut min_raw = f32::MAX;
        for &i in &cluster {
            spill = spill.max(filled[i]);
            min_raw = min_raw.min(raw[i]);
        }
        // Outlet: lowest outside neighbour the basin can spill to.
        let mut outlet = u32::MAX;
        let mut outlet_h = f32::MAX;
        for &i in &cluster {
            let k = cluster_neighbours(i, &mut nbuf);
            for &nb in &nbuf[..k] {
                if stamp[nb] == seq {
                    continue;
                }
                if filled[nb] < spill && filled[nb] < outlet_h {
                    outlet_h = filled[nb];
                    outlet = nb as u32;
                }
            }
        }
        let _ = dirs; // outlet derived from spill geometry, not D8 here
        let id = reservoirs.len() as u32;
        let (kind, salinity) = if outlet == u32::MAX {
            (ReservoirKind::Endorheic, 0.6) // closed basin → evaporative/brackish
        } else if spill - min_raw < SHALLOW {
            (ReservoirKind::Wetland, 0.0) // shallow, marshy
        } else {
            (ReservoirKind::Lake, 0.0)
        };
        reservoirs.push(Reservoir {
            id,
            kind,
            spill_level: spill,
            outlet_cell: outlet,
            salinity,
        });
        for &i in &cluster {
            rid[i] = id;
        }
    }

    (reservoirs, rid)
}

/// Local water-table height. Sits below the pit-filled surface — shallowest
/// in wet lowlands, deepest in dry highlands — and is pinned to the water
/// surface inside lake/wetland reservoirs.
///
/// Depth-to-water is calibrated against real groundwater tables at our 1.5 m
/// tile scale: ~1 Z (1.5 m) in saturated lowland to ~16 Z (24 m) in true arid.
/// The raw-frame depth (`filled - aquifer_level`) is later multiplied by
/// `GLOBE_H_TO_Z = 8` everywhere it crosses into Z (well shafts, fluid sim
/// seep gate, chunk-gen Pass 4.5), so 1..16 Z means raw 0.125..2.0.
pub fn aquifer_table(
    filled: &[f32],
    rainfall_norm: &[f32],
    rid: &[u32],
    reservoirs: &[Reservoir],
) -> Vec<f32> {
    let n = W * H;
    let mut a = vec![0.0f32; n];
    for i in 0..n {
        let r = rid[i];
        if let Some(res) = reservoirs.get(r as usize) {
            if !matches!(res.kind, ReservoirKind::Ocean) {
                a[i] = res.spill_level; // water table == lake/wetland surface
                continue;
            }
        }
        let s = filled[i];
        let wet = rainfall_norm[i].clamp(0.0, 1.0);
        // Raw depth: 0.0625 (saturated, ~0.5 Z ≈ 0.75 m — damp lowland) ..
        // 1.5 (true arid, ~12 Z ≈ 18 m). Multiplied by GLOBE_H_TO_Z=8
        // downstream. The wet end lets per-tile jitter (~±1.5 Z amplitude)
        // genuinely dip below the table in moist biomes; the arid end is
        // well past max jitter so deserts produce no spurious marshes.
        let depth = 0.0625 + 1.4375 * (1.0 - wet);
        a[i] = s - depth;
    }
    a
}

/// Orchestrator: assemble the full `HydrologyMap` from the existing extraction
/// products. `rainfall_norm` is per-cell rainfall in `[0,1]` (read from the
/// finalised `WorldCell.rainfall`). Pure & deterministic.
pub fn build_hydrology(
    raw: &[f32],
    filled: &[f32],
    dirs: &[u32],
    rainfall_norm: &[f32],
) -> HydrologyMap {
    let n = W * H;
    let discharge = weighted_discharge(dirs, rainfall_norm);
    let (reservoirs, rid) = classify_reservoirs(raw, filled, dirs);
    let aquifer = aquifer_table(filled, rainfall_norm, &rid, &reservoirs);
    let mut cells = Vec::with_capacity(n);
    for i in 0..n {
        cells.push(HydroCell {
            raw_height: raw[i],
            filled_height: filled[i],
            flow_to: dirs[i],
            discharge: discharge[i],
            reservoir_id: rid[i],
            aquifer_level: aquifer[i],
        });
    }
    HydrologyMap { cells, reservoirs }
}

/// Channel depth (globe height units, sub-z) from discharge. Log-scaled so a
/// trickle reads ~0.05 and a major river ~0.6.
pub fn depth_for_discharge(discharge: f32) -> f32 {
    (0.06 * discharge.max(0.0).ln_1p()).clamp(0.05, 0.6)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Builds the real globe and exercises the hydrology truth invariants
    // (mirrors the globe.rs integration-style tests; W*H pure arrays).
    fn real() -> super::super::globe::Globe {
        super::super::globe::generate_globe(42)
    }

    #[test]
    fn discharge_monotone_downstream() {
        let g = real();
        let hy = &g.hydrology;
        assert_eq!(hy.cells.len(), W * H);
        for (i, c) in hy.cells.iter().enumerate() {
            let d = c.flow_to as usize;
            if d != i {
                assert!(
                    hy.cells[d].discharge + 1e-3 >= c.discharge,
                    "discharge dropped downstream at {i}->{d}"
                );
            }
        }
    }

    #[test]
    fn river_levels_monotone_downstream() {
        let g = real();
        for e in &g.rivers.edges {
            assert!(
                e.to_level <= e.from_level + 1e-4,
                "river level rose downstream: {} -> {}",
                e.from_level,
                e.to_level
            );
        }
    }

    #[test]
    fn ocean_reservoir_is_salt_sea_level() {
        let g = real();
        let ocean = &g.hydrology.reservoirs[0];
        assert_eq!(ocean.kind, ReservoirKind::Ocean);
        assert_eq!(ocean.spill_level, 0.0);
        assert_eq!(ocean.salinity, 1.0);
    }

    #[test]
    fn reservoir_cells_share_spill_level() {
        let g = real();
        let hy = &g.hydrology;
        for (i, c) in hy.cells.iter().enumerate() {
            if let Some(r) = hy.reservoirs.get(c.reservoir_id as usize) {
                if r.kind != ReservoirKind::Ocean {
                    let lv = g
                        .water_level_at((i % W) as i32 * 64, (i / W) as i32 * 64)
                        .unwrap_or(f32::NAN);
                    assert!(
                        (lv - r.spill_level).abs() < 1e-3,
                        "reservoir member cell level != spill_level"
                    );
                }
            }
        }
    }

    #[test]
    fn endorheic_is_brackish() {
        let g = real();
        for r in &g.hydrology.reservoirs {
            if r.kind == ReservoirKind::Endorheic {
                assert!(r.salinity > 0.0, "endorheic basin should be brackish");
            }
            if r.kind == ReservoirKind::Lake || r.kind == ReservoirKind::Wetland {
                assert_eq!(r.salinity, 0.0, "open lake/wetland should be fresh");
            }
        }
    }

    #[test]
    fn aquifer_not_above_filled_surface_on_dry_land() {
        let g = real();
        let hy = &g.hydrology;
        for c in &hy.cells {
            if c.reservoir_id == u32::MAX {
                assert!(
                    c.aquifer_level <= c.filled_height + 1e-4,
                    "water table above terrain on dry land"
                );
            }
        }
    }

    #[test]
    fn strahler_increments_at_synthetic_confluence() {
        // Two order-1 tributaries (cells A, B) flow into C; C flows to D.
        let n = W * H;
        let mut dirs: Vec<u32> = (0..n as u32).collect(); // all sinks
        let a = idx(10, 10);
        let b = idx(12, 10);
        let c = idx(11, 11);
        let d = idx(11, 12);
        dirs[a] = c as u32;
        dirs[b] = c as u32;
        dirs[c] = d as u32;
        let mut accum = vec![0u32; n];
        accum[a] = 100;
        accum[b] = 100;
        accum[c] = 250;
        accum[d] = 300;
        let order = strahler_order(&dirs, &accum, 80);
        assert_eq!(order[a], 1);
        assert_eq!(order[b], 1);
        assert_eq!(order[c], 2, "confluence of two order-1 should be order-2");
        assert_eq!(order[d], 2, "single-parent carry keeps order-2");
    }
}
