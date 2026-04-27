use crate::world::chunk::{CHUNK_HEIGHT, Z_MIN};
use crate::world::tile::TileKind;
use bevy::prelude::Color;

pub fn tile_color(kind: TileKind) -> Color {
    match kind {
        TileKind::Grass => Color::srgb(0.35, 0.65, 0.25),
        TileKind::Water => Color::srgb(0.15, 0.40, 0.75),
        TileKind::Stone => Color::srgb(0.50, 0.50, 0.50),
        TileKind::Forest => Color::srgb(0.10, 0.40, 0.15),
        TileKind::Farmland => Color::srgb(0.70, 0.55, 0.25),
        TileKind::Road => Color::srgb(0.55, 0.45, 0.35),
        TileKind::Air => Color::srgb(0.00, 0.00, 0.00), // never directly rendered
        TileKind::Wall => Color::srgb(0.28, 0.24, 0.22),
        TileKind::Ramp => Color::srgb(0.60, 0.50, 0.35),
        TileKind::Dirt => Color::srgb(0.45, 0.30, 0.18),
    }
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
