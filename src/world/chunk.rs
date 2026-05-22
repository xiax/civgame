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

#[derive(Clone)]
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
    /// Chebyshev distance (in tiles) from each surface (lx, ly) to the
    /// nearest river tile. `u8::MAX` means "no river within feather radius";
    /// otherwise the value is `0..=RIVER_FEATHER_DIST` (where 0 is on the
    /// channel itself). Populated by `generate_chunk_from_globe` while it
    /// stamps river polylines, so no extra spatial walks at query time.
    pub surface_river_distance: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
    /// Solid bed/ground Z at each (lx, ly). Equals `surface_z` for dry tiles;
    /// for wet columns it sits below the water surface by the column depth.
    /// `surface_z` keeps its meaning (rendered top = water surface for wet
    /// tiles); this is the *additive* solid-ground accessor (Phase 2).
    pub surface_ground_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
    /// Water column depth in Z-units (sub-z `f32`). `0.0` = dry.
    pub surface_water_depth: Box<[[f32; CHUNK_SIZE]; CHUNK_SIZE]>,
    /// Hydrology reservoir id this column belongs to (`u32::MAX` = none).
    pub surface_reservoir_id: Box<[[u32; CHUNK_SIZE]; CHUNK_SIZE]>,
    pub entities: Vec<Entity>,
    pub aggregate: ChunkAggregate,
    pub is_active: bool,
}

/// Lightweight read-only view of a single water column.
#[derive(Clone, Copy, Debug)]
pub struct WaterColumn {
    /// Rendered water-surface Z (== `surface_z`).
    pub level_z: i32,
    /// Solid bed Z (== `ground_z`).
    pub bed_z: i32,
    /// Depth in Z-units (`0.0` = dry).
    pub depth: f32,
    pub kind: TileKind,
    pub reservoir_id: u32,
}

