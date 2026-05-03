//! Biome classification (Whittaker-style). Pure functions used by both
//! globe-cell classification and per-tile sampling for cross-mega-chunk
//! continuity.

use super::globe::{Biome, Globe};

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
