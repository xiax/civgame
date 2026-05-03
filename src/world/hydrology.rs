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
/// Each river edge's `width` scales with the (log of) flow accumulation at
/// the upstream endpoint.
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
            let width = width_for_accum(accum[cur]);
            edges.push(RiverEdge { from, to, width });
            if visited_edge[next] {
                break;
            }
            cur = next;
        }
    }
    RiverNetwork { edges }
}

fn width_for_accum(accum: u32) -> u8 {
    // log-scale: 100→1, 1000→2, 10000→3, 100000→4
    let l = (accum as f32).max(1.0).log10();
    (l - 1.5).max(1.0).min(5.0) as u8
}

/// Quantise flow accumulation into the u16 field stored on `WorldCell`.
/// Saturates at u16::MAX. Divide-by-16 keeps small streams visible while
/// huge rivers don't overflow.
pub fn quantise_accum(accum: u32) -> u16 {
    (accum / 16).min(u16::MAX as u32) as u16
}