impl Chunk {
    /// Construct a chunk that has no river-proximity data — equivalent to
    /// "no river anywhere in this chunk." Used by tests, pathfinding fixtures,
    /// and any caller that doesn't materialise river edges.
    pub fn new(
        surface_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_kind: Box<[[TileKind; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_fertility: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
    ) -> Self {
        let surface_river_distance = Box::new([[u8::MAX; CHUNK_SIZE]; CHUNK_SIZE]);
        Self::new_with_rivers(
            surface_z,
            surface_kind,
            surface_fertility,
            surface_river_distance,
        )
    }

    /// Back-compat constructor: derives dry water columns (ground == surface,
    /// depth 0, no reservoir). Used by tests/fixtures and any caller that
    /// doesn't materialise hydrology.
    pub fn new_with_rivers(
        surface_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_kind: Box<[[TileKind; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_fertility: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_river_distance: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
    ) -> Self {
        let surface_ground_z = surface_z.clone();
        let surface_water_depth = Box::new([[0.0f32; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_reservoir_id = Box::new([[u32::MAX; CHUNK_SIZE]; CHUNK_SIZE]);
        Self::new_hydro(
            surface_z,
            surface_kind,
            surface_fertility,
            surface_river_distance,
            surface_ground_z,
            surface_water_depth,
            surface_reservoir_id,
        )
    }

    /// Full constructor with explicit water-column data (used by
    /// `generate_chunk_from_globe`).
    #[allow(clippy::too_many_arguments)]
    pub fn new_hydro(
        surface_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_kind: Box<[[TileKind; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_fertility: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_river_distance: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_ground_z: Box<[[i8; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_water_depth: Box<[[f32; CHUNK_SIZE]; CHUNK_SIZE]>,
        surface_reservoir_id: Box<[[u32; CHUNK_SIZE]; CHUNK_SIZE]>,
    ) -> Self {
        Self {
            deltas: AHashMap::new(),
            surface_z,
            surface_kind,
            surface_fertility,
            surface_river_distance,
            surface_ground_z,
            surface_water_depth,
            surface_reservoir_id,
            entities: Vec::new(),
            aggregate: ChunkAggregate::default(),
            is_active: true,
        }
    }

    /// Chebyshev distance (in tiles) from (lx, ly) to the nearest river.
    /// `u8::MAX` if no river is within the feather radius.
    pub fn river_distance_at(&self, lx: usize, ly: usize) -> u8 {
        self.surface_river_distance[ly][lx]
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
            // A removed surface re-exposes solid ground: keep the dry
            // invariant (ground == surface, no water column). Runtime water
            // (Phase 3) overlays via RuntimeWater, not set_delta.
            self.surface_ground_z[ly][lx] = self.surface_z[ly][lx];
            self.surface_water_depth[ly][lx] = 0.0;
            self.surface_reservoir_id[ly][lx] = u32::MAX;
        } else if data.kind != TileKind::Air && z >= cur_surf {
            self.surface_z[ly][lx] = z as i8;
            self.surface_kind[ly][lx] = data.kind;
            // A built/dug tile is dry land of its own kind.
            self.surface_ground_z[ly][lx] = z as i8;
            self.surface_water_depth[ly][lx] = 0.0;
            self.surface_reservoir_id[ly][lx] = u32::MAX;
        }
    }

    /// Solid bed Z at (lx, ly). Equals `surface_z` for dry tiles.
    pub fn ground_z_at(&self, lx: usize, ly: usize) -> i32 {
        self.surface_ground_z[ly][lx] as i32
    }

    /// Water column depth in Z-units at (lx, ly). `0.0` = dry.
    pub fn water_depth_at(&self, lx: usize, ly: usize) -> f32 {
        self.surface_water_depth[ly][lx]
    }

    /// Reservoir id at (lx, ly). `u32::MAX` = none.
    pub fn reservoir_id_at(&self, lx: usize, ly: usize) -> u32 {
        self.surface_reservoir_id[ly][lx]
    }

    /// Read-only water-column view at (lx, ly).
    pub fn water_column_at(&self, lx: usize, ly: usize) -> WaterColumn {
        WaterColumn {
            level_z: self.surface_z[ly][lx] as i32,
            bed_z: self.surface_ground_z[ly][lx] as i32,
            depth: self.surface_water_depth[ly][lx],
            kind: self.surface_tile_kind(lx, ly),
            reservoir_id: self.surface_reservoir_id[ly][lx],
        }
    }

    /// Overlay a persistent runtime water column (Phase 3 `RuntimeWater`,
    /// Phase 5 fluid sim) at (lx, ly): solid bed at `ground_z`, `depth` (> 0)
    /// Z-units of water on top, rendered surface = `bed + ceil(depth)` —
    /// mirroring the Phase 2 worldgen stamp (`bed = water_surf - depth.ceil()`)
    /// so a runtime-flooded column satisfies the same `bed <= surf, depth > 0`
    /// invariant a worldgen river does. The surface kind flips to `Water`;
    /// Phase 6 refines fresh/brackish/salt via `water_kind_at` + salinity.
    /// `depth <= 0` is a no-op (drained cells are removed from `RuntimeWater`,
    /// never stamped). Returns `true` iff the column actually changed (caller
    /// emits `TileChangedEvent` only then).
    pub fn apply_water_column(
        &mut self,
        lx: usize,
        ly: usize,
        ground_z: i8,
        depth: f32,
        reservoir_id: u32,
    ) -> bool {
        if depth <= 0.0 {
            return false;
        }
        let bed = (ground_z as i32).clamp(Z_MIN, Z_MAX);
        let surf = (bed + depth.ceil() as i32).clamp(Z_MIN, Z_MAX);
        let changed = self.surface_z[ly][lx] as i32 != surf
            || self.surface_ground_z[ly][lx] as i32 != bed
            || (self.surface_water_depth[ly][lx] - depth).abs() > 1e-4
            || self.surface_reservoir_id[ly][lx] != reservoir_id
            || self.surface_kind[ly][lx] != TileKind::Water;
        self.surface_z[ly][lx] = surf as i8;
        self.surface_ground_z[ly][lx] = bed as i8;
        self.surface_water_depth[ly][lx] = depth;
        self.surface_reservoir_id[ly][lx] = reservoir_id;
        self.surface_kind[ly][lx] = TileKind::Water;
        changed
    }
}

#[derive(Resource, Default, Clone)]
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

    /// Solid bed/ground Z at world tile (tx, ty). Equals `surface_z_at` for
    /// dry tiles; for wet columns it returns the bed below the water surface.
    /// Returns `Z_MIN - 1` if the chunk is not loaded. Use this (not
    /// `surface_z_at`) wherever solid terrain elevation is compared — see the
    /// Phase 0 audit list in `src/world/CLAUDE.md`.
    pub fn ground_z_at(&self, tile_x: i32, tile_y: i32) -> i32 {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get(&coord)
            .map(|c| c.surface_ground_z[ly][lx] as i32)
            .unwrap_or(Z_MIN - 1)
    }

    /// Water column depth (Z-units, sub-z) at (tx, ty). `0.0` = dry / unloaded.
    pub fn water_depth_at(&self, tile_x: i32, tile_y: i32) -> f32 {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get(&coord)
            .map(|c| c.surface_water_depth[ly][lx])
            .unwrap_or(0.0)
    }

    /// Water-surface Z at (tx, ty) if the column is wet, else `None`.
    pub fn water_level_at(&self, tile_x: i32, tile_y: i32) -> Option<f32> {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        let c = self.0.get(&coord)?;
        if c.surface_water_depth[ly][lx] > 0.0 {
            Some(c.surface_z[ly][lx] as f32)
        } else {
            None
        }
    }

    /// Reservoir id at (tx, ty). `u32::MAX` = none / unloaded.
    pub fn reservoir_id_at(&self, tile_x: i32, tile_y: i32) -> u32 {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get(&coord)
            .map(|c| c.surface_reservoir_id[ly][lx])
            .unwrap_or(u32::MAX)
    }

    /// Read-only water-column view at (tx, ty), if the chunk is loaded.
    pub fn water_column_at(&self, tile_x: i32, tile_y: i32) -> Option<WaterColumn> {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0.get(&coord).map(|c| c.water_column_at(lx, ly))
    }

    /// World-tile wrapper over `Chunk::apply_water_column` (Phase 3 restamp,
    /// Phase 5 fluid sim). Returns `false` (no-op) if the chunk isn't loaded.
    pub fn apply_water_column(
        &mut self,
        tile_x: i32,
        tile_y: i32,
        ground_z: i8,
        depth: f32,
        reservoir_id: u32,
    ) -> bool {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get_mut(&coord)
            .map(|c| c.apply_water_column(lx, ly, ground_z, depth, reservoir_id))
            .unwrap_or(false)
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

    /// Standable Z at (tx, ty) closest to `hint_z`. Searches outward
    /// (hint, hint±1, hint±2, …) within `[Z_MIN, Z_MAX]`. Falls back to
    /// `surface_z_at(tx, ty)` if no Z slice is standable (e.g. mid-air
    /// click); callers receive a sensible value either way.
    pub fn nearest_standable_z(&self, tile_x: i32, tile_y: i32, hint_z: i32) -> i32 {
        if self.passable_at(tile_x, tile_y, hint_z) {
            return hint_z;
        }
        let max_radius = (Z_MAX - Z_MIN).max(1);
        for d in 1..=max_radius {
            let up = hint_z + d;
            if up <= Z_MAX && self.passable_at(tile_x, tile_y, up) {
                return up;
            }
            let dn = hint_z - d;
            if dn >= Z_MIN && self.passable_at(tile_x, tile_y, dn) {
                return dn;
            }
        }
        self.surface_z_at(tile_x, tile_y)
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

    /// Number of open `Air` / `Ramp` Z-levels directly above the surface at
    /// `(tile_x, tile_y)`, counted until the first solid tile (or `Z_MAX`).
    /// Drives vehicle vertical-clearance gating — a vehicle spanning
    /// `height_z` world Z-levels needs `vertical_clearance_at >= height_z`
    /// over every footprint tile or it fails at the overhang / tunnel mouth.
    /// Returns 0 for an unloaded chunk.
    pub fn vertical_clearance_at(&self, tile_x: i32, tile_y: i32) -> i32 {
        let surf = self.surface_z_at(tile_x, tile_y);
        if surf < Z_MIN {
            return 0;
        }
        let mut clear = 0;
        let mut z = surf + 1;
        while z <= Z_MAX {
            if matches!(
                self.tile_at(tile_x, tile_y, z).kind,
                TileKind::Air | TileKind::Ramp
            ) {
                clear += 1;
                z += 1;
            } else {
                break;
            }
        }
        clear
    }

    /// Profile-aware passability. `Land` is `passable_at` verbatim.
    /// `Amphibious` additionally accepts a **water-surface cell** as
    /// standable: a wet column (`water_depth_at > 0`) whose surface Z is
    /// `tz`, whose foot tile is `Water`/`River`, with `Air`/`Ramp`
    /// headspace above. Lets a swimmer's path plan over open water.
    pub fn passable_for(
        &self,
        tile_x: i32,
        tile_y: i32,
        tz: i32,
        profile: crate::pathfinding::tile_cost::TraversalProfile,
    ) -> bool {
        if self.passable_at(tile_x, tile_y, tz) {
            return true;
        }
        if profile != crate::pathfinding::tile_cost::TraversalProfile::Amphibious {
            return false;
        }
        if tz < Z_MIN || tz > Z_MAX {
            return false;
        }
        // Must be the wet column's surface Z.
        if self.water_depth_at(tile_x, tile_y) <= 0.0 || self.surface_z_at(tile_x, tile_y) != tz {
            return false;
        }
        let here = self.tile_at(tile_x, tile_y, tz).kind;
        if !matches!(here, TileKind::Water | TileKind::River) {
            return false;
        }
        let head = self.tile_at(tile_x, tile_y, tz + 1).kind;
        matches!(head, TileKind::Air | TileKind::Ramp)
    }

    /// Profile-aware 3D step. `Land` is `passable_step_3d` verbatim;
    /// `Amphibious` validates the destination via `passable_for`.
    pub fn passable_step_for(
        &self,
        from: (i32, i32, i32),
        to: (i32, i32, i32),
        profile: crate::pathfinding::tile_cost::TraversalProfile,
    ) -> bool {
        let (sx, sy, sz) = from;
        let (dx, dy, dz) = to;
        let (ddx, ddy, ddz) = (dx - sx, dy - sy, dz - sz);
        if ddx.abs() > 1 || ddy.abs() > 1 || ddz.abs() > 1 {
            return false;
        }
        if ddx == 0 && ddy == 0 && ddz == 0 {
            return false;
        }
        if sz < Z_MIN || sz > Z_MAX {
            return false;
        }
        self.passable_for(dx, dy, dz, profile)
    }

    /// 3D step passability for an agent moving from (sx,sy,sz) to (dx,dy,dz).
    /// 8-connected in XY, |Δz| ≤ 1. Both endpoints must be standable.
    pub fn passable_step_3d(&self, from: (i32, i32, i32), to: (i32, i32, i32)) -> bool {
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

    /// Chebyshev distance (in tiles) from (tx, ty) to the nearest river,
    /// capped at `RIVER_FEATHER_DIST`. Returns `u8::MAX` when the chunk is
    /// unloaded or no river lies within the feather. Cheap O(1).
    pub fn river_distance_at(&self, tile_x: i32, tile_y: i32) -> u8 {
        let (coord, lx, ly) = Self::coord_and_local(tile_x, tile_y);
        self.0
            .get(&coord)
            .map(|c| c.river_distance_at(lx, ly))
            .unwrap_or(u8::MAX)
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
    fn passable_for_amphibious_accepts_water_surface() {
        use crate::pathfinding::tile_cost::TraversalProfile;
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), make_chunk(0));
        // Wet (5,5): bed at z=-2, depth 2 → water surface at z=0.
        map.0
            .get_mut(&ChunkCoord(0, 0))
            .unwrap()
            .apply_water_column(5, 5, -2, 2.0, 0);
        assert_eq!(map.surface_z_at(5, 5), 0);
        // Land never stands on water; Amphibious stands on the surface.
        assert!(!map.passable_at(5, 5, 0));
        assert!(!map.passable_for(5, 5, 0, TraversalProfile::Land));
        assert!(map.passable_for(5, 5, 0, TraversalProfile::Amphibious));
        // A dry grass tile stays passable under both profiles.
        assert!(map.passable_for(8, 8, 0, TraversalProfile::Land));
        assert!(map.passable_for(8, 8, 0, TraversalProfile::Amphibious));
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

    #[test]
    fn fill_above_surface_raises_surface_z() {
        // Inverse of carve_at_surface_lowers_surface_z. fill_tile relies on
        // set_delta updating surface_z upward when a non-Air tile is written
        // at z >= cur_surf.
        let mut map = make_map_with_chunk(5);
        assert_eq!(map.surface_z_at(5, 5), 5);
        map.set_tile(
            5,
            5,
            6,
            TileData {
                kind: TileKind::Dirt,
                ..Default::default()
            },
        );
        assert_eq!(map.surface_z_at(5, 5), 6);
    }

    // --- Phase 3: persistent runtime water column overlay ---

    #[test]
    fn apply_water_column_overlays_wet_column() {
        // Dry Grass chunk at z=2; flood it 3 Z-units deep (a dam pool).
        let mut chunk = make_chunk(2);
        assert!(chunk.apply_water_column(5, 3, 2, 3.0, 7));

        // surface = bed + ceil(depth); the Phase 2 worldgen invariant.
        assert_eq!(chunk.surface_z[3][5] as i32, 5);
        assert_eq!(chunk.ground_z_at(5, 3), 2);
        assert_eq!(chunk.water_depth_at(5, 3), 3.0);
        assert_eq!(chunk.reservoir_id_at(5, 3), 7);
        assert_eq!(chunk.surface_tile_kind(5, 3), TileKind::Water);

        let col = chunk.water_column_at(5, 3);
        assert!(col.bed_z <= col.level_z, "wet bed above surface");
        assert!(col.depth > 0.0);
        assert_eq!(col.kind, TileKind::Water);
        assert_eq!(col.reservoir_id, 7);
    }

    #[test]
    fn apply_water_column_idempotent() {
        let mut chunk = make_chunk(2);
        assert!(chunk.apply_water_column(5, 3, 2, 3.0, 7));
        // Re-applying identical state reports no change → caller skips the
        // TileChangedEvent (sleeping cells stay cheap on every restamp).
        assert!(!chunk.apply_water_column(5, 3, 2, 3.0, 7));
        // A real change (deeper pool) re-reports.
        assert!(chunk.apply_water_column(5, 3, 2, 4.0, 7));
    }

    #[test]
    fn apply_water_column_zero_depth_is_noop() {
        // Drained cells are *removed* from RuntimeWater, never stamped with
        // depth 0 — apply must leave the dry terrain untouched.
        let mut chunk = make_chunk(2);
        assert!(!chunk.apply_water_column(5, 3, 2, 0.0, 7));
        assert_eq!(chunk.surface_tile_kind(5, 3), TileKind::Grass);
        assert_eq!(chunk.water_depth_at(5, 3), 0.0);
        assert_eq!(chunk.reservoir_id_at(5, 3), u32::MAX);
    }

    #[test]
    fn apply_water_column_via_chunkmap_and_unloaded() {
        let mut map = make_map_with_chunk(2);
        assert!(map.apply_water_column(5, 3, 2, 2.5, 4));
        assert_eq!(map.water_depth_at(5, 3), 2.5);
        assert_eq!(map.surface_z_at(5, 3), 2 + 3); // ceil(2.5) = 3
        assert_eq!(map.ground_z_at(5, 3), 2);
        // Unloaded chunk: no-op, no panic.
        assert!(!map.apply_water_column(9999, 9999, 0, 1.0, 0));
    }
}
