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
    if elevation_f > MOUNTAIN_ELEV_GATE {
        return Biome::Mountain;
    }
    if elevation_f < OCEAN_ELEV_GATE {
        return Biome::Ocean;
    }
    classify_land(elevation_f, temp_f, rainfall_f)
}

/// Land-biome subset of [`classify`] — runs the Wetland/Badlands/Steppe and
/// Whittaker matrix without the Ocean/Mountain elevation gates. Behaviour
/// inside `[OCEAN_ELEV_GATE, MOUNTAIN_ELEV_GATE]` is identical to
/// [`classify`]. Split out so the surface-biome layer can call it with
/// *warped* temp/rain but the tile's *true* elevation (gates stay on true
/// elevation, see [`classify_surface_at_tile`]).
pub fn classify_land(elevation_f: f32, temp_f: f32, rainfall_f: f32) -> Biome {
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

/// True-elevation gates kept on the canonical climate field: tiles inside
/// the Ocean or Mountain elevation band classify identically to
/// [`classify_at_tile`] regardless of any surface-biome warp. This is the
/// structural guarantee that coasts / water columns / salinity / inland
/// orography stay in sync with hydrology — no inland oceans, no random
/// inland peaks from the warp.
pub const OCEAN_ELEV_GATE: f32 = 0.22;
pub const MOUNTAIN_ELEV_GATE: f32 = 0.82;

/// Per-tile biome classification using the bilinearly-interpolated climate
/// field — eliminates hard biome stripes at climate-cell boundaries.
///
/// **Canonical**: this is the AI / world-sim / water-salinity classifier
/// and stays decoupled from any visual surface warp. Visual / terrain
/// systems should call [`classify_surface_at_tile`] (or
/// [`surface_biome_sample_at_tile`] for ecotone-aware kind picking) so
/// borders are organic without dragging canonical semantics with them.
pub fn classify_at_tile(globe: &Globe, tile_x: i32, tile_y: i32) -> Biome {
    let (elev_u, temp_c, rain_u) = globe.sample_climate(tile_x, tile_y);
    let elev_f = elev_u / 255.0;
    // Convert temp_c (-30..50ish) back into a 0..1 normalised temperature
    // that matches the scale used during globe gen.
    let temp_f = ((temp_c + 30.0) / 80.0).clamp(0.0, 1.0);
    let rain_f = (rain_u / 255.0).clamp(0.0, 1.0);
    classify(elev_f, temp_f, rain_f)
}

// ───────────────────────── Surface-biome layer ──────────────────────────
//
// A *separate* biome decision used for visible terrain and previews.
// Canonical [`classify`] / [`classify_at_tile`] are untouched. The land
// portion of the decision is run on **warped** temp/rain samples so borders
// feather organically; the true (unwarped) elevation drives the Ocean and
// Mountain gates so coasts and orography stay exactly where hydrology /
// reservoirs / salinity expect them.
//
// All warp noise is a *stateless* hash value-noise — pure fn of
// `(globe.seed, tile_x, tile_y)`. We deliberately do NOT use the `noise`
// crate's `Perlin` here: `classify_surface_at_tile` runs per-tile in
// chunk-gen and per-pixel in the preview (oversample 4 ≈ 2M pixels), and
// `Perlin::set_seed` builds a 256-entry permutation table on every call.
// The `Perlin` instances on `WorldGen` aren't reachable from the preview
// path (only `&Globe`), so this hash route is also what gives
// preview↔terrain parity.

/// Warp wavelength (tiles). One climate cell is 64 tiles (2 chunks ×
/// `CHUNK_SIZE`); a 128-tile wavelength keeps warp features sub-continental
/// but multi-cell so a Grassland/Desert edge can curve through several
/// cells before relaxing.
const SURFACE_WARP_WAVELENGTH: f32 = 128.0;
/// Max warp amplitude in tiles (~⅓ of a climate cell). Pulled from the
/// original biome-edge plan.
const SURFACE_WARP_AMPLITUDE: f32 = 24.0;
/// Max ecotone accent weight (0..1). Capped low so the accent dithers into
/// the base palette rather than replacing it; matches the plan default.
pub const MAX_ACCENT_WEIGHT: f32 = 0.35;

#[inline]
fn surface_hash_unit(seed: u32, x: i32, y: i32) -> f32 {
    // xxhash-style 32-bit avalanche over (seed, x, y). Pure, ~5ns, no alloc.
    let mut h = seed
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add((x as u32).wrapping_mul(0x85EB_CA6B))
        .wrapping_add((y as u32).wrapping_mul(0xC2B2_AE35));
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    // Map u32 → [0, 1].
    (h as f32) * (1.0 / u32::MAX as f32)
}

#[inline]
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Smoothed unit value-noise at fractional `(x, y)` over a 1-unit integer
/// lattice; output in `[0, 1]`. Caller pre-scales `x/y` to control the
/// wavelength.
#[inline]
fn surface_value_noise01(seed: u32, x: f32, y: f32) -> f32 {
    let x0 = x.floor();
    let y0 = y.floor();
    let xi = x0 as i32;
    let yi = y0 as i32;
    let tx = smoothstep(x - x0);
    let ty = smoothstep(y - y0);
    let c00 = surface_hash_unit(seed, xi, yi);
    let c10 = surface_hash_unit(seed, xi + 1, yi);
    let c01 = surface_hash_unit(seed, xi, yi + 1);
    let c11 = surface_hash_unit(seed, xi + 1, yi + 1);
    let a = c00 + (c10 - c00) * tx;
    let b = c01 + (c11 - c01) * tx;
    a + (b - a) * ty
}

#[inline]
fn salt_seed(seed: u64, salt: u64) -> u32 {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt;
    // Fold to 32 bits.
    (mixed ^ (mixed >> 32)) as u32
}

/// Domain-warp offset (in tiles) applied to climate sampling for the
/// surface-biome layer. Pure fn of `(seed, tx, ty)`; two decorrelated
/// channels give the X and Y components.
pub fn surface_warp_offset(seed: u64, tile_x: i32, tile_y: i32) -> (f32, f32) {
    let sx = salt_seed(seed, 0x51A1_5E5E_1234_5678);
    let sy = salt_seed(seed, 0xA110_C0DE_DEAD_BEEF);
    let x = tile_x as f32 / SURFACE_WARP_WAVELENGTH;
    let y = tile_y as f32 / SURFACE_WARP_WAVELENGTH;
    // [0,1] → [-1,1] for symmetric warp.
    let dx = (surface_value_noise01(sx, x, y) - 0.5) * 2.0 * SURFACE_WARP_AMPLITUDE;
    let dy = (surface_value_noise01(sy, x, y) - 0.5) * 2.0 * SURFACE_WARP_AMPLITUDE;
    (dx, dy)
}

/// Visual / terrain biome classifier. Mirrors [`classify_at_tile`] except
/// the land branch runs on temp/rain sampled at a domain-warped offset, so
/// land-biome borders feather organically. Ocean/Mountain gates stay on
/// the tile's true elevation (no inland oceans, no random inland peaks,
/// coasts and water columns untouched).
pub fn classify_surface_at_tile(globe: &Globe, tile_x: i32, tile_y: i32) -> Biome {
    let (elev_u, _, _) = globe.sample_climate(tile_x, tile_y);
    let elev_f = elev_u / 255.0;
    if elev_f > MOUNTAIN_ELEV_GATE {
        return Biome::Mountain;
    }
    if elev_f < OCEAN_ELEV_GATE {
        return Biome::Ocean;
    }
    let (dx, dy) = surface_warp_offset(globe.seed, tile_x, tile_y);
    let wx = tile_x + dx.round() as i32;
    let wy = tile_y + dy.round() as i32;
    let (_, temp_c_w, rain_u_w) = globe.sample_climate(wx, wy);
    let temp_f = ((temp_c_w + 30.0) / 80.0).clamp(0.0, 1.0);
    let rain_f = (rain_u_w / 255.0).clamp(0.0, 1.0);
    classify_land(elev_f, temp_f, rain_f)
}

/// Layered surface-biome sample. `base` is the warped land biome at this
/// tile; `accent` is the land biome reached by a *second*, decorrelated
/// warp (the biome "just across" the local gradient); `accent_weight ∈
/// [0, MAX_ACCENT_WEIGHT]` is the dither weight for visible material
/// selection (0 deep inside a biome, capped near the border). Returned
/// `base`/`accent` are always equal for Ocean/Mountain (no soft blending
/// across hydrology/orography gates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceBiomeSample {
    pub base: Biome,
    pub accent: Biome,
    /// In tenths-of-a-percent (×10000) for `Eq` derivation; use
    /// [`SurfaceBiomeSample::accent_weight`] for the f32 view.
    weight_q: u16,
}

