//! Geomorphology layer — per-cell relief classification derived from the
//! finalised hydrology field. Drives per-tile detail amplitude, material
//! palette gating, fertility multipliers, settlement scoring, and world-map
//! tooltip.
//!
//! Built once in `globe::generate_globe` after `build_hydrology`, stored as
//! `Globe.relief: ReliefMap`, sampled per-tile via `Globe::sample_relief`.
//! Bumps `GLOBE_FILE_VERSION` whenever this layout changes.

use serde::{Deserialize, Serialize};

use super::globe::{
    HydrologyMap, ReservoirKind, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS,
};

/// Coarse landform classification used by terrain noise amp, palette gating,
/// fertility multiplier, settlement scoring, and the world-map tooltip.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ReliefClass {
    OceanShelf = 0,
    CoastalPlain = 1,
    Floodplain = 2,
    BasinWetland = 3,
    #[default]
    LowlandPlain = 4,
    RollingHills = 5,
    UplandPlateau = 6,
    Badlands = 7,
    Foothills = 8,
    MountainSlope = 9,
    MountainRidge = 10,
}

impl ReliefClass {
    pub fn name(self) -> &'static str {
        match self {
            ReliefClass::OceanShelf => "Ocean Shelf",
            ReliefClass::CoastalPlain => "Coastal Plain",
            ReliefClass::Floodplain => "Floodplain",
            ReliefClass::BasinWetland => "Basin Wetland",
            ReliefClass::LowlandPlain => "Lowland Plain",
            ReliefClass::RollingHills => "Rolling Hills",
            ReliefClass::UplandPlateau => "Upland Plateau",
            ReliefClass::Badlands => "Badlands",
            ReliefClass::Foothills => "Foothills",
            ReliefClass::MountainSlope => "Mountain Slope",
            ReliefClass::MountainRidge => "Mountain Ridge",
        }
    }

    /// Does this landform permit bare-rock outcrops? Plains and basins must
    /// not paint Stone/Granite even when surface noise crosses the high band.
    pub fn permits_stone_outcrop(self) -> bool {
        matches!(
            self,
            ReliefClass::UplandPlateau
                | ReliefClass::Badlands
                | ReliefClass::Foothills
                | ReliefClass::MountainSlope
                | ReliefClass::MountainRidge
        )
    }

    /// Per-class fertility multiplier — composes with `kind_fertility_factor`
    /// and `river_fertility_mult`. Floodplain wins for farmland; mountain
    /// slopes/ridges and ocean shelf are near-zero.
    pub fn fertility_mult(self) -> f32 {
        match self {
            ReliefClass::Floodplain => 1.30,
            ReliefClass::BasinWetland => 1.10,
            ReliefClass::CoastalPlain | ReliefClass::LowlandPlain => 1.10,
            ReliefClass::RollingHills => 1.00,
            ReliefClass::UplandPlateau => 0.85,
            ReliefClass::Foothills => 0.60,
            ReliefClass::Badlands => 0.40,
            ReliefClass::MountainSlope | ReliefClass::MountainRidge => 0.15,
            ReliefClass::OceanShelf => 0.0,
        }
    }

    /// True for landforms a settlement/founder picker should reject outright
    /// (mountain slopes/ridges can't support buildings; ocean shelf is wet).
    pub fn rejects_settlement(self) -> bool {
        matches!(
            self,
            ReliefClass::MountainSlope | ReliefClass::MountainRidge | ReliefClass::OceanShelf
        )
    }

    /// Soft scoring delta for settlement pickers (settled home + AI faction).
    /// `rejects_settlement` cells should be hard-rejected by the caller — this
    /// returns 0 for them so we don't double-count.
    pub fn settlement_score_bonus(self) -> i32 {
        match self {
            ReliefClass::LowlandPlain
            | ReliefClass::CoastalPlain
            | ReliefClass::Floodplain
            | ReliefClass::RollingHills => 40,
            ReliefClass::UplandPlateau => 10,
            ReliefClass::Foothills => -20,
            ReliefClass::BasinWetland | ReliefClass::Badlands => -40,
            ReliefClass::MountainSlope | ReliefClass::MountainRidge | ReliefClass::OceanShelf => 0,
        }
    }
}

