//! Thermal + hydraulic erosion on the climate-cell heightmap.
//!
//! Operates on `Vec<f32>` of length `GLOBE_WIDTH * GLOBE_HEIGHT` (row-major).
//! Heights are in arbitrary normalised units in roughly [-1, 1] (sea level
//! ≈ 0). X wraps, Y clamps.

use super::globe::{GLOBE_HEIGHT, GLOBE_WIDTH};

const W: usize = GLOBE_WIDTH as usize;
const H: usize = GLOBE_HEIGHT as usize;

#[inline]
fn idx(gx: usize, gy: usize) -> usize {
    gy * W + gx
}

#[inline]
fn neighbours_4(gx: usize, gy: usize) -> [(usize, usize); 4] {
    let xm = (gx + W - 1) % W;
    let xp = (gx + 1) % W;
    let ym = gy.saturating_sub(1);
    let yp = (gy + 1).min(H - 1);
    [(xm, gy), (xp, gy), (gx, ym), (gx, yp)]
}

/// Talus-angle thermal erosion. For each cell, redistribute material to its
/// lowest 4-neighbour if the slope exceeds `talus`. Each iteration is a
/// single pass.
///
/// `talus` is a normalised slope threshold; ~0.05 produces visible smoothing.
/// `iters` ~ 20 is plenty for 256×128.
pub fn thermal(height: &mut [f32], talus: f32, iters: u32) {
    debug_assert_eq!(height.len(), W * H);
    let mut delta = vec![0.0f32; W * H];
    for _ in 0..iters {
        delta.fill(0.0);
        for gy in 0..H {
            for gx in 0..W {
                let h_self = height[idx(gx, gy)];
                let mut lowest_h = h_self;
                let mut lowest_pos = None;
                for (nx, ny) in neighbours_4(gx, gy) {
                    let h_n = height[idx(nx, ny)];
                    if h_n < lowest_h {
                        lowest_h = h_n;
                        lowest_pos = Some((nx, ny));
                    }
                }
                if let Some((nx, ny)) = lowest_pos {
                    let diff = h_self - lowest_h;
                    if diff > talus {
                        let move_amt = (diff - talus) * 0.5;
                        delta[idx(gx, gy)] -= move_amt;
                        delta[idx(nx, ny)] += move_amt;
                    }
                }
            }
        }
        for i in 0..height.len() {
            height[i] += delta[i];
        }
    }
}

/// Grid-based hydraulic erosion. Each iteration:
///  1. Pour `rain` water on every cell.
///  2. Each cell sends water + dissolved sediment to lower neighbours,
///     weighted by slope.
///  3. Where flow is heavy, additional sediment is dissolved (erosion);
///     where flow is light, sediment settles (deposition).
///
/// Iters ~ 40 produces visible carved valleys at 256×128 scale.
pub fn hydraulic(height: &mut [f32], iters: u32) {
    debug_assert_eq!(height.len(), W * H);
    let rain = 0.01f32;
    let solubility = 0.06f32;
    let evaporation = 0.10f32;
    let deposition = 0.02f32;

    let mut water = vec![0.0f32; W * H];
    let mut sediment = vec![0.0f32; W * H];
    let mut flow = vec![0.0f32; W * H];

    for _ in 0..iters {
        // 1. Rain.
        for w in water.iter_mut() {
            *w += rain;
        }

        // 2. Compute lateral flow: each cell distributes its water to lower
        //    neighbours weighted by elevation difference.
        flow.fill(0.0);
        for gy in 0..H {
            for gx in 0..W {
                let i = idx(gx, gy);
                let h_self = height[i] + water[i];
                let mut diffs: [f32; 4] = [0.0; 4];
                let mut total = 0.0f32;
                let neigh = neighbours_4(gx, gy);
                for (k, (nx, ny)) in neigh.iter().enumerate() {
                    let j = idx(*nx, *ny);
                    let h_n = height[j] + water[j];
                    let d = h_self - h_n;
                    if d > 0.0 {
                        diffs[k] = d;
                        total += d;
                    }
                }
                if total > 1e-6 {
                    let move_w = water[i].min(total * 0.5);
                    let sed_per_w = sediment[i] / water[i].max(1e-6);
                    for (k, (nx, ny)) in neigh.iter().enumerate() {
                        if diffs[k] <= 0.0 {
                            continue;
                        }
                        let frac = diffs[k] / total;
                        let dw = move_w * frac;
                        let j = idx(*nx, *ny);
                        water[i] -= dw;
                        water[j] += dw;
                        let ds = sed_per_w * dw;
                        sediment[i] -= ds;
                        sediment[j] += ds;
                        flow[i] += dw;
                    }
                }
            }
        }

        // 3. Erode high-flow cells, deposit low-flow cells.
        for i in 0..W * H {
            let capacity = solubility * flow[i];
            if sediment[i] < capacity {
                let erode = (capacity - sediment[i]).min(0.05);
                height[i] -= erode;
                sediment[i] += erode;
            } else {
                let deposit = (sediment[i] - capacity) * deposition;
                height[i] += deposit;
                sediment[i] -= deposit;
            }
        }

        // 4. Evaporate.
        for w in water.iter_mut() {
            *w *= 1.0 - evaporation;
        }
    }
}
