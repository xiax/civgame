//! Composite building footprints — Phase 4 of the Construction Overhaul.
//!
//! Defines `FootprintShape` (Rect / L / U) and `Rotation` (4-way),
//! plus pure-tile-set helpers that generalise `footprint_z_stats` and
//! `is_clear_footprint` to non-rectangular masks.
//!
//! v1 deliberately ships only the type lay-down + helper functions;
//! `BuildIntent::Hut`/`Longhouse` continue to use the rectangular
//! `footprint_z_stats` path. Future templates (Farmstead, RowHouse,
//! CourtyardHouse) consume these helpers when wired into a new
//! `BuildIntent::Composite` variant.

use crate::world::chunk::ChunkMap;

/// Cardinal rotation of a shape mask. R0 is the canonical orientation;
/// R90 rotates 90° clockwise (so an east-facing frontage becomes
/// south-facing, etc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

/// Which side of a `UShape` the open courtyard faces. Combines with
/// `Rotation` to produce an 8-orientation set if needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpeningSide {
    North,
    East,
    South,
    West,
}

/// Anchor convention: `(0, 0)` is the *centre* of the canonical bbox.
/// Tiles are emitted as world-space `(anchor.x + ox, anchor.y + oy)`
/// after `rotate_offset` is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FootprintShape {
    /// `(2 * half_w + 1) × (2 * half_h + 1)` rectangle. Equivalent to the
    /// existing `footprint_z_stats(cx, cy, half_w, half_h)` shape.
    Rect { half_w: i32, half_h: i32 },
    /// L-shape: a `w1 × h1` block (canonical: south-west corner) joined to
    /// a `w2 × h2` block at the south-east corner. Used by Farmstead
    /// (residence + attached yard strip).
    LShape { w1: i32, h1: i32, w2: i32, h2: i32 },
    /// U-shape: outer `w_outer × h_outer` rect with a `w_yard × h_yard`
    /// notch removed on `opening`. Used by CourtyardHouse.
    UShape {
        w_outer: i32,
        h_outer: i32,
        w_yard: i32,
        h_yard: i32,
        opening: OpeningSide,
    },
}

/// Apply a rotation to a `(dx, dy)` offset relative to the anchor.
#[inline]
pub fn rotate_offset(rot: Rotation, dx: i32, dy: i32) -> (i32, i32) {
    match rot {
        Rotation::R0 => (dx, dy),
        Rotation::R90 => (-dy, dx),
        Rotation::R180 => (-dx, -dy),
        Rotation::R270 => (dy, -dx),
    }
}

/// Every tile covered by `shape` at `anchor` under `rot`. Tiles are
/// emitted in row-major canonical order before rotation.
pub fn shape_tiles(shape: FootprintShape, anchor: (i32, i32), rot: Rotation) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let (ax, ay) = anchor;
    match shape {
        FootprintShape::Rect { half_w, half_h } => {
            for dy in -half_h..=half_h {
                for dx in -half_w..=half_w {
                    let (rx, ry) = rotate_offset(rot, dx, dy);
                    out.push((ax + rx, ay + ry));
                }
            }
        }
        FootprintShape::LShape { w1, h1, w2, h2 } => {
            // Canonical L: block A is south-west `w1 × h1`, block B is
            // south-east `w2 × h2`, joined along their southern edge.
            // Origin of block A starts at `(-w1, 0)`; block B at `(0, 0)`.
            for dy in 0..h1 {
                for dx in 0..w1 {
                    let (rx, ry) = rotate_offset(rot, dx - w1, dy);
                    out.push((ax + rx, ay + ry));
                }
            }
            for dy in 0..h2 {
                for dx in 0..w2 {
                    let (rx, ry) = rotate_offset(rot, dx, dy);
                    out.push((ax + rx, ay + ry));
                }
            }
        }
        FootprintShape::UShape {
            w_outer,
            h_outer,
            w_yard,
            h_yard,
            opening,
        } => {
            // Outer rect anchored at south-west `(0, 0)`. Notch removed
            // depending on `opening`.
            let yard_w = w_yard.min(w_outer.saturating_sub(2));
            let yard_h = h_yard.min(h_outer.saturating_sub(2));
            let (yx0, yy0) = match opening {
                OpeningSide::North => ((w_outer - yard_w) / 2, h_outer - yard_h),
                OpeningSide::South => ((w_outer - yard_w) / 2, 0),
                OpeningSide::East => (w_outer - yard_w, (h_outer - yard_h) / 2),
                OpeningSide::West => (0, (h_outer - yard_h) / 2),
            };
            for dy in 0..h_outer {
                for dx in 0..w_outer {
                    let in_yard = dx >= yx0 && dx < yx0 + yard_w && dy >= yy0 && dy < yy0 + yard_h;
                    if in_yard {
                        continue;
                    }
                    let (rx, ry) = rotate_offset(rot, dx, dy);
                    out.push((ax + rx, ay + ry));
                }
            }
        }
    }
    out
}