/// Per-climate-cell relief diagnostics. Parallel to `Globe.cells` (row-major,
/// `GLOBE_WIDTH × GLOBE_HEIGHT`).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ReliefCell {
    /// `|∇filled_height|` via 3×3 Sobel, normalised by an arbitrary scale so a
    /// fully-flat cell reads 0 and a steep mountain face approaches 1.
    pub slope_norm: f32,
    /// `(max − min) filled_height` in 5×5 window, normalised against globe
    /// elevation span.
    pub local_relief: f32,
    /// Topographic position index (TPI): `(filled − mean_5x5) / max(local_relief, ε)`,
    /// in `[-1, 1]`. Positive = local high (ridge / hilltop); negative = local
    /// low (valley / basin floor).
    pub topographic_position: f32,
    /// Chebyshev distance (in cells) to nearest cell with `elev < OCEAN_ELEV_GATE`.
    pub coast_distance: u16,
    /// Chebyshev distance (in cells) to nearest cell with `elev > MOUNTAIN_ELEV_GATE`.
    pub mountain_distance: u16,
    /// `filled_height − aquifer_level` in globe height units. Low = water-table
    /// near surface (boggy lowland); high = deep table (arid upland).
    pub aquifer_depth_norm: f32,
    /// Cached classification for tooltip / scoring. Per-tile sampling
    /// re-derives this from interpolated numerics to avoid cell-seam patches.
    pub relief: ReliefClass,
}

/// Serialized geomorph layer. Lives on `Globe` alongside `HydrologyMap`.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct ReliefMap {
    /// `GLOBE_WIDTH × GLOBE_HEIGHT`, row-major. Empty when v9 cache loads —
    /// caller regenerates via the version-bump path.
    #[serde(default)]
    pub cells: Vec<ReliefCell>,
}

// ── Classification thresholds ────────────────────────────────────────────────
//
// Calibrated against `OCEAN_ELEV_GATE=0.22` / `MOUNTAIN_ELEV_GATE=0.82` and
// the 30%-ocean / 10%-mountain percentile remap. Numbers are in normalised
// units: `slope_norm` and `local_relief` 0..1, `elev_norm` 0..1.

const OCEAN_ELEV_GATE: f32 = 0.22;
const MOUNTAIN_ELEV_GATE: f32 = 0.82;

const PLAIN_SLOPE_MAX: f32 = 0.020;
const PLAIN_RELIEF_MAX: f32 = 0.045;
const ROLLING_SLOPE_MAX: f32 = 0.050;
const ROLLING_RELIEF_MAX: f32 = 0.100;
const PLATEAU_SLOPE_MAX: f32 = 0.025;
const PLATEAU_RELIEF_MAX: f32 = 0.050;
const PLATEAU_ELEV_MIN: f32 = 0.55;
const BADLANDS_SLOPE_MIN: f32 = 0.050;
const BADLANDS_RELIEF_MIN: f32 = 0.120;
const BADLANDS_RAIN_MAX: f32 = 0.25;
const FOOTHILLS_SLOPE_MIN: f32 = 0.040;
const MTN_SLOPE_SLOPE_MIN: f32 = 0.070;
const MTN_RIDGE_SLOPE_MIN: f32 = 0.100;
const MTN_RIDGE_TPI_MIN: f32 = 0.50;
const MTN_RIDGE_ELEV_MIN: f32 = 0.88;
const BASIN_AQUIFER_MAX: f32 = 0.05;
const BASIN_SLOPE_MAX: f32 = 0.010;
const COAST_DIST_MAX: u16 = 3;
const MOUNTAIN_NEIGHBOR_DIST: u16 = 2;
const FOOTHILLS_DIST_MAX: u16 = 5;

