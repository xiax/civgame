use crate::world::chunk::{CHUNK_HEIGHT, Z_MIN};
use crate::world::tile::{OreKind, TileKind};
use bevy::prelude::Color;

pub fn tile_color(kind: TileKind) -> Color {
    match kind {
        TileKind::Grass => Color::srgb(0.35, 0.65, 0.25),
        TileKind::Water => Color::srgb(0.15, 0.40, 0.75),
        TileKind::River => Color::srgb(0.30, 0.55, 0.85),
        TileKind::Stone => Color::srgb(0.50, 0.50, 0.50),
        TileKind::Forest => Color::srgb(0.10, 0.40, 0.15),
        TileKind::Sand => Color::srgb(0.86, 0.77, 0.55),
        TileKind::Road => Color::srgb(0.55, 0.45, 0.35),
        TileKind::Air => Color::srgb(0.00, 0.00, 0.00), // never directly rendered
        TileKind::Wall => Color::srgb(0.28, 0.24, 0.22),
        TileKind::Ramp => Color::srgb(0.60, 0.50, 0.35),
        TileKind::Dirt => Color::srgb(0.45, 0.30, 0.18),
        // Fallback for Ore without an OreKind (shouldn't happen in practice).
        // Use ore_tile_color() to render specific ores.
        TileKind::Ore => Color::srgb(0.32, 0.28, 0.26),
        // Climate surfaces
        TileKind::Snow => Color::srgb(0.93, 0.94, 0.97),
        TileKind::Marsh => Color::srgb(0.30, 0.45, 0.30),
        TileKind::Scrub => Color::srgb(0.55, 0.62, 0.35),
        // Stone lithologies
        TileKind::Granite => Color::srgb(0.55, 0.52, 0.50),
        TileKind::Limestone => Color::srgb(0.78, 0.74, 0.62),
        TileKind::Sandstone => Color::srgb(0.80, 0.62, 0.40),
        TileKind::Basalt => Color::srgb(0.25, 0.22, 0.22),
        // Soil variants
        TileKind::Loam => Color::srgb(0.42, 0.30, 0.18),
        TileKind::Silt => Color::srgb(0.55, 0.45, 0.30),
        TileKind::Clay => Color::srgb(0.60, 0.40, 0.30),
        TileKind::SandySoil => Color::srgb(0.72, 0.60, 0.40),
        // Constructed timber span over a river.
        TileKind::Bridge => Color::srgb(0.55, 0.38, 0.22),
        // Tilled farm soil — golden-earth, deliberately distinct from Grass,
        // Road (0.55,0.45,0.35), and Loam (0.42,0.30,0.18) so a field reads
        // as a block.
        TileKind::Cropland => Color::srgb(0.62, 0.47, 0.17),
    }
}

/// Color for a specific ore embedded in a `TileKind::Ore` tile. Used by the
/// chunk renderer to pick a per-ore material handle.
pub fn ore_tile_color(ore: OreKind) -> Color {
    match ore {
        OreKind::None => tile_color(TileKind::Ore),
        OreKind::Copper => Color::srgb(0.72, 0.40, 0.20),
        OreKind::Tin => Color::srgb(0.78, 0.78, 0.82),
        OreKind::Iron => Color::srgb(0.45, 0.32, 0.30),
        OreKind::Coal => Color::srgb(0.12, 0.10, 0.10),
        OreKind::Gold => Color::srgb(0.92, 0.78, 0.18),
        OreKind::Silver => Color::srgb(0.85, 0.85, 0.90),
    }
}

/// Z-shaded version of `ore_tile_color`.
pub fn shaded_ore_tile_color(ore: OreKind, z: i32) -> Color {
    let srgb = ore_tile_color(ore).to_srgba();
    let t = (z - Z_MIN) as f32 / (CHUNK_HEIGHT - 1) as f32;
    let shade = 0.55 + 0.45 * t;
    Color::srgb(srgb.red * shade, srgb.green * shade, srgb.blue * shade)
}

/// Base tile color shaded by discrete Z level.
/// Z_MIN → 55% brightness; Z_MAX → 100% brightness.
pub fn shaded_tile_color(kind: TileKind, z: i32) -> Color {
    let srgb = tile_color(kind).to_srgba();
    let t = (z - Z_MIN) as f32 / (CHUNK_HEIGHT - 1) as f32; // 0..1
    let shade = 0.55 + 0.45 * t;
    Color::srgb(srgb.red * shade, srgb.green * shade, srgb.blue * shade)
}

/// Quantise Z into one of 8 shade buckets (z / 4).
pub fn z_bucket(z: i32) -> i32 {
    z.div_euclid(4)
}
