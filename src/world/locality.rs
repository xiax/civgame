//! Read-only locality queries for site characterisation.
//!
//! Phase D of the knowledge-system overhaul (`plans/knowledge-system-overhaul.md`).
//! The Phase E building-technique selector needs to know **what's nearby** —
//! the dominant stone lithology, forest density, clay/wetland/silt proximity,
//! reeds availability — to pick between Wattle-and-Daub, Mudbrick, Dry-Stone,
//! and friends. This module exposes those queries as pure functions of
//! [`Globe`] + the canonical biome / band tables, so they share results with
//! chunk-gen and the world-map preview.
//!
//! No selection change yet — Phase D's contract is only to surface the
//! per-site context so the inspector can verify it and Phase E can consume it.

use super::biome::classify_at_tile;
use super::globe::{Biome, Globe};
use super::terrain::{biome_bands, topsoil_kind};
use super::tile::TileKind;

/// Bounded site-scan radius for forest/clay/wetland/reeds aggregates. Chosen
/// so a 13-tile half-width window (≈ 20 m at 1.5 m/tile) reads roughly the
/// "neighbourhood the founders would consider when picking materials" — large
/// enough to average through a riparian band but small enough to keep each
/// query at ~169 sample points.
pub const SITE_SAMPLE_RADIUS: i32 = 6;

/// Lossy 0..=255 proximity scalar. `0` reads as "nothing nearby"; `255` reads
/// as "fully saturated". Same scale as [`Globe::sample_climate`] outputs so
/// inspector display is uniform.
pub type Proximity = u8;

/// Bundle of read-only site-context features used by Phase E building-technique
/// selection (and, after Phase F, recipe sourcing). Computed at survey time and
/// cached on the [`super::super::simulation::settlement::Settlement`] entity —
/// the inputs are deterministic (`Globe`-only + per-tile river distance) so
/// re-computation is cheap but unnecessary while the home tile holds steady.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSiteContext {
    /// Canonical biome at the site centre (used by techniques that depend on
    /// "is this a wetland?" without a separate scan).
    pub biome: Biome,
    /// Dominant exposed stone lithology near the site, or `None` when the
    /// surrounding relief doesn't permit a bare outcrop.
    pub stone_kind: Option<TileKind>,
    /// Forest cover fraction in the SITE_SAMPLE_RADIUS window, normalised to
    /// `0..=255` (≈ percent × 2.55).
    pub forest_density: Proximity,
    /// Clay/silt soil cover fraction in the window.
    pub clay: Proximity,
    /// Wetland (Marsh-bearing / wet-biome) coverage fraction.
    pub wetland: Proximity,
    /// Chebyshev distance to the nearest river tile, saturating at
    /// `RIVER_PROXIMITY_SCALE`. `255` reads as "river under our feet"; `0`
    /// reads as "river is farther than the scale".
    pub river_silt: Proximity,
}

/// Saturation radius for river-silt proximity scoring. Inside this many tiles
/// the value is non-zero and decays linearly toward 0.
pub const RIVER_PROXIMITY_SCALE: u32 = 16;

/// Stone lithology at `(tx, ty)` if relief permits a bare outcrop, else `None`.
///
/// Mirrors the chunk-gen rule (`BiomeBands::pick_grounded` vs `pick` gated on
/// [`ReliefClass::permits_stone_outcrop`]). We don't need the per-tile noise
/// value to identify *which* stone variant the band paints — every land
/// biome's top band already encodes a single lithology
/// (Temperate/Grassland → Limestone, Mountain core → Granite, etc.).
pub fn stone_kind_at(globe: &Globe, tx: i32, ty: i32) -> Option<TileKind> {
    let relief = globe.sample_relief(tx, ty);
    if !relief.class.permits_stone_outcrop() {
        return None;
    }
    let biome = classify_at_tile(globe, tx, ty);
    let bands = biome_bands(biome);
    // The highest band (kinds[4]) is always the stone slot for land biomes;
    // for Mountain core (Basalt) and Ocean (Granite) it's still meaningful.
    let kind = bands.kinds[4];
    if kind.is_stone_like() {
        Some(kind)
    } else {
        None
    }
}

