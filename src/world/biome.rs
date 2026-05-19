//! Biome classification (Whittaker-style). Pure functions used by both
//! globe-cell classification and per-tile sampling for cross-mega-chunk
//! continuity.

use super::globe::{Biome, Globe};
use super::tile::TileKind;

/// Salinity classification for a water tile. Lakes/wetlands inside
/// continents are `Fresh`; closed (endorheic) basins evaporate to
/// `Brackish`; the ocean is `Salt`. Rivers and freshwater Marsh always read
/// `Fresh` without sampling (a channel stays fresh even crossing a salty
/// basin). **Only `Fresh` is drinkable** — see [`WaterKind::is_drinkable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaterKind {
    Fresh,
    /// Endorheic / partially-saline — too salty for agents and animals.
    Brackish,
    Salt,
}

impl WaterKind {
    /// Agents/animals only drink `Fresh`. Brackish and Salt are rejected by
    /// the thirst pipeline and animal water-seek.
    #[inline]
    pub fn is_drinkable(self) -> bool {
        matches!(self, WaterKind::Fresh)
    }
}

/// Salinity below this reads `Fresh`; at/above [`SALT_SALINITY`] reads
/// `Salt`; in between is `Brackish`. Tuned to worldgen's reservoir salinity
/// (`hydrology.rs`: Lake/Wetland 0.0, Endorheic 0.6, Ocean 1.0).
pub const BRACKISH_SALINITY: f32 = 0.2;
pub const SALT_SALINITY: f32 = 0.7;

/// Pure salinity → [`WaterKind`] mapping (the single threshold decision,
/// unit-tested without a globe).
#[inline]
pub fn classify_salinity(salinity: f32) -> WaterKind {
    if salinity >= SALT_SALINITY {
        WaterKind::Salt
    } else if salinity >= BRACKISH_SALINITY {
        WaterKind::Brackish
    } else {
        WaterKind::Fresh
    }
}

/// Returns the salinity classification for any `Water | River | Marsh` tile
/// from hydrology truth (`Globe::salinity_at` = the tile's reservoir
/// salinity, 0.0 for rivers / open lakes). Caller is expected to have
/// verified the tile is water-like; non-water tiles return `Fresh` as a
/// neutral default. River/Marsh channels skip the sample (always fresh).
/// Signature is byte-identical to the Phase 1 version — only the body now
/// reads salinity instead of a binary ocean test.
pub fn water_kind_at(globe: &Globe, kind: TileKind, tile_x: i32, tile_y: i32) -> WaterKind {
    match kind {
        TileKind::River | TileKind::Marsh => WaterKind::Fresh,
        TileKind::Water => {
            let s = globe.salinity_at(tile_x, tile_y);
            match classify_salinity(s) {
                // Defensive fallback: an ocean cell that somehow isn't a
                // reservoir member (salinity 0) still reads Salt (Phase 1).
                WaterKind::Fresh
                    if matches!(classify_at_tile(globe, tile_x, tile_y), Biome::Ocean) =>
                {
                    WaterKind::Salt
                }
                other => other,
            }
        }
        _ => WaterKind::Fresh,
    }
}

/// Classify a biome from normalised climate inputs.
///
/// - `elevation_f`: 0..1 (sea level → highest peak)
/// - `temp_f`: 0..1 (cold → hot)
/// - `rainfall_f`: 0..1 (dry → wet)
pub fn classify(elevation_f: f32, temp_f: f32, rainfall_f: f32) -> Biome {
    if elevation_f > 0.82 {
        return Biome::Mountain;
    }
    if elevation_f < 0.22 {
        return Biome::Ocean;
    }
    // Warm + waterlogged lowlands → Wetland. Distinct from Tropical: same
    // warmth, but persistently saturated and below the upland transition.
    if elevation_f < 0.30 && rainfall_f > 0.75 && temp_f > 0.30 {
        return Biome::Wetland;
    }
    // Eroded arid uplands → Badlands. Sits between Desert (low elev, hot)
    // and Mountain (high elev). Rocky, sparse vegetation.
    if rainfall_f < 0.25 && elevation_f >= 0.45 && elevation_f <= 0.80 {
        return Biome::Badlands;
    }
    // Dry temperate grassland strip between Grassland and Desert.
    if rainfall_f >= 0.30 && rainfall_f < 0.50 && temp_f >= 0.40 && temp_f < 0.70 {
        return Biome::Steppe;
    }
    match (temp_f > 0.55, rainfall_f > 0.55, temp_f > 0.3) {
        _ if temp_f < 0.2 => Biome::Tundra,
        _ if temp_f < 0.35 && rainfall_f > 0.45 => Biome::Taiga,
        (true, true, _) => Biome::Tropical,
        (true, false, _) => Biome::Desert,
        (false, true, true) => Biome::Temperate,
        _ => Biome::Grassland,
    }
}

/// Per-tile biome classification using the bilinearly-interpolated climate
/// field — eliminates hard biome stripes at climate-cell boundaries.
pub fn classify_at_tile(globe: &Globe, tile_x: i32, tile_y: i32) -> Biome {
    let (elev_u, temp_c, rain_u) = globe.sample_climate(tile_x, tile_y);
    let elev_f = elev_u / 255.0;
    // Convert temp_c (-30..50ish) back into a 0..1 normalised temperature
    // that matches the scale used during globe gen.
    let temp_f = ((temp_c + 30.0) / 80.0).clamp(0.0, 1.0);
    let rain_f = (rain_u / 255.0).clamp(0.0, 1.0);
    classify(elev_f, temp_f, rain_f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salinity_thresholds_map_to_kinds() {
        assert_eq!(classify_salinity(0.0), WaterKind::Fresh);
        assert_eq!(classify_salinity(0.1), WaterKind::Fresh);
        // Endorheic worldgen salinity (0.6) is brackish.
        assert_eq!(classify_salinity(0.6), WaterKind::Brackish);
        assert_eq!(classify_salinity(BRACKISH_SALINITY), WaterKind::Brackish);
        // Ocean worldgen salinity (1.0) is salt.
        assert_eq!(classify_salinity(1.0), WaterKind::Salt);
        assert_eq!(classify_salinity(SALT_SALINITY), WaterKind::Salt);
    }

    #[test]
    fn only_fresh_is_drinkable() {
        assert!(WaterKind::Fresh.is_drinkable());
        assert!(!WaterKind::Brackish.is_drinkable());
        assert!(!WaterKind::Salt.is_drinkable());
    }

    #[test]
    fn rivers_and_marsh_always_fresh() {
        // Channel stays fresh regardless of any reservoir sampling.
        let g = crate::world::globe::generate_globe(42);
        assert_eq!(water_kind_at(&g, TileKind::River, 0, 0), WaterKind::Fresh);
        assert_eq!(water_kind_at(&g, TileKind::Marsh, 12345, -678), WaterKind::Fresh);
        // Non-water tile → neutral Fresh default.
        assert_eq!(water_kind_at(&g, TileKind::Grass, 5, 5), WaterKind::Fresh);
    }
}
