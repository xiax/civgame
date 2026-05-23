use crate::simulation::construction::{DoorMap, Wall, WallMap};
use crate::world::chunk::ChunkMap;
use bevy::prelude::Query;

/// 3D voxel LOS from `from` to `to` (each is `(tx, ty, foot_z)`).
///
/// Walks an integer voxel grid using 2D Bresenham on (x, y) with linearly
/// interpolated z. At each non-endpoint voxel, queries `chunk_map.tile_at`
/// and rejects if the tile is opaque (`Wall`, or a closed `Door`). Open doors
/// are transparent. Underground rock reads as `Wall` via the deltas-only
/// `tile_at`, so an unbroken stretch of solid earth correctly blocks sight.
pub fn has_los(
    chunk_map: &ChunkMap,
    door_map: &DoorMap,
    from: (i32, i32, i8),
    to: (i32, i32, i8),
) -> bool {
    walk_ray(from, to, |(x, y, z)| {
        let kind = chunk_map.tile_at(x, y, z as i32).kind;
        if kind.is_opaque() {
            return true;
        }
        if let Some(door) = door_map.0.get(&(x, y)) {
            if !door.open {
                return true;
            }
        }
        false
    })
}

/// Faction-aware LOS for fog / resource vision. Same ray walk as `has_los`,
/// but skips opacity for the observer's own constructed walls (`WallMap`
/// entry with `owner_faction == Some(observer_faction)`) and own doors
/// (open or closed). Natural rock (no `WallMap` entry OR `owner_faction == None`)
/// and enemy walls / closed enemy doors still block. Combat / sound /
/// projectile LOS keep calling `has_los` — they MUST treat own walls as
/// solid so a wall actually defends.
pub fn has_vision_los(
    chunk_map: &ChunkMap,
    wall_map: &WallMap,
    door_map: &DoorMap,
    wall_q: &Query<&Wall>,
    from: (i32, i32, i8),
    to: (i32, i32, i8),
    observer_faction: u32,
) -> bool {
    walk_ray(from, to, |(x, y, z)| {
        let kind = chunk_map.tile_at(x, y, z as i32).kind;
        if kind.is_opaque() {
            // Own constructed wall? Transparent to own vision.
            if let Some(&wall_entity) = wall_map.0.get(&(x, y)) {
                if let Ok(wall) = wall_q.get(wall_entity) {
                    if wall.owner_faction == Some(observer_faction) {
                        return false;
                    }
                }
            }
            // Natural rock (no WallMap entry) or enemy wall blocks.
            return true;
        }
        if let Some(door) = door_map.0.get(&(x, y)) {
            // Own door is transparent regardless of open/closed state.
            // Foreign closed door blocks; foreign open door is transparent.
            if door.faction_id == observer_faction {
                return false;
            }
            if !door.open {
                return true;
            }
        }
        false
    })
}

/// Shared inner walker. Calls `blocker((x, y, z))` at every non-endpoint
/// voxel along the 3D Bresenham ray; returns `false` (LOS blocked) the
/// moment any voxel reports `true`, else `true` (LOS clear). Endpoints
/// are skipped (caller's own tiles can't block).
fn walk_ray(
    from: (i32, i32, i8),
    to: (i32, i32, i8),
    mut blocker: impl FnMut((i32, i32, i8)) -> bool,
) -> bool {
    let (mut x0, mut y0) = (from.0, from.1);
    let (x1, y1) = (to.0, to.1);
    // Eye height: ray walks 1 voxel above the standing tile at both endpoints,
    // so a 1-tile terrain rise doesn't read as below-surface Wall and block sight.
    // i32 widening keeps Z_MAX safely below i8::MAX after the +1 bump.
    let from_z = (from.2 as i32 + 1) as f32;
    let to_z = (to.2 as i32 + 1) as f32;

    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;

    let total_dist = (dx.max(dy)).max(1) as f32;
    let mut steps = 0i32;

    loop {
        steps += 1;

        // Skip endpoint voxels (caller's tiles); only test voxels in between.
        if (x0, y0) != (from.0, from.1) && (x0, y0) != (to.0, to.1) {
            let t = steps as f32 / total_dist;
            let ray_z = (from_z + t * (to_z - from_z)).round() as i32;
            let z_i8 = ray_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
            if blocker((x0, y0, z_i8)) {
                return false;
            }
        }

        if x0 == x1 && y0 == y1 {
            break;
        }

        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x0 += sx;
        }
        if e2 < dx {
            err += dx;
            y0 += sy;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk_map() -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    #[test]
    fn same_tile_has_los() {
        let m = flat_chunk_map();
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 0, 0), (0, 0, 0)));
    }

    #[test]
    fn flat_terrain_always_has_los() {
        let m = flat_chunk_map();
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 0, 0), (10, 5, 0)));
    }

    #[test]
    fn small_terrain_rise_does_not_block_los() {
        let mut m = ChunkMap::default();
        let mut surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        surface_z[0][5] = 1;
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 0, 0), (10, 0, 0)));
    }

    #[test]
    fn underground_tunnel_has_los_along_its_length() {
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[5i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Stone; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        for x in 0..10i32 {
            m.set_tile(
                x,
                10,
                -3,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
            m.set_tile(
                x,
                10,
                -4,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
        }
        let d = DoorMap::default();
        assert!(has_los(&m, &d, (0, 10, -4), (9, 10, -4)));
    }

    #[test]
    fn solid_rock_between_underground_agents_blocks_los() {
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[5i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Stone; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        let d = DoorMap::default();
        assert!(!has_los(&m, &d, (0, 0, -4), (9, 0, -4)));
    }

    #[test]
    fn surface_to_underground_blocked_by_overhead_rock() {
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        let d = DoorMap::default();
        assert!(!has_los(&m, &d, (0, 0, 0), (10, 0, -4)));
    }
}