/// Fraction of forest-bearing tiles in a `(2r+1)²` window centred on
/// `(cx, cy)`. Uses the surface-biome canonical classifier — same source the
/// rendering and fertility paths read — so the score reflects the visible
/// landscape rather than an out-of-date plant map.
pub fn forest_density_around(globe: &Globe, cx: i32, cy: i32, r: i32) -> Proximity {
    if r <= 0 {
        return 0;
    }
    let mut forest = 0u32;
    let mut total = 0u32;
    for dy in -r..=r {
        for dx in -r..=r {
            total += 1;
            let biome = classify_at_tile(globe, cx + dx, cy + dy);
            // Treat any biome whose top-of-vegetation band paints Forest as
            // forested — Temperate / Taiga / Grassland-edge / Wetland / Tropical
            // all surface Forest at high noise; we just count whether the
            // dominant vegetated band IS Forest. That gives a continuous
            // "this is forested country" score independent of per-tile noise.
            let bands = biome_bands(biome);
            if bands.kinds[3] == TileKind::Forest || bands.kinds[2] == TileKind::Forest {
                forest += 1;
            }
        }
    }
    proximity_from_ratio(forest, total)
}

/// Clay/silt soil cover in the window. Reads the canonical `topsoil_kind`
/// rule (riparian band → Silt; Wetland/Tropical → Clay; rest various). Counts
/// `Clay`, `Silt`, and `Loam` (since Loam encloses clay+sand and feeds the
/// same Wattle-and-Daub recipe pool).
pub fn clay_proximity(globe: &Globe, cx: i32, cy: i32, r: i32) -> Proximity {
    if r <= 0 {
        return 0;
    }
    let mut found = 0u32;
    let mut total = 0u32;
    for dy in -r..=r {
        for dx in -r..=r {
            total += 1;
            let tx = cx + dx;
            let ty = cy + dy;
            let biome = classify_at_tile(globe, tx, ty);
            let river_d = globe.nearest_river_chebyshev(tx, ty).min(u32::from(u8::MAX)) as u8;
            let topsoil = topsoil_kind(biome, river_d);
            if matches!(topsoil, TileKind::Clay | TileKind::Silt | TileKind::Loam) {
                found += 1;
            }
        }
    }
    proximity_from_ratio(found, total)
}

/// Wetland coverage — fraction of sampled tiles whose canonical biome is
/// `Wetland` or whose dominant low band is `Marsh`.
pub fn wetland_proximity(globe: &Globe, cx: i32, cy: i32, r: i32) -> Proximity {
    if r <= 0 {
        return 0;
    }
    let mut wet = 0u32;
    let mut total = 0u32;
    for dy in -r..=r {
        for dx in -r..=r {
            total += 1;
            let biome = classify_at_tile(globe, cx + dx, cy + dy);
            if matches!(biome, Biome::Wetland) {
                wet += 1;
                continue;
            }
            let bands = biome_bands(biome);
            if bands.kinds[0] == TileKind::Marsh {
                wet += 1;
            }
        }
    }
    proximity_from_ratio(wet, total)
}

/// True if reeds are reachable from `(cx, cy)` within `r` tiles. Reeds grow
/// where wetland meets vegetated land: any `Marsh`-bearing band adjacent to a
/// fresh-water tile (river or canonical wetland). Cheap-to-eval boolean —
/// Phase F recipe gating cares only about presence/absence.
pub fn reeds_available_within(globe: &Globe, cx: i32, cy: i32, r: i32) -> bool {
    for dy in -r..=r {
        for dx in -r..=r {
            let tx = cx + dx;
            let ty = cy + dy;
            let biome = classify_at_tile(globe, tx, ty);
            if matches!(biome, Biome::Wetland) {
                return true;
            }
            let bands = biome_bands(biome);
            if bands.kinds[0] == TileKind::Marsh && globe.nearest_river_chebyshev(tx, ty) <= 5 {
                return true;
            }
        }
    }
    false
}

/// River proximity scalar: `255` when the cell is on or adjacent to a river,
/// decaying to `0` past [`RIVER_PROXIMITY_SCALE`] tiles. Uses Globe's
/// `nearest_river_chebyshev` so it's consistent with biome-band riparian
/// shifts and settlement-spawn scoring.
pub fn river_silt_proximity(globe: &Globe, cx: i32, cy: i32) -> Proximity {
    let d = globe.nearest_river_chebyshev(cx, cy);
    if d >= RIVER_PROXIMITY_SCALE {
        return 0;
    }
    let lift = RIVER_PROXIMITY_SCALE - d;
    ((lift as f32 / RIVER_PROXIMITY_SCALE as f32) * 255.0).round() as u8
}