/// Floodplain shoulder width (in tiles) keyed on Strahler stream order.
/// Higher-order trunks have wider floodplains; minor headwaters have none.
pub fn floodplain_radius(strahler_order: u8) -> u8 {
    match strahler_order {
        0..=1 => 0,
        2 => 2,
        3 => 4,
        _ => 6,
    }
}

/// Classify a single cell from already-computed numerics. Pure.
pub fn classify(
    elev_norm: f32,
    slope_norm: f32,
    local_relief: f32,
    tpi: f32,
    coast_distance: u16,
    mountain_distance: u16,
    river_distance_cells: u32,
    strahler_order: u8,
    aquifer_depth_norm: f32,
    rain_norm: f32,
    reservoir_kind: Option<ReservoirKind>,
) -> ReliefClass {
    // Priority order: water bodies → basins → flood/coast → mountain ridge →
    // mountain slope → badlands → foothills → plateau → hills → plain.
    if matches!(reservoir_kind, Some(ReservoirKind::Ocean)) {
        return ReliefClass::OceanShelf;
    }
    if matches!(
        reservoir_kind,
        Some(ReservoirKind::Wetland) | Some(ReservoirKind::Endorheic)
    ) {
        return ReliefClass::BasinWetland;
    }
    if aquifer_depth_norm < BASIN_AQUIFER_MAX && slope_norm < BASIN_SLOPE_MAX {
        return ReliefClass::BasinWetland;
    }

    // Floodplain: within a stream-order-keyed shoulder + slope-flat.
    let shoulder = floodplain_radius(strahler_order) as u32;
    if shoulder > 0 && river_distance_cells <= shoulder && slope_norm < PLAIN_SLOPE_MAX {
        return ReliefClass::Floodplain;
    }

    // Mountain ridge: high elevation + steep + locally-elevated TPI.
    if elev_norm > MTN_RIDGE_ELEV_MIN
        && slope_norm > MTN_RIDGE_SLOPE_MIN
        && tpi > MTN_RIDGE_TPI_MIN
    {
        return ReliefClass::MountainRidge;
    }
    // Mountain slope: near a mountain neighbour and steep.
    if mountain_distance <= MOUNTAIN_NEIGHBOR_DIST && slope_norm > MTN_SLOPE_SLOPE_MIN {
        return ReliefClass::MountainSlope;
    }

    if rain_norm < BADLANDS_RAIN_MAX
        && local_relief > BADLANDS_RELIEF_MIN
        && slope_norm > BADLANDS_SLOPE_MIN
    {
        return ReliefClass::Badlands;
    }

    if mountain_distance <= FOOTHILLS_DIST_MAX && slope_norm > FOOTHILLS_SLOPE_MIN {
        return ReliefClass::Foothills;
    }

    // Coastal plain: close to ocean + flat.
    if coast_distance <= COAST_DIST_MAX && slope_norm < PLAIN_SLOPE_MAX {
        return ReliefClass::CoastalPlain;
    }

    if elev_norm > PLATEAU_ELEV_MIN
        && slope_norm < PLATEAU_SLOPE_MAX
        && local_relief < PLATEAU_RELIEF_MAX
    {
        return ReliefClass::UplandPlateau;
    }

    if slope_norm < ROLLING_SLOPE_MAX && local_relief < ROLLING_RELIEF_MAX {
        // Plain vs rolling: tighter slope/relief = plain, otherwise rolling.
        if slope_norm < PLAIN_SLOPE_MAX && local_relief < PLAIN_RELIEF_MAX {
            return ReliefClass::LowlandPlain;
        }
        return ReliefClass::RollingHills;
    }

    ReliefClass::LowlandPlain
}