/// Mean + spread of surface_z over the tiles `shape` covers. Generalises
/// `footprint_z_stats` to non-rectangular masks. Returns `(mean_z,
/// max-min spread)` clamped to `i8` / `u8`.
pub fn shape_z_stats(
    chunk_map: &ChunkMap,
    shape: FootprintShape,
    anchor: (i32, i32),
    rot: Rotation,
) -> (i8, u8) {
    let tiles = shape_tiles(shape, anchor, rot);
    if tiles.is_empty() {
        return (0, 0);
    }
    let mut sum: i32 = 0;
    let mut min_z = i32::MAX;
    let mut max_z = i32::MIN;
    for (tx, ty) in &tiles {
        let z = chunk_map.surface_z_at(*tx, *ty);
        sum += z;
        min_z = min_z.min(z);
        max_z = max_z.max(z);
    }
    let mean = sum / tiles.len() as i32;
    let spread = (max_z - min_z).max(0).min(255) as u8;
    (mean.clamp(i8::MIN as i32, i8::MAX as i32) as i8, spread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_shape_tiles_match_canonical_size() {
        let tiles = shape_tiles(
            FootprintShape::Rect {
                half_w: 1,
                half_h: 1,
            },
            (10, 10),
            Rotation::R0,
        );
        assert_eq!(tiles.len(), 9);
        assert!(tiles.contains(&(9, 9)));
        assert!(tiles.contains(&(11, 11)));
    }

    #[test]
    fn lshape_tiles_count_correct() {
        let tiles = shape_tiles(
            FootprintShape::LShape {
                w1: 2,
                h1: 2,
                w2: 4,
                h2: 6,
            },
            (0, 0),
            Rotation::R0,
        );
        // 2*2 + 4*6 = 28 tiles.
        assert_eq!(tiles.len(), 28);
    }

    #[test]
    fn ushape_excludes_yard() {
        // Outer 5×5 with central 1×3 north-facing yard notch.
        let tiles = shape_tiles(
            FootprintShape::UShape {
                w_outer: 5,
                h_outer: 5,
                w_yard: 1,
                h_yard: 3,
                opening: OpeningSide::North,
            },
            (0, 0),
            Rotation::R0,
        );
        // 25 outer minus 3 yard = 22 tiles.
        assert_eq!(tiles.len(), 22);
        // Yard tile at (2, 4) (north edge) should NOT be in set.
        assert!(!tiles.contains(&(2, 4)));
        // Outer tiles still present.
        assert!(tiles.contains(&(0, 0)));
        assert!(tiles.contains(&(4, 4)));
    }

    #[test]
    fn rotation_180_negates_offset() {
        let (x, y) = rotate_offset(Rotation::R180, 3, 4);
        assert_eq!((x, y), (-3, -4));
    }

    #[test]
    fn rotation_90_swaps_with_sign() {
        let (x, y) = rotate_offset(Rotation::R90, 3, 4);
        assert_eq!((x, y), (-4, 3));
    }
}
