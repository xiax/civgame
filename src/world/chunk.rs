use super::tile::{TileData, TileKind};
use ahash::AHashMap;
use bevy::prelude::*;

pub const CHUNK_SIZE: usize = 32;
pub const CHUNK_HEIGHT: usize = 32; // total discrete Z levels
pub const Z_MIN: i32 = -16; // deepest underground
pub const Z_MAX: i32 = 15; // highest peak

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ChunkCoord(pub i32, pub i32);

impl ChunkCoord {
    pub fn from_world(world_x: f32, world_y: f32, tile_size: f32) -> Self {
        let tile_x = (world_x / tile_size).floor() as i32;
        let tile_y = (world_y / tile_size).floor() as i32;
        ChunkCoord(
            tile_x.div_euclid(CHUNK_SIZE as i32),
            tile_y.div_euclid(CHUNK_SIZE as i32),
        )
    }

    pub fn chebyshev_dist(self, other: ChunkCoord) -> i32 {
        (self.0 - other.0).abs().max((self.1 - other.1).abs())
    }
}

/// Aggregate stats for chunks simulated at LOD::Aggregate level.
#[derive(Default, Clone, Copy)]
pub struct ChunkAggregate {
    pub pop_count: u32,
    pub avg_hunger: u8,
    pub avg_mood: i8,
    pub food_produced: f32,
    pub food_consumed: f32,
    pub employed_count: u32,
}

pub struct Chunk {
    /// Sparse overrides from procedurally generated state.
    /// Key: (local_x as u8, local_y as u8, z_local as u8) where z_local = z - Z_MIN.
    pub deltas: AHashMap<(u8, u8, u8), TileData>,
    /// Topmost non-Air Z at each (lx, ly). Stored as i8 (fits Z_MIN..=Z_MAX).
    pub surface_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
    /// Procedurally computed surface tile kind at each (lx, ly).
    pub surface_kind: Box<[[TileKind; CHUNK_SIZE]; CHUNK_SIZE]>,
    /// Procedurally computed surface fertility at each (lx, ly).
    pub surface_fertility: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
    pub entities: Vec<Entity>,
    pub aggregate: ChunkAggregate,
    pub is_active: bool,
}

impl Chunk {
    pub fn new(
        surface_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_kind: Box<[[TileKind; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_fertility: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
    ) -> Self {
        Self {
            deltas: AHashMap::new(),
            surface_z,
            surface_kind,
            surface_fertility,
            entities: Vec::new(),
            aggregate: ChunkAggregate::default(),
            is_active: true,
        }
    }

    /// Read the tile delta override, if any.
    pub fn delta(&self, lx: usize, ly: usize, z_local: usize) -> Option<TileData> {
        self.deltas
            .get(&(lx as u8, ly as u8, z_local as u8))
            .copied()
    }

    /// Tile lookup at arbitrary Z using deltas + surface caches only.
    /// Doesn't consult procedural cave noise — uncarved underground reads as
    /// solid Wall. Use `terrain::tile_at_3d` if you need procgen-aware reads.
    pub fn tile_at_local(&self, lx: usize, ly: usize, z: i32) -> TileData {
        let z_local = (z - Z_MIN) as usize;
        if let Some(d) = self.delta(lx, ly, z_local) {
            return d;
        }
        let surf_z = self.surface_z[ly][lx] as i32;
        if z > surf_z {
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            }
        } else if z == surf_z {
            TileData {
                kind: self.surface_kind[ly][lx],
                fertility: self.surface_fertility[ly][lx],
                ..Default::default()
            }
        } else {
            TileData {
                kind: TileKind::Wall,
                ..Default::default()
            }
        }
    }

    /// Effective surface tile kind: delta overrides the procedural cache.
    pub fn surface_tile_kind(&self, lx: usize, ly: usize) -> TileKind {
        let surf_z = self.surface_z[ly][lx] as i32;
        let z_local = (surf_z - Z_MIN) as usize;
        if let Some(d) = self.delta(lx, ly, z_local) {
            d.kind
        } else {
            self.surface_kind[ly][lx]
        }
    }

    /// Passability at (lx, ly) using cached kind and delta overrides.
    pub fn is_locally_passable(&self, lx: usize, ly: usize) -> bool {
        self.surface_tile_kind(lx, ly).is_passable()
    }

    /// Effective surface fertility: delta overrides the procedural cache.
    pub fn surface_fertility_at(&self, lx: usize, ly: usize) -> u8 {
        let surf_z = self.surface_z[ly][lx] as i32;
        let z_local = (surf_z - Z_MIN) as usize;
        if let Some(d) = self.delta(lx, ly, z_local) {
            d.fertility
        } else {
            self.surface_fertility[ly][lx]
        }
    }

    /// Write a tile override and update the surface_z / surface_kind caches.
    pub fn set_delta(&mut self, lx: usize, ly: usize, z: i32, data: TileData) {
        let z_local = (z - Z_MIN) as usize;
        self.deltas
            .insert((lx as u8, ly as u8, z_local as u8), data);

        let cur_surf = self.surface_z[ly][lx] as i32;

        if data.kind == TileKind::Air && z == cur_surf {
            // Surface tile removed — scan downward through deltas to find new top.
            let mut new_surf = Z_MIN - 1;
            for z2 in (Z_MIN..cur_surf).rev() {
                let zl2 = (z2 - Z_MIN) as usize;
                if let Some(d) = self.deltas.get(&(lx as u8, ly as u8, zl2 as u8)) {
                    if d.kind != TileKind::Air {
                        new_surf = z2;
                        break;
                    }
                } else {
                    new_surf = z2;
                    break;
                }
            }
            self.surface_z[ly][lx] = new_surf.max(Z_MIN) as i8;
            // Surface kind changes too — but without WorldGen we use Air as placeholder.
            self.surface_kind[ly][lx] = TileKind::Air;
        } else if data.kind != TileKind::Air && z >= cur_surf {
            self.surface_z[ly][lx] = z as i8;
            self.surface_kind[ly][lx] = data.kind;
        }
    }
}

#[derive(Resource, Default)]
pub struct ChunkMap(pub AHashMap<ChunkCoord, Chunk>);

impl ChunkMap {
    fn coord_and_local(tile_x: i32, tile_y: i32) -> (ChunkCoord, usize, usize) {
        let coord = ChunkCoord(
            tile_x.div_euclid(CHUNK_SIZE as i32),
            tile_y.div_euclid(CHUNK_SIZE as i32),
        );
        let lx = tile_x.rem_euclid(CHUNK_SIZE as i32) as usize;
        let ly = tile_y.rem_euclid(CHUNK_SIZE as i32) as usize;
        (coord, lx, ly)
    }

