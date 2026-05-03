//! Climate fields: temperature and orographic rainfall.
//!
//! Temperature is a simple latitude × elevation function. Rainfall layers
//! base noise with orographic precipitation: tracing a few cells upwind
//! along the prevailing wind, every elevation rise adds rain to the lee
//! side; every drop after a peak subtracts (rain shadow).

use super::globe::{GLOBE_HEIGHT, GLOBE_WIDTH};

const W: usize = GLOBE_WIDTH as usize;
const H: usize = GLOBE_HEIGHT as usize;

#[inline]
fn idx(gx: usize, gy: usize) -> usize {
    gy * W + gx
}

/// Prevailing wind direction at a given latitude. Returns `(dx_cells, _dy)`
/// where dx is +1 (eastward) or -1 (westward); dy stays 0 (we don't model
/// meridional advection).
///
/// Latitude bands (mirrored across the equator):
///  - 0..33%   (tropics)        → trade winds, westward (-1)
///  - 33..66%  (mid-latitudes)  → westerlies, eastward (+1)
///  - 66..100% (polar)          → polar easterlies, westward (-1)
pub fn wind_dx(gy: usize) -> i32 {
    let lat = ((gy as f32 + 0.5) - H as f32 * 0.5).abs() / (H as f32 * 0.5);
    if lat < 0.33 {
        -1
    } else if lat < 0.66 {
        1
    } else {
        -1
    }
}

/// Per-cell temperature in °C. `elev` is normalised in [0, 1] (the same
/// convention as `WorldCell.elevation as f32 / 255.0`).
pub fn temperature_c(gy: usize, elev: f32) -> i8 {
    let lat = ((gy as f32 + 0.5) - H as f32 * 0.5).abs() / (H as f32 * 0.5);
    let temp_f = (1.0 - lat * 0.55 - elev * 0.45).clamp(0.0, 1.0);
    (temp_f * 80.0 - 30.0) as i8
}

/// Apply orographic precipitation effects to a base rainfall field.
///
/// `base_rain[i]` and `elev[i]` are normalised in [0, 1]. The output is the
/// adjusted rainfall, also in [0, 1], reflecting wet upwind slopes and dry
/// rain-shadow lees.
///
/// Algorithm: for each cell, walk `lookahead` cells upwind. Each consecutive
/// rise in elevation adds rainfall (orographic lift); each drop after a peak
/// subtracts (descending air dries out).
pub fn orographic(base_rain: &[f32], elev: &[f32], lookahead: i32) -> Vec<f32> {
    debug_assert_eq!(base_rain.len(), W * H);
    debug_assert_eq!(elev.len(), W * H);
    let mut out = base_rain.to_vec();
    for gy in 0..H {
        let dx = wind_dx(gy);
        for gx in 0..W {
            let mut bonus = 0.0f32;
            let mut prev = elev[idx(gx, gy)];
            // Walk upwind: positions are (gx - dx*k) for k=1..=lookahead.
            // (Upwind = opposite of wind direction.)
            for k in 1..=lookahead {
                let ux = ((gx as i32 - dx * k).rem_euclid(W as i32)) as usize;
                let h = elev[idx(ux, gy)];
                let dh = prev - h; // positive if upwind is lower → wind climbing toward us
                // dh > 0: air rising as it approaches us → orographic boost
                // dh < 0: air descending → rain shadow
                bonus += dh * 0.45 / k as f32;
                prev = h;
            }
            out[idx(gx, gy)] = (out[idx(gx, gy)] + bonus).clamp(0.0, 1.0);
        }
    }
    out
}