impl SurfaceBiomeSample {
    #[inline]
    pub fn accent_weight(&self) -> f32 {
        self.weight_q as f32 / 10_000.0
    }

    #[inline]
    fn pure(b: Biome) -> Self {
        Self {
            base: b,
            accent: b,
            weight_q: 0,
        }
    }
}

/// Sample the surface biome at a tile with ecotone information. Derivation
/// is O(1) per tile (no spatial probe): the secondary warp gives the
/// "other side" biome, and if it differs from `base` we're inside a
/// transition band → emit a dithered `accent_weight` capped at
/// `MAX_ACCENT_WEIGHT`. Band width follows the climate gradient magnitude
/// naturally (gentle gradients widen the ecotone).
pub fn surface_biome_sample_at_tile(
    globe: &Globe,
    tile_x: i32,
    tile_y: i32,
) -> SurfaceBiomeSample {
    let (elev_u, _, _) = globe.sample_climate(tile_x, tile_y);
    let elev_f = elev_u / 255.0;
    if elev_f > MOUNTAIN_ELEV_GATE {
        return SurfaceBiomeSample::pure(Biome::Mountain);
    }
    if elev_f < OCEAN_ELEV_GATE {
        return SurfaceBiomeSample::pure(Biome::Ocean);
    }

    // Primary warp → base.
    let (dx1, dy1) = surface_warp_offset(globe.seed, tile_x, tile_y);
    let (_, t1, r1) =
        globe.sample_climate(tile_x + dx1.round() as i32, tile_y + dy1.round() as i32);
    let temp_f1 = ((t1 + 30.0) / 80.0).clamp(0.0, 1.0);
    let rain_f1 = (r1 / 255.0).clamp(0.0, 1.0);
    let base = classify_land(elev_f, temp_f1, rain_f1);

    // Secondary, decorrelated warp → accent. We offset by an extra
    // half-amplitude in a salt-different direction so accent reflects what
    // a nearby tile would have read.
    let salt = globe.seed ^ 0x51A1_5E5E_5E5E_51A1;
    let (dx2_extra, dy2_extra) = surface_warp_offset(salt, tile_x, tile_y);
    let dx2 = dx1 + dx2_extra * 0.5;
    let dy2 = dy1 + dy2_extra * 0.5;
    let (_, t2, r2) =
        globe.sample_climate(tile_x + dx2.round() as i32, tile_y + dy2.round() as i32);
    let temp_f2 = ((t2 + 30.0) / 80.0).clamp(0.0, 1.0);
    let rain_f2 = (r2 / 255.0).clamp(0.0, 1.0);
    let accent = classify_land(elev_f, temp_f2, rain_f2);

    let weight = if base == accent {
        0.0
    } else {
        // High-freq dither so the visible boundary speckles rather than
        // forming a clean isoline. Wavelength ≈ 5 tiles → fine grain.
        let dither = surface_value_noise01(
            salt_seed(globe.seed, 0xECEC_0A0A_0A0A_ECEC),
            tile_x as f32 * 0.18,
            tile_y as f32 * 0.18,
        );
        MAX_ACCENT_WEIGHT * dither
    };
    SurfaceBiomeSample {
        base,
        accent,
        weight_q: (weight * 10_000.0).round().clamp(0.0, u16::MAX as f32) as u16,
    }
}