    /// Surface Z at world tile (tx, ty). Returns Z_MIN - 1 if chunk not loaded.
    pub fn surface_z_at(&self, tile_x: i32, tile_y: i32) -> i32 {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get(&coord)
            .map(|c| c.surface_z[ly][lx] as i32)
            .unwrap_or(Z_MIN - 1)
    }

    /// Surface tile kind at (tx, ty). Returns None if chunk not loaded.
    pub fn tile_kind_at(&self, tile_x: i32, tile_y: i32) -> Option<TileKind> {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0.get(&coord).map(|c| c.surface_tile_kind(lx, ly))
    }

    /// Check the delta map for an override at (tx, ty, tz).
    /// Returns None if chunk not loaded or no delta present.
    pub fn tile_delta_at(&self, tile_x: i32, tile_y: i32, tz: i32) -> Option<TileData> {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        let z_local = (tz - Z_MIN) as usize;
        self.0.get(&coord)?.delta(lx, ly, z_local)
    }

    /// Apply a tile modification — writes a delta and updates the caches.
    pub fn set_tile(&mut self, tile_x: i32, tile_y: i32, tz: i32, data: TileData) {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        if let Some(chunk) = self.0.get_mut(&coord) {
            chunk.set_delta(lx, ly, tz, data);
        }
    }

    /// Tile lookup at world (tx, ty, tz) using deltas + surface caches.
    /// Returns Wall if the chunk is unloaded (treat-as-solid for safety).
    pub fn tile_at(&self, tile_x: i32, tile_y: i32, tz: i32) -> TileData {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        match self.0.get(&coord) {
            Some(c) => c.tile_at_local(lx, ly, tz),
            None => TileData {
                kind: TileKind::Wall,
                ..Default::default()
            },
        }
    }

    /// Is (tx, ty, tz) a tile an agent can stand on (foot-Z convention)?
    /// The tile at z must be a passable surface (Grass/Dirt/Ramp/etc.)
    /// AND the tile at z+1 must be empty headspace (Air or Ramp).
    pub fn passable_at(&self, tile_x: i32, tile_y: i32, tz: i32) -> bool {
        if tz < Z_MIN || tz > Z_MAX {
            return false;
        }
        let here = self.tile_at(tile_x, tile_y, tz);
        if !here.kind.is_passable() {
            return false;
        }
        let head = self.tile_at(tile_x, tile_y, tz + 1);
        matches!(head.kind, TileKind::Air | TileKind::Ramp)
    }

    /// 3D step passability for an agent moving from (sx,sy,sz) to (dx,dy,dz).
    /// 8-connected in XY, |Δz| ≤ 1. Both endpoints must be standable.
    pub fn passable_step_3d(
        &self,
        from: (i32, i32, i32),
        to: (i32, i32, i32),
    ) -> bool {
        let (sx, sy, sz) = from;
        let (dx, dy, dz) = to;
        let ddx = dx - sx;
        let ddy = dy - sy;
        let ddz = dz - sz;
        if ddx.abs() > 1 || ddy.abs() > 1 || ddz.abs() > 1 {
            return false;
        }
        if ddx == 0 && ddy == 0 && ddz == 0 {
            return false;
        }
        // Source sanity (skip when source is in unloaded chunk).
        if sz < Z_MIN || sz > Z_MAX {
            return false;
        }
        self.passable_at(dx, dy, dz)
    }

    /// Surface fertility at (tx, ty). Returns 0 if chunk not loaded.
    pub fn tile_fertility_at(&self, tile_x: i32, tile_y: i32) -> Option<u8> {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0.get(&coord).map(|c| c.surface_fertility_at(lx, ly))
    }