/// Build the per-cell `ReliefMap` from the finalised hydrology field.
/// Pure & deterministic. Computes 3×3 Sobel slope, 5×5 local-relief window,
/// 5×5 TPI, chebyshev coast/mountain distance transforms, and stamps a
/// `ReliefClass` per cell.
///
/// Inputs:
///   * `elev_norm`: `GLOBE_WIDTH × GLOBE_HEIGHT` per-cell normalised elevation in `[0, 1]`.
///   * `hydro`: built `HydrologyMap` (post `build_hydrology`).
///   * `rain_norm`: per-cell rainfall in `[0, 1]`.
///   * `river_cell_mask`: per-cell `true` iff a river polyline touches the cell.
///   * `strahler_at_cell`: per-cell Strahler order (0 for non-river cells).
pub fn build_relief(
    elev_norm: &[f32],
    hydro: &HydrologyMap,
    rain_norm: &[f32],
    river_cell_mask: &[bool],
    strahler_at_cell: &[u8],
) -> ReliefMap {
    let w = GLOBE_WIDTH as usize;
    let h = GLOBE_HEIGHT as usize;
    let n = w * h;
    debug_assert_eq!(hydro.cells.len(), n);
    debug_assert_eq!(elev_norm.len(), n);
    debug_assert_eq!(rain_norm.len(), n);
    debug_assert_eq!(river_cell_mask.len(), n);
    debug_assert_eq!(strahler_at_cell.len(), n);

    // ── 1. Slope (3×3 Sobel on filled_height) ────────────────────────────
    let filled: Vec<f32> = hydro.cells.iter().map(|c| c.filled_height).collect();
    let mut slope = vec![0.0f32; n];
    // Sobel kernel scaling: |∇h| ≈ √(Gx² + Gy²) / 8. We then normalise so a
    // 0.20 elevation drop over one cell reads ≈ 1.0 — that's a ~steep slope
    // at our cell scale (64 tiles ≈ 96 m).
    const SLOPE_NORM_SCALE: f32 = 5.0;
    for gy in 0..h {
        for gx in 0..w {
            let xm = (gx + w - 1) % w;
            let xp = (gx + 1) % w;
            let ym = if gy == 0 { 0 } else { gy - 1 };
            let yp = if gy + 1 >= h { h - 1 } else { gy + 1 };
            let h00 = filled[ym * w + xm];
            let h01 = filled[ym * w + gx];
            let h02 = filled[ym * w + xp];
            let h10 = filled[gy * w + xm];
            let h12 = filled[gy * w + xp];
            let h20 = filled[yp * w + xm];
            let h21 = filled[yp * w + gx];
            let h22 = filled[yp * w + xp];
            let gx_grad = (h02 + 2.0 * h12 + h22) - (h00 + 2.0 * h10 + h20);
            let gy_grad = (h20 + 2.0 * h21 + h22) - (h00 + 2.0 * h01 + h02);
            let mag = (gx_grad * gx_grad + gy_grad * gy_grad).sqrt() / 8.0;
            slope[gy * w + gx] = (mag * SLOPE_NORM_SCALE).clamp(0.0, 1.0);
        }
    }

    // ── 2. Local relief + TPI (5×5 window) ───────────────────────────────
    let mut local_relief = vec![0.0f32; n];
    let mut tpi = vec![0.0f32; n];
    // Normalisation: 5×5 spans 320 tiles ≈ 480 m; a 0.30 elevation delta
    // (~30 % of full globe span) is "high relief".
    const RELIEF_NORM_SCALE: f32 = 3.33;
    for gy in 0..h {
        for gx in 0..w {
            let mut hmin = f32::INFINITY;
            let mut hmax = f32::NEG_INFINITY;
            let mut sum = 0.0f32;
            let mut count = 0.0f32;
            for dy in -2..=2i32 {
                let yy = (gy as i32 + dy).clamp(0, h as i32 - 1) as usize;
                for dx in -2..=2i32 {
                    let xx = (gx as i32 + dx).rem_euclid(w as i32) as usize;
                    let v = filled[yy * w + xx];
                    if v < hmin {
                        hmin = v;
                    }
                    if v > hmax {
                        hmax = v;
                    }
                    sum += v;
                    count += 1.0;
                }
            }
            let span = (hmax - hmin).max(0.0);
            local_relief[gy * w + gx] = (span * RELIEF_NORM_SCALE).clamp(0.0, 1.0);
            let mean = sum / count;
            let here = filled[gy * w + gx];
            let denom = span.max(1e-3);
            tpi[gy * w + gx] = ((here - mean) / denom).clamp(-1.0, 1.0);
        }
    }

    // ── 3. Chebyshev distance transforms for coast + mountain ────────────
    let mut coast_dist = vec![u16::MAX; n];
    let mut mountain_dist = vec![u16::MAX; n];
    let mut coast_seeds: Vec<(i32, i32)> = Vec::new();
    let mut mountain_seeds: Vec<(i32, i32)> = Vec::new();
    for gy in 0..h {
        for gx in 0..w {
            let e = elev_norm[gy * w + gx];
            if e < OCEAN_ELEV_GATE {
                coast_seeds.push((gx as i32, gy as i32));
                coast_dist[gy * w + gx] = 0;
            }
            if e > MOUNTAIN_ELEV_GATE {
                mountain_seeds.push((gx as i32, gy as i32));
                mountain_dist[gy * w + gx] = 0;
            }
        }
    }
    chebyshev_bfs(&coast_seeds, &mut coast_dist, w, h);
    chebyshev_bfs(&mountain_seeds, &mut mountain_dist, w, h);

    // ── 4. Per-cell chebyshev distance to nearest river cell ─────────────
    let mut river_dist = vec![u32::MAX; n];
    let mut river_seeds: Vec<(i32, i32)> = Vec::new();
    for gy in 0..h {
        for gx in 0..w {
            if river_cell_mask[gy * w + gx] {
                river_seeds.push((gx as i32, gy as i32));
                river_dist[gy * w + gx] = 0;
            }
        }
    }
    chebyshev_bfs_u32(&river_seeds, &mut river_dist, w, h);

    // ── 5. Compose ReliefCell per index ──────────────────────────────────
    let mut cells = vec![ReliefCell::default(); n];
    for i in 0..n {
        let hc = hydro.cells[i];
        let aquifer_depth_norm = (hc.filled_height - hc.aquifer_level).max(0.0);
        let reservoir_kind = hydro.reservoirs.get(hc.reservoir_id as usize).map(|r| r.kind);
        let class = classify(
            elev_norm[i],
            slope[i],
            local_relief[i],
            tpi[i],
            coast_dist[i],
            mountain_dist[i],
            river_dist[i],
            strahler_at_cell[i],
            aquifer_depth_norm,
            rain_norm[i],
            reservoir_kind,
        );
        cells[i] = ReliefCell {
            slope_norm: slope[i],
            local_relief: local_relief[i],
            topographic_position: tpi[i],
            coast_distance: coast_dist[i],
            mountain_distance: mountain_dist[i],
            aquifer_depth_norm,
            relief: class,
        };
    }

    ReliefMap { cells }
}

