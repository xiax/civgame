//! River-aware helpers used by early-era settlement seeding and the
//! organic settlement planner.
//!
//! Three responsibilities:
//!
//! - **`same_bank_bfs`** — bounded BFS from `start` to `target` over
//!   passable, non-`is_water_like()` tiles. Returns `true` only if a dry
//!   walking path exists. Reuses the bounded-BFS pattern from
//!   `doormat_reaches_home` (1500-node cap by default).
//! - **`project_to_safe_bank`** — spirals outward from a desired tile
//!   until it finds one that is (a) passable, (b) not water-like, (c)
//!   reachable from `home` without crossing River. Used by paleo hearths
//!   / nomadic camp rings so radial offsets don't deposit beds in rivers
//!   or on the wrong bank.
//! - **`river_orientation_near`** — coarse cardinal axis of a nearby
//!   river segment (NS / EW / NE-SW / NW-SE). Drives the organic
//!   planner's "align main spine parallel to river" rule.

use crate::world::chunk::ChunkMap;

/// Cardinal-or-diagonal axis along which a nearby river segment runs.
/// Used by `organic_settlement` to align road spines parallel to water.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiverAxis {
    /// North-South (vertical) channel.
    NS,
    /// East-West (horizontal) channel.
    EW,
    /// Northwest-Southeast diagonal.
    NwSe,
    /// Northeast-Southwest diagonal.
    NeSw,
}

const BFS_NODE_CAP: usize = 1500;
/// Max spiral radius (chebyshev) when projecting a desired tile onto a
/// safe bank candidate. Larger values increase the chance of finding a
/// candidate at the cost of more `tile_kind_at` lookups.
pub const PROJECT_MAX_RADIUS: i32 = 6;

/// Bounded BFS from `start` to `target`. Returns true iff a path of
/// passable, non-water-like tiles connects them within `BFS_NODE_CAP`
/// explored nodes. Diagonals allowed; matches the agent step set in
/// `pathfinding::step`. Rejects River/Water/Bridge crossings — bridges
/// are passable but `is_water_like()` so this BFS treats them as
/// "wet ground". (Same-bank reachability is the dry-land question; the
/// bridge-aware question uses regular A*.)
pub fn same_bank_bfs(
    chunk_map: &ChunkMap,
    start: (i32, i32),
    target: (i32, i32),
) -> bool {
    if start == target {
        return true;
    }
    let mut visited: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::with_capacity(BFS_NODE_CAP);
    let mut queue: std::collections::VecDeque<(i32, i32)> = std::collections::VecDeque::new();
    queue.push_back(start);
    visited.insert(start);
    while let Some(cur) = queue.pop_front() {
        if visited.len() >= BFS_NODE_CAP {
            return false;
        }
        for (dx, dy) in [
            (1, 0),
            (-1, 0),
            (0, 1),
            (0, -1),
            (1, 1),
            (1, -1),
            (-1, 1),
            (-1, -1),
        ] {
            let nx = cur.0 + dx;
            let ny = cur.1 + dy;
            if (nx, ny) == target {
                // Final step is allowed to land on the target even if it
                // isn't classified as passable — the caller may be asking
                // "can I reach this tile" not "can I stand on it forever".
                return true;
            }
            if visited.contains(&(nx, ny)) {
                continue;
            }
            let Some(kind) = chunk_map.tile_kind_at(nx, ny) else {
                continue;
            };
            if !kind.is_passable() || kind.is_water_like() {
                continue;
            }
            visited.insert((nx, ny));
            queue.push_back((nx, ny));
        }
    }
    false
}

/// Spiral outward from `desired` for a same-bank, non-water-like tile
/// reachable from `home`. Returns `None` only when no such candidate
/// exists inside `PROJECT_MAX_RADIUS`.
pub fn project_to_safe_bank(
    chunk_map: &ChunkMap,
    desired: (i32, i32),
    home: (i32, i32),
) -> Option<(i32, i32)> {
    // Radius 0 first — if desired itself is good, take it.
    for r in 0..=PROJECT_MAX_RADIUS {
        for dx in -r..=r {
            for dy in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let nx = desired.0 + dx;
                let ny = desired.1 + dy;
                let Some(kind) = chunk_map.tile_kind_at(nx, ny) else {
                    continue;
                };
                if !kind.is_passable() || kind.is_water_like() {
                    continue;
                }
                if same_bank_bfs(chunk_map, home, (nx, ny)) {
                    return Some((nx, ny));
                }
            }
        }
    }
    None
}