    /// Passability at (tx, ty) using cached surface data.
    pub fn is_passable(&self, tile_x: i32, tile_y: i32) -> bool {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get(&coord)
            .map(|c| c.is_locally_passable(lx, ly))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chunk(surf_z: i8) -> Chunk {
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    #[test]
    fn chunk_coord_from_world() {
        let coord = ChunkCoord::from_world(0.0, 0.0, 16.0);
        assert_eq!(coord, ChunkCoord(0, 0));

        let coord = ChunkCoord::from_world(32.0 * 16.0, 0.0, 16.0);
        assert_eq!(coord, ChunkCoord(1, 0));
    }

    #[test]
    fn chunk_delta_roundtrip() {
        let mut chunk = make_chunk(0);
        let data = TileData {
            kind: TileKind::Wall,
            ..Default::default()
        };
        chunk.set_delta(5, 3, 0, data);
        let d = chunk.delta(5, 3, (0 - Z_MIN) as usize);
        assert_eq!(d.unwrap().kind, TileKind::Wall);
    }

    #[test]
    fn surface_tile_kind_respects_delta() {
        let mut chunk = make_chunk(0);
        assert_eq!(chunk.surface_tile_kind(5, 3), TileKind::Grass);
        chunk.set_delta(
            5,
            3,
            0,
            TileData {
                kind: TileKind::Wall,
                ..Default::default()
            },
        );
        assert_eq!(chunk.surface_tile_kind(5, 3), TileKind::Wall);
        assert!(!chunk.is_locally_passable(5, 3));
    }

    #[test]
    fn chebyshev_dist() {
        let a = ChunkCoord(0, 0);
        let b = ChunkCoord(3, 2);
        assert_eq!(a.chebyshev_dist(b), 3);
    }

    #[test]
    fn z_constants_valid() {
        assert_eq!(Z_MAX - Z_MIN + 1, CHUNK_HEIGHT as i32);
    }

    fn make_map_with_chunk(surf_z: i8) -> ChunkMap {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), make_chunk(surf_z));
        map
    }

    #[test]
    fn passable_at_surface_grass() {
        // Grass surface at z=0, headspace at z=1 is implicitly Air (z>surface).
        let map = make_map_with_chunk(0);
        assert!(map.passable_at(5, 5, 0));
    }

    #[test]
    fn passable_at_solid_rock_rejects() {
        // Hill column with surface=5; below the surface, every Z is Wall.
        // Trying to stand at z=2 inside untunneled rock: foot=Wall (not passable).
        let map = make_map_with_chunk(5);
        assert!(!map.passable_at(5, 5, 2));
    }

    #[test]
    fn passable_at_carved_tunnel() {
        // Hill surface=5. Carve a tunnel cell at (x, y, 0):
        //   z=0 = Dirt (floor), z=1 = Air (headspace).
        let mut map = make_map_with_chunk(5);
        map.set_tile(
            5,
            5,
            1,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        map.set_tile(
            5,
            5,
            0,
            TileData {
                kind: TileKind::Dirt,
                ..Default::default()
            },
        );
        assert!(map.passable_at(5, 5, 0));
    }

    #[test]
    fn passable_at_no_headspace_rejects() {
        // Floor at z=0 (Dirt) but headspace z=1 is solid Wall — agent can't fit.
        let mut map = make_map_with_chunk(5);
        map.set_tile(
            5,
            5,
            0,
            TileData {
                kind: TileKind::Dirt,
                ..Default::default()
            },
        );
        // z=1 stays Wall (since surface=5, z=1 < surface defaults to Wall).
        assert!(!map.passable_at(5, 5, 0));
    }

    #[test]
    fn passable_step_3d_rejects_dz_without_ramp() {
        let map = make_map_with_chunk(0);
        // Surface tiles at z=0; stepping diagonally to z=1 with no ramp at either end.
        assert!(!map.passable_step_3d((5, 5, 0), (5, 6, 1)));
    }

    #[test]
    fn passable_step_3d_accepts_with_ramp() {
        let mut map = make_map_with_chunk(0);
        // Make (5, 6) a Ramp surface at z=1; headspace z=2 is implicit Air.
        map.set_tile(
            5,
            6,
            1,
            TileData {
                kind: TileKind::Ramp,
                ..Default::default()
            },
        );
        assert!(map.passable_step_3d((5, 5, 0), (5, 6, 1)));
    }

    #[test]
    fn carve_below_surface_preserves_surface_z() {
        let mut map = make_map_with_chunk(5);
        let surf_before = map.surface_z_at(5, 5);
        assert_eq!(surf_before, 5);
        // Carve at z=2 (below surface) — should NOT lower surface_z.
        map.set_tile(
            5,
            5,
            2,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        assert_eq!(map.surface_z_at(5, 5), 5);
    }

    #[test]
    fn carve_at_surface_lowers_surface_z() {
        let mut map = make_map_with_chunk(5);
        map.set_tile(
            5,
            5,
            5,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        assert_eq!(map.surface_z_at(5, 5), 4);
    }
}