/// Multi-source chebyshev BFS with X-wrap, Y-clamp. Distances initialised
/// upstream; seeds carry `dist = 0`.
fn chebyshev_bfs(seeds: &[(i32, i32)], dist: &mut [u16], w: usize, h: usize) {
    use std::collections::VecDeque;
    let mut q: VecDeque<(i32, i32)> = VecDeque::with_capacity(seeds.len() * 8);
    for &s in seeds {
        q.push_back(s);
    }
    while let Some((gx, gy)) = q.pop_front() {
        let d = dist[gy as usize * w + gx as usize];
        if d == u16::MAX {
            continue;
        }
        let nd = d.saturating_add(1);
        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = (gx + dx).rem_euclid(w as i32);
                let ny = gy + dy;
                if ny < 0 || ny >= h as i32 {
                    continue;
                }
                let ni = ny as usize * w + nx as usize;
                if dist[ni] > nd {
                    dist[ni] = nd;
                    q.push_back((nx, ny));
                }
            }
        }
    }
}

fn chebyshev_bfs_u32(seeds: &[(i32, i32)], dist: &mut [u32], w: usize, h: usize) {
    use std::collections::VecDeque;
    let mut q: VecDeque<(i32, i32)> = VecDeque::with_capacity(seeds.len() * 8);
    for &s in seeds {
        q.push_back(s);
    }
    while let Some((gx, gy)) = q.pop_front() {
        let d = dist[gy as usize * w + gx as usize];
        if d == u32::MAX {
            continue;
        }
        let nd = d.saturating_add(1);
        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = (gx + dx).rem_euclid(w as i32);
                let ny = gy + dy;
                if ny < 0 || ny >= h as i32 {
                    continue;
                }
                let ni = ny as usize * w + nx as usize;
                if dist[ni] > nd {
                    dist[ni] = nd;
                    q.push_back((nx, ny));
                }
            }
        }
    }
}

