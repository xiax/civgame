//! Hydrology: pit-fill, D8 flow, flow accumulation, river extraction.
//!
//! Operates on the climate-cell heightmap. Sea level is implicit at 0.0;
//! cells at-or-below sea level are treated as drainage sinks (the ocean
//! absorbs all flow without backing up).

use super::globe::{RiverEdge, RiverNetwork, GLOBE_HEIGHT, GLOBE_WIDTH};
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
    let mut stack: Vec<u32> = (0..(W * H) as u32).filter(|&i| indeg[i as usize] == 0).collect();
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
pub fn extract_rivers(
    height: &[f32],
    dirs: &[u32],
    accum: &[u32],
    min_accum: u32,
) -> RiverNetwork {
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