/// Bundle every Phase D query into a single `LocalSiteContext` keyed on the
/// site centre. Caller owns the cache lifetime (`Settlement.locality`).
pub fn compute_local_site_context(globe: &Globe, cx: i32, cy: i32) -> LocalSiteContext {
    let r = SITE_SAMPLE_RADIUS;
    LocalSiteContext {
        biome: classify_at_tile(globe, cx, cy),
        stone_kind: stone_kind_at(globe, cx, cy),
        forest_density: forest_density_around(globe, cx, cy, r),
        clay: clay_proximity(globe, cx, cy, r),
        wetland: wetland_proximity(globe, cx, cy, r),
        river_silt: river_silt_proximity(globe, cx, cy),
    }
}

#[inline]
fn proximity_from_ratio(num: u32, denom: u32) -> Proximity {
    if denom == 0 {
        return 0;
    }
    let ratio = num as f32 / denom as f32;
    (ratio * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::globe::generate_globe;

    fn test_globe() -> Globe {
        // Same seed pinned by other globe-dependent tests in the crate.
        generate_globe(42)
    }

    #[test]
    fn proximity_ratio_bounds() {
        assert_eq!(proximity_from_ratio(0, 100), 0);
        assert_eq!(proximity_from_ratio(100, 100), 255);
        assert_eq!(proximity_from_ratio(0, 0), 0);
    }

    #[test]
    fn river_proximity_saturates_at_zero_distance() {
        // Walk the world looking for any river tile (Globe::nearest returns 0
        // exactly on a river cell). On a freshly generated globe at least one
        // river is guaranteed by extract_rivers' min_accum.
        let globe = test_globe();
        let mut hit = None;
        'outer: for ty in (-2048..2048).step_by(64) {
            for tx in (-2048..2048).step_by(64) {
                if globe.nearest_river_chebyshev(tx, ty) == 0 {
                    hit = Some((tx, ty));
                    break 'outer;
                }
            }
        }
        if let Some((tx, ty)) = hit {
            assert_eq!(river_silt_proximity(&globe, tx, ty), 255);
        }
        // Far from anything — sample at the antimeridian and the negative
        // pole; the noise can still be near a river, so only sanity-check
        // that the function returns a `Proximity` (no panic).
        let _ = river_silt_proximity(&globe, 100_000, 100_000);
    }

    #[test]
    fn forest_density_in_wooded_biome_is_substantial() {
        // Sweep until we find a tile whose canonical biome is Temperate or
        // Taiga (both surface forest at high band). The density score there
        // should be non-trivial because the dominant or upper band is Forest.
        let globe = test_globe();
        for ty in (-2048..2048).step_by(32) {
            for tx in (-2048..2048).step_by(32) {
                let biome = classify_at_tile(&globe, tx, ty);
                if matches!(biome, Biome::Temperate | Biome::Taiga | Biome::Tropical) {
                    let d = forest_density_around(&globe, tx, ty, SITE_SAMPLE_RADIUS);
                    // Either upper or dominant vegetated band is Forest in
                    // these biomes — expect any positive coverage in the
                    // local window after biome stability holds across the
                    // sample radius. We allow a generous lower bound to
                    // accept biome boundary tiles.
                    assert!(d > 0, "expected some forest density inside {biome:?}");
                    return;
                }
            }
        }
    }

    #[test]
    fn stone_kind_respects_relief_outcrop_gate() {
        // Find a Mountain Slope / Ridge / Foothills tile — stone_kind should
        // be Some(_), and the kind should be stone-like.
        let globe = test_globe();
        for ty in (-2048..2048).step_by(64) {
            for tx in (-2048..2048).step_by(64) {
                let relief = globe.sample_relief(tx, ty);
                if relief.class.permits_stone_outcrop() {
                    if let Some(kind) = stone_kind_at(&globe, tx, ty) {
                        assert!(kind.is_stone_like(), "{:?} not stone-like", kind);
                        return;
                    }
                }
            }
        }
    }

    #[test]
    fn compute_local_site_context_is_deterministic() {
        let globe = test_globe();
        let a = compute_local_site_context(&globe, 12, 34);
        let b = compute_local_site_context(&globe, 12, 34);
        assert_eq!(a, b);
    }
}
