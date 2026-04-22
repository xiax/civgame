use bevy::prelude::Color;
use crate::world::tile::TileKind;

pub fn tile_color(kind: TileKind) -> Color {
    match kind {
        TileKind::Grass    => Color::srgb(0.35, 0.65, 0.25),
        TileKind::Water    => Color::srgb(0.15, 0.40, 0.75),
        TileKind::Stone    => Color::srgb(0.50, 0.50, 0.50),
        TileKind::Forest   => Color::srgb(0.10, 0.40, 0.15),
        TileKind::Farmland => Color::srgb(0.70, 0.55, 0.25),
        TileKind::Road     => Color::srgb(0.55, 0.45, 0.35),
    }
}