/// Sample a small window around `center` and infer the dominant axis of
/// any river tiles inside it. Returns `None` when no river is within
/// `radius`. Cheap and deterministic — counts River tiles by axis bucket.
pub fn river_orientation_near(
    chunk_map: &ChunkMap,
    center: (i32, i32),
    radius: i32,
) -> Option<RiverAxis> {
    let mut ns: u32 = 0;
    let mut ew: u32 = 0;
    let mut nw_se: u32 = 0;
    let mut ne_sw: u32 = 0;
    let mut river_seen = false;
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            let nx = center.0 + dx;
            let ny = center.1 + dy;
            let Some(kind) = chunk_map.tile_kind_at(nx, ny) else {
                continue;
            };
            if !matches!(kind, crate::world::tile::TileKind::River) {
                continue;
            }
            river_seen = true;
            // For each river tile, count which axis its neighbours align
            // with: count adjacent river cells along each of the four
            // axes.
            for (adx, ady, axis) in [
                (0, 1, 0u8),  // NS
                (0, -1, 0),
                (1, 0, 1),    // EW
                (-1, 0, 1),
                (1, 1, 2),    // NwSe (diagonal: dx==dy)
                (-1, -1, 2),
                (1, -1, 3),   // NeSw (diagonal: dx==-dy)
                (-1, 1, 3),
            ] {
                let mx = nx + adx;
                let my = ny + ady;
                if let Some(k) = chunk_map.tile_kind_at(mx, my) {
                    if matches!(k, crate::world::tile::TileKind::River) {
                        match axis {
                            0 => ns += 1,
                            1 => ew += 1,
                            2 => nw_se += 1,
                            _ => ne_sw += 1,
                        }
                    }
                }
            }
        }
    }
    if !river_seen {
        return None;
    }
    let (mut best, mut best_count) = (RiverAxis::NS, ns);
    for (axis, count) in [
        (RiverAxis::EW, ew),
        (RiverAxis::NwSe, nw_se),
        (RiverAxis::NeSw, ne_sw),
    ] {
        if count > best_count {
            best = axis;
            best_count = count;
        }
    }
    // No connected river tiles found within the window — single isolated
    // river cell. Default to NS as a stable fallback.
    if best_count == 0 {
        return Some(RiverAxis::NS);
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk(kind: TileKind) -> Chunk {
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[8u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    fn flat_map(chunk_radius: i32, kind: TileKind) -> ChunkMap {
        let mut m = ChunkMap::default();
        for cy in -chunk_radius..=chunk_radius {
            for cx in -chunk_radius..=chunk_radius {
                m.0.insert(ChunkCoord(cx, cy), flat_chunk(kind));
            }
        }
        m
    }

    fn write_river_strip(m: &mut ChunkMap, x: i32, y_range: std::ops::RangeInclusive<i32>) {
        for y in y_range {
            m.set_tile(
                x,
                y,
                0,
                TileData {
                    kind: TileKind::River,
                    ..Default::default()
                },
            );
        }
    }

    #[test]
    fn same_bank_reachable_on_dry_ground() {
        let m = flat_map(1, TileKind::Grass);
        assert!(same_bank_bfs(&m, (0, 0), (5, 5)));
    }

    #[test]
    fn river_blocks_same_bank() {
        let mut m = flat_map(1, TileKind::Grass);
        // NS river at x = 0 spanning enough of the region that BFS can't
        // detour around within the node cap.
        write_river_strip(&mut m, 0, -30..=30);
        assert!(!same_bank_bfs(&m, (-3, 0), (3, 0)));
        // Same bank still reachable.
        assert!(same_bank_bfs(&m, (-3, 0), (-1, 5)));
    }

    #[test]
    fn project_steers_off_river() {
        let mut m = flat_map(1, TileKind::Grass);
        // Long enough that BFS can't go around within the node cap.
        write_river_strip(&mut m, 0, -30..=30);
        // Desired tile sits inside the river; home east of it.
        let projected = project_to_safe_bank(&m, (0, 3), (3, 3));
        assert!(projected.is_some());
        let p = projected.unwrap();
        let kind = m.tile_kind_at(p.0, p.1).unwrap();
        assert!(kind.is_passable() && !kind.is_water_like());
        // Projection sits on the east bank (same side as home).
        assert!(p.0 > 0, "projected onto wrong bank: {:?}", p);
    }

    #[test]
    fn project_returns_none_with_no_safe_candidate() {
        // No loaded chunks at all — every `tile_kind_at` returns None and
        // the spiral can't find a candidate.
        let m = ChunkMap::default();
        assert!(project_to_safe_bank(&m, (0, 0), (0, 0)).is_none());
    }

    #[test]
    fn orientation_detects_ns_river() {
        let mut m = flat_map(1, TileKind::Grass);
        write_river_strip(&mut m, 0, -8..=8);
        assert_eq!(river_orientation_near(&m, (3, 0), 6), Some(RiverAxis::NS));
    }

    #[test]
    fn orientation_none_without_river() {
        let m = flat_map(1, TileKind::Grass);
        assert_eq!(river_orientation_near(&m, (0, 0), 4), None);
    }
}
