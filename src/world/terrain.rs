use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};

use super::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE, CHUNK_HEIGHT, Z_MIN, Z_MAX};
use super::globe::{Biome, Globe, WorldCell, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use super::tile::{TileData, TileKind};

pub const WORLD_CHUNKS_X: i32 = 16;
pub const WORLD_CHUNKS_Y: i32 = 16;
pub const TILE_SIZE: f32 = 16.0;

const WORLD_SEED: u32 = 42;

/// Both Perlin instances used for world generation, stored as a Bevy resource.
#[derive(Resource)]
pub struct WorldGen {
    pub surface: Perlin, // 2D surface height + tile kind (seed WORLD_SEED)
    pub cave:    Perlin, // 3D cave cavities (seed WORLD_SEED + 1)
}

impl WorldGen {
    pub fn new() -> Self {
        Self {
            surface: Perlin::default().set_seed(WORLD_SEED),
            cave:    Perlin::default().set_seed(WORLD_SEED + 1),
        }
    }
}

impl Default for WorldGen {
    fn default() -> Self { Self::new() }
}

/// Return biome tile thresholds: (water_t, grass_t, farm_t, forest_t).
pub fn biome_thresholds(biome: Biome) -> (f32, f32, f32, f32) {
    match biome {
        Biome::Ocean     => (0.90, 0.95, 0.97, 0.99),
        Biome::Tundra    => (0.15, 0.80, 0.85, 0.90),
        Biome::Taiga     => (0.18, 0.35, 0.40, 0.70),
        Biome::Temperate => (0.22, 0.45, 0.60, 0.75),
        Biome::Grassland => (0.18, 0.60, 0.75, 0.82),
        Biome::Tropical  => (0.20, 0.30, 0.35, 0.78),
        Biome::Desert    => (0.10, 0.65, 0.68, 0.70),
        Biome::Mountain  => (0.12, 0.25, 0.28, 0.40),
    }
}

/// Fractional surface noise value at (tx, ty). Range [0, 1].
fn surface_v(tx: i32, ty: i32, surface: &Perlin) -> f32 {
    let nx = tx as f64 * 0.04;
    let ny = ty as f64 * 0.04;
    let v = surface.get([nx, ny]) * 0.6
          + surface.get([nx * 2.0, ny * 2.0]) * 0.3
          + surface.get([nx * 4.0, ny * 4.0]) * 0.1;
    ((v + 1.0) * 0.5) as f32
}

/// Compute discrete surface Z level at world tile (tx, ty). O(1).
pub fn surface_height(tx: i32, ty: i32, gen: &WorldGen) -> i32 {
    let v = surface_v(tx, ty, &gen.surface);
    let z = Z_MIN as f32 + v * CHUNK_HEIGHT as f32;
    (z.round() as i32).clamp(Z_MIN, Z_MAX)
}

fn surface_kind_fn(v: f32, water_t: f32, grass_t: f32, farm_t: f32, forest_t: f32) -> TileKind {
    if v < water_t       { TileKind::Water }
    else if v < grass_t  { TileKind::Grass }
    else if v < farm_t   { TileKind::Farmland }
    else if v < forest_t { TileKind::Forest }
    else                 { TileKind::Stone }
}

/// Deterministically compute the tile at world tile coords (tx, ty, tz).
/// Pure — no side effects, no allocations. Safe to call from any thread.
pub fn proc_tile(
    tx: i32, ty: i32, tz: i32,
    gen: &WorldGen,
    water_t: f32, grass_t: f32, farm_t: f32, forest_t: f32,
) -> TileData {
    let v = surface_v(tx, ty, &gen.surface);
    let surf_z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
    let surf_z = surf_z.clamp(Z_MIN, Z_MAX);

    if tz > surf_z {
        return TileData { kind: TileKind::Air, ..Default::default() };
    }

    if tz == surf_z {
        let kind = surface_kind_fn(v, water_t, grass_t, farm_t, forest_t);
        let fertility = if matches!(kind, TileKind::Farmland | TileKind::Grass) {
            ((1.0 - (v - 0.45).abs() * 5.0).max(0.0) * 255.0) as u8
        } else {
            0
        };
        return TileData { kind, elevation: (v * 255.0) as u8, fertility, flags: 0 };
    }

    // Below surface — check for cave cavities.
    let nx = tx as f64 * 0.08;
    let ny = ty as f64 * 0.08;
    let nz = tz as f64 * 0.12;
    let cave_v = gen.cave.get([nx, ny, nz]);

    if cave_v > 0.55 {
        // Carved cavity. The first Air tile above solid rock is the Dirt floor.
        let below_v = gen.cave.get([nx, ny, (tz as f64 - 1.0) * 0.12]);
        let kind = if below_v <= 0.55 { TileKind::Dirt } else { TileKind::Air };
        return TileData { kind, ..Default::default() };
    }

    TileData { kind: TileKind::Wall, ..Default::default() }
}

/// Surface tile lookup — returns None if the chunk is not loaded.
pub fn surface_tile_at(
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32, ty: i32,
) -> Option<TileData> {
    let surf_z = chunk_map.surface_z_at(tx, ty);
    if surf_z < Z_MIN { return None; }
    Some(tile_at_3d(chunk_map, gen, globe, tx, ty, surf_z))
}

/// Full tile lookup: check delta map first, then fall through to proc_tile.
pub fn tile_at_3d(
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32, ty: i32, tz: i32,
) -> TileData {
    if let Some(d) = chunk_map.tile_delta_at(tx, ty, tz) {
        return d;
    }
    let (gx, gy) = Globe::cell_for_chunk(
        tx.div_euclid(CHUNK_SIZE as i32),
        ty.div_euclid(CHUNK_SIZE as i32),
    );
    let biome = globe.cell(gx, gy).map(|c| c.biome).unwrap_or_default();
    let (water_t, grass_t, farm_t, forest_t) = biome_thresholds(biome);
    proc_tile(tx, ty, tz, gen, water_t, grass_t, farm_t, forest_t)
}

/// Build a new Chunk: empty delta map + surface_z and surface_kind caches pre-filled.
pub fn generate_chunk_from_globe(coord: ChunkCoord, globe_cell: &WorldCell, gen: &WorldGen) -> Chunk {
    let (water_t, grass_t, farm_t, forest_t) = biome_thresholds(globe_cell.biome);

    let mut surface_z         = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_kind      = Box::new([[TileKind::default(); CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);

    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let global_tx = coord.0 * CHUNK_SIZE as i32 + lx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ly as i32;
            let v = surface_v(global_tx, global_ty, &gen.surface);
            let z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
            let z = z.clamp(Z_MIN, Z_MAX);
            let kind = surface_kind_fn(v, water_t, grass_t, farm_t, forest_t);
            let fertility = if matches!(kind, TileKind::Farmland | TileKind::Grass) {
                ((1.0 - (v - 0.45).abs() * 5.0).max(0.0) * 255.0) as u8
            } else { 0 };
            surface_z[ly][lx]         = z as i8;
            surface_kind[ly][lx]      = kind;
            surface_fertility[ly][lx] = fertility;
        }
    }
    Chunk::new(surface_z, surface_kind, surface_fertility)
}

pub fn spawn_world_system(
    gen: Res<WorldGen>,
    mut chunk_map: ResMut<ChunkMap>,
    globe: Res<Globe>,
    sandbox: Option<Res<crate::sandbox::SandboxMode>>,
) {
    let start_cx = (GLOBE_WIDTH  / 2) * GLOBE_CELL_CHUNKS;
    let start_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;

    let (chunks_x, chunks_y) = if sandbox.is_some() { (5, 5) } else { (WORLD_CHUNKS_X, WORLD_CHUNKS_Y) };

    for dy in 0..chunks_y {
        for dx in 0..chunks_x {
            let coord = ChunkCoord(start_cx + dx, start_cy + dy);
            let (gx, gy) = Globe::cell_for_chunk(coord.0, coord.1);
            let cell = globe.cell(gx, gy).copied().unwrap_or_default();
            let chunk = generate_chunk_from_globe(coord, &cell, &gen);
            chunk_map.0.insert(coord, chunk);
        }
    }

    info!(
        "Initial area generated: {}x{} chunks at globe center ({},{})",
        chunks_x, chunks_y, start_cx, start_cy
    );
}

/// Convert tile coordinates to world-space pixel position (center of tile).
pub fn tile_to_world(tile_x: i32, tile_y: i32) -> Vec2 {
    Vec2::new(
        tile_x as f32 * TILE_SIZE + TILE_SIZE * 0.5,
        tile_y as f32 * TILE_SIZE + TILE_SIZE * 0.5,
    )
}

/// Convert world-space position to tile coordinates.
pub fn world_to_tile(pos: Vec2) -> (i32, i32) {
    (
        (pos.x / TILE_SIZE).floor() as i32,
        (pos.y / TILE_SIZE).floor() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_height_in_range() {
        let gen = WorldGen::new();
        let z = surface_height(0, 0, &gen);
        assert!((Z_MIN..=Z_MAX).contains(&z));
    }

    #[test]
    fn proc_tile_above_surface_is_air() {
        let gen = WorldGen::new();
        let surf = surface_height(5, 5, &gen);
        let t = proc_tile(5, 5, surf + 1, &gen, 0.22, 0.45, 0.60, 0.75);
        assert_eq!(t.kind, TileKind::Air);
    }

    #[test]
    fn proc_tile_surface_not_wall_or_air() {
        let gen = WorldGen::new();
        let surf = surface_height(5, 5, &gen);
        let t = proc_tile(5, 5, surf, &gen, 0.22, 0.45, 0.60, 0.75);
        assert!(!matches!(t.kind, TileKind::Air | TileKind::Wall));
    }

    #[test]
    fn proc_tile_deep_is_wall_or_cave() {
        let gen = WorldGen::new();
        let surf = surface_height(5, 5, &gen);
        let t = proc_tile(5, 5, surf - 5, &gen, 0.22, 0.45, 0.60, 0.75);
        assert!(matches!(t.kind, TileKind::Wall | TileKind::Air | TileKind::Dirt));
    }

    #[test]
    fn proc_tile_deterministic() {
        let gen = WorldGen::new();
        let a = proc_tile(10, 20, 0, &gen, 0.22, 0.45, 0.60, 0.75);
        let b = proc_tile(10, 20, 0, &gen, 0.22, 0.45, 0.60, 0.75);
        assert_eq!(a.kind, b.kind);
    }

    #[test]
    fn tile_world_roundtrip() {
        let pos = tile_to_world(5, 7);
        let (tx, ty) = world_to_tile(pos);
        assert_eq!((tx, ty), (5, 7));
    }
}