/// Deterministic per-tile dither noise in `[0, 1]` for ecotone kind
/// selection. Compare against `sample.accent_weight()` to decide whether
/// to consult the base or accent biome's `BiomeBands`.
pub fn surface_band_dither(seed: u64, tile_x: i32, tile_y: i32) -> f32 {
    surface_value_noise01(
        salt_seed(seed, 0xB0B0_F00F_B0B0_F00F),
        tile_x as f32 * 0.42,
        tile_y as f32 * 0.42,
    )
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

    #[test]
    fn classify_land_matches_classify_inside_gates() {
        // For any (elev, temp, rain) inside the Ocean/Mountain gates,
        // classify == classify_land — the only difference is the gate
        // short-circuit at the top of `classify`.
        for &e in &[0.22, 0.30, 0.45, 0.60, 0.75, 0.82] {
            for &t in &[0.05, 0.25, 0.45, 0.65, 0.85] {
                for &r in &[0.05, 0.25, 0.45, 0.65, 0.85] {
                    let a = classify(e, t, r);
                    let b = classify_land(e, t, r);
                    assert_eq!(a, b, "mismatch at elev={} temp={} rain={}", e, t, r);
                }
            }
        }
    }

    #[test]
    fn warp_offset_is_deterministic_and_seed_sensitive() {
        let a = surface_warp_offset(42, 1000, -500);
        let b = surface_warp_offset(42, 1000, -500);
        assert_eq!(a.0.to_bits(), b.0.to_bits(), "warp not deterministic (x)");
        assert_eq!(a.1.to_bits(), b.1.to_bits(), "warp not deterministic (y)");
        let c = surface_warp_offset(43, 1000, -500);
        assert!(
            (a.0 - c.0).abs() > 1e-6 || (a.1 - c.1).abs() > 1e-6,
            "warp not seed-sensitive"
        );
        // Amplitude bound.
        for tx in -2000..2000i32 {
            let (dx, dy) = surface_warp_offset(42, tx, tx / 3);
            assert!(
                dx.abs() <= 24.0 + 1e-3 && dy.abs() <= 24.0 + 1e-3,
                "warp out of bounds at tx={}: ({}, {})",
                tx,
                dx,
                dy,
            );
        }
    }

    #[test]
    fn surface_classifier_preserves_ocean_mountain_gates() {
        // Within the Ocean/Mountain elevation bands the surface classifier
        // must agree with canonical `classify_at_tile` — this is the
        // structural guarantee that the warp never spawns inland oceans or
        // random inland peaks.
        let g = crate::world::globe::generate_globe(42);
        let mut checked = 0;
        for ty in (-256..256i32).step_by(13) {
            for tx in (-256..256i32).step_by(13) {
                let (elev_u, _, _) = g.sample_climate(tx, ty);
                let elev_f = elev_u / 255.0;
                if elev_f > MOUNTAIN_ELEV_GATE || elev_f < OCEAN_ELEV_GATE {
                    let canonical = classify_at_tile(&g, tx, ty);
                    let surface = classify_surface_at_tile(&g, tx, ty);
                    assert_eq!(
                        canonical, surface,
                        "gate desync at ({}, {}) elev_f={}",
                        tx, ty, elev_f,
                    );
                    checked += 1;
                }
            }
        }
        // Sanity: actually exercised the gates on a sample.
        assert!(checked > 0, "no gated tiles sampled — adjust the loop");
    }

    #[test]
    fn surface_sample_weight_bounds_and_pure_interior() {
        let g = crate::world::globe::generate_globe(42);
        for ty in (-512..512i32).step_by(19) {
            for tx in (-512..512i32).step_by(19) {
                let s = surface_biome_sample_at_tile(&g, tx, ty);
                let w = s.accent_weight();
                assert!(
                    (0.0..=MAX_ACCENT_WEIGHT + 1e-3).contains(&w),
                    "accent_weight {} out of bounds at ({}, {})",
                    w,
                    tx,
                    ty,
                );
                // Whenever base == accent we expect weight 0 (interior).
                if s.base == s.accent {
                    assert_eq!(s.weight_q, 0, "interior tile has weight at ({}, {})", tx, ty);
                }
                // Ocean/Mountain gates → pure samples (no ecotone).
                if matches!(s.base, Biome::Ocean | Biome::Mountain) {
                    assert_eq!(s.base, s.accent);
                    assert_eq!(s.weight_q, 0);
                }
            }
        }
    }

    #[test]
    fn surface_sample_produces_ecotone_variety() {
        // Walk a long transect and count how many tiles end up with
        // accent != base (i.e. inside a transition band). On any non-empty
        // globe this should fire on a meaningful fraction of land tiles.
        let g = crate::world::globe::generate_globe(42);
        let mut total_land = 0;
        let mut in_ecotone = 0;
        for ty in (-1024..1024i32).step_by(7) {
            for tx in (-1024..1024i32).step_by(7) {
                let s = surface_biome_sample_at_tile(&g, tx, ty);
                if matches!(s.base, Biome::Ocean | Biome::Mountain) {
                    continue;
                }
                total_land += 1;
                if s.base != s.accent {
                    in_ecotone += 1;
                }
            }
        }
        assert!(total_land > 100, "transect produced too little land");
        // Should always have *some* ecotone tiles in a globe-sized sample.
        assert!(
            in_ecotone > 0,
            "no ecotone tiles found in {} land samples — warp likely inert",
            total_land,
        );
    }
}