/// Bilinearly-interpolated per-tile relief sample. `class` re-derived from
/// interpolated numerics so the per-tile class is continuous across cell
/// boundaries (avoids 64-tile patchwork).
#[derive(Clone, Copy, Debug)]
pub struct ReliefSample {
    pub slope: f32,
    pub local_relief: f32,
    pub mountain_distance: f32,
    pub coast_distance: f32,
    pub aquifer_depth_norm: f32,
    pub topographic_position: f32,
    pub class: ReliefClass,
}

/// Estimate per-mega-chunk dominant relief by sampling the cell-grid in an
/// 8×8 window centred on the mega-chunk. Returns `None` for empty maps.
pub fn dominant_relief_in_megachunk(relief: &ReliefMap, mx: i32, my: i32) -> Option<ReliefClass> {
    if relief.cells.is_empty() {
        return None;
    }
    let w = GLOBE_WIDTH;
    let h = GLOBE_HEIGHT;
    let cells_per_megachunk = (MEGACHUNK_SIZE_CHUNKS / super::globe::GLOBE_CELL_CHUNKS).max(1);
    let cx0 = mx * cells_per_megachunk;
    let cy0 = my * cells_per_megachunk;
    let mut counts: [u32; 11] = [0; 11];
    for dy in 0..cells_per_megachunk {
        for dx in 0..cells_per_megachunk {
            let gx = (cx0 + dx).rem_euclid(w);
            let gy = (cy0 + dy).clamp(0, h - 1);
            let cls = relief.cells[(gy * w + gx) as usize].relief as usize;
            counts[cls] += 1;
        }
    }
    let mut best = 0usize;
    for i in 1..11 {
        if counts[i] > counts[best] {
            best = i;
        }
    }
    // Safety: index ≤ 10 maps to a valid ReliefClass variant.
    Some(match best {
        0 => ReliefClass::OceanShelf,
        1 => ReliefClass::CoastalPlain,
        2 => ReliefClass::Floodplain,
        3 => ReliefClass::BasinWetland,
        4 => ReliefClass::LowlandPlain,
        5 => ReliefClass::RollingHills,
        6 => ReliefClass::UplandPlateau,
        7 => ReliefClass::Badlands,
        8 => ReliefClass::Foothills,
        9 => ReliefClass::MountainSlope,
        _ => ReliefClass::MountainRidge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_basic_classes() {
        // Lowland plain — flat, low relief, no other gates.
        let c = classify(0.40, 0.010, 0.020, 0.0, 50, 50, 999, 0, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::LowlandPlain);

        // Rolling hills — modest slope.
        let c = classify(0.45, 0.030, 0.080, 0.1, 50, 50, 999, 0, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::RollingHills);

        // Plateau — flat and high.
        let c = classify(0.65, 0.015, 0.030, 0.0, 50, 50, 999, 0, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::UplandPlateau);

        // Mountain ridge — steep, high, TPI positive.
        let c = classify(0.95, 0.20, 0.30, 0.8, 50, 0, 999, 0, 1.0, 0.5, None);
        assert_eq!(c, ReliefClass::MountainRidge);

        // Mountain slope — steep + neighbour to mountain.
        let c = classify(0.70, 0.10, 0.20, 0.2, 50, 1, 999, 0, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::MountainSlope);

        // Badlands — dry + chopped relief.
        let c = classify(0.55, 0.08, 0.20, 0.0, 50, 50, 999, 0, 0.5, 0.15, None);
        assert_eq!(c, ReliefClass::Badlands);

        // Foothills — close to mountain + moderate slope.
        let c = classify(0.55, 0.05, 0.10, 0.0, 50, 4, 999, 0, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::Foothills);

        // Coastal plain — close to ocean + flat.
        let c = classify(0.30, 0.010, 0.020, 0.0, 2, 50, 999, 0, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::CoastalPlain);

        // Floodplain — within stream-order shoulder + flat.
        let c = classify(0.30, 0.010, 0.020, 0.0, 50, 50, 2, 3, 0.5, 0.5, None);
        assert_eq!(c, ReliefClass::Floodplain);

        // Basin wetland — wetland reservoir.
        let c = classify(
            0.30,
            0.005,
            0.010,
            -0.5,
            50,
            50,
            999,
            0,
            0.0,
            0.6,
            Some(ReservoirKind::Wetland),
        );
        assert_eq!(c, ReliefClass::BasinWetland);

        // Ocean shelf — ocean reservoir.
        let c = classify(
            0.10,
            0.005,
            0.010,
            -0.8,
            0,
            50,
            999,
            0,
            0.0,
            0.5,
            Some(ReservoirKind::Ocean),
        );
        assert_eq!(c, ReliefClass::OceanShelf);
    }

    #[test]
    fn permits_stone_outcrop_only_for_upland_classes() {
        assert!(!ReliefClass::LowlandPlain.permits_stone_outcrop());
        assert!(!ReliefClass::Floodplain.permits_stone_outcrop());
        assert!(!ReliefClass::BasinWetland.permits_stone_outcrop());
        assert!(ReliefClass::UplandPlateau.permits_stone_outcrop());
        assert!(ReliefClass::Badlands.permits_stone_outcrop());
        assert!(ReliefClass::Foothills.permits_stone_outcrop());
        assert!(ReliefClass::MountainSlope.permits_stone_outcrop());
        assert!(ReliefClass::MountainRidge.permits_stone_outcrop());
    }

    #[test]
    fn fertility_mult_monotonic_for_arable_classes() {
        assert!(ReliefClass::Floodplain.fertility_mult() >= ReliefClass::LowlandPlain.fertility_mult());
        assert!(ReliefClass::LowlandPlain.fertility_mult() >= ReliefClass::UplandPlateau.fertility_mult());
        assert!(ReliefClass::UplandPlateau.fertility_mult() >= ReliefClass::Foothills.fertility_mult());
        assert!(ReliefClass::Foothills.fertility_mult() >= ReliefClass::Badlands.fertility_mult());
        assert!(ReliefClass::Badlands.fertility_mult() >= ReliefClass::MountainSlope.fertility_mult());
    }

    #[test]
    fn settlement_score_orders_plains_above_uplands() {
        assert!(
            ReliefClass::LowlandPlain.settlement_score_bonus()
                > ReliefClass::UplandPlateau.settlement_score_bonus()
        );
        assert!(
            ReliefClass::UplandPlateau.settlement_score_bonus()
                > ReliefClass::Foothills.settlement_score_bonus()
        );
        assert!(
            ReliefClass::Foothills.settlement_score_bonus()
                > ReliefClass::Badlands.settlement_score_bonus()
        );
    }

    #[test]
    fn rejects_settlement_for_impassable_classes() {
        assert!(ReliefClass::MountainSlope.rejects_settlement());
        assert!(ReliefClass::MountainRidge.rejects_settlement());
        assert!(ReliefClass::OceanShelf.rejects_settlement());
        assert!(!ReliefClass::Foothills.rejects_settlement());
        assert!(!ReliefClass::Badlands.rejects_settlement());
    }
}
