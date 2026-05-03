use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};
use std::time::Instant;

use super::biome as biome_mod;
use super::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_HEIGHT, CHUNK_SIZE, Z_MAX, Z_MIN};
use super::globe::{Biome, Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use super::tile::{OreKind, TileData, TileKind};

pub const WORLD_CHUNKS_X: i32 = 32;
pub const WORLD_CHUNKS_Y: i32 = 32;
pub const TILE_SIZE: f32 = 16.0;

const WORLD_SEED: u32 = 42;

/// Perlin instances used for world generation, stored as a Bevy resource.
/// One noise field per ore lets veins of different ores overlap and span
/// independent depth bands (see `ORE_BANDS`).
#[derive(Resource)]
pub struct WorldGen {
    pub surface: Perlin, // 2D surface height + tile kind (seed WORLD_SEED)
    pub cave: Perlin,    // 3D cave cavities (seed WORLD_SEED + 1)
    pub coal: Perlin,    // 3D coal vein noise (WORLD_SEED + 2)
    pub copper: Perlin,  // 3D copper vein noise (WORLD_SEED + 3)
    pub iron: Perlin,    // 3D iron vein noise (WORLD_SEED + 4)
    pub tin: Perlin,     // 3D tin vein noise (WORLD_SEED + 5)
    pub silver: Perlin,  // 3D silver vein noise (WORLD_SEED + 6)
    pub gold: Perlin,    // 3D gold vein noise (WORLD_SEED + 7)
}

impl WorldGen {
    pub fn new() -> Self {
        Self {
            surface: Perlin::default().set_seed(WORLD_SEED),
            cave: Perlin::default().set_seed(WORLD_SEED + 1),
            coal: Perlin::default().set_seed(WORLD_SEED + 2),
            copper: Perlin::default().set_seed(WORLD_SEED + 3),
            iron: Perlin::default().set_seed(WORLD_SEED + 4),
            tin: Perlin::default().set_seed(WORLD_SEED + 5),
            silver: Perlin::default().set_seed(WORLD_SEED + 6),
            gold: Perlin::default().set_seed(WORLD_SEED + 7),
        }
    }

    fn perlin_for_ore(&self, ore: OreKind) -> Option<&Perlin> {
        match ore {
            OreKind::Coal => Some(&self.coal),
            OreKind::Copper => Some(&self.copper),
            OreKind::Iron => Some(&self.iron),
            OreKind::Tin => Some(&self.tin),
            OreKind::Silver => Some(&self.silver),
            OreKind::Gold => Some(&self.gold),
            OreKind::None => None,
        }
    }
}

/// Topsoil layer depth (number of `Dirt` tiles below the surface) by biome.
/// Mountains have thin soil; taiga/tropical/temperate have deep soil.
pub fn topsoil_depth(biome: Biome) -> i32 {
    match biome {
        Biome::Mountain => 1,
        Biome::Desert | Biome::Tundra => 2,
        Biome::Grassland => 3,
        Biome::Taiga | Biome::Tropical | Biome::Temperate => 4,
        Biome::Ocean => 0,
    }
}

/// Ore vein parameters. `top_offset..=bot_offset` is the depth band (in tiles
/// below the surface) where this ore can spawn; outside the band the noise is
/// not even sampled. Higher `threshold` = rarer (Perlin output ≈ [-1, 1]).
struct OreBand {
    kind: OreKind,
    top_offset: i32,
    bot_offset: i32,
    threshold: f64,
    freq_xy: f64,
    freq_z: f64,
}

const ORE_BANDS: &[OreBand] = &[
    OreBand { kind: OreKind::Coal,   top_offset: 1,  bot_offset: 6,  threshold: 0.45, freq_xy: 0.10, freq_z: 0.18 },
    OreBand { kind: OreKind::Copper, top_offset: 2,  bot_offset: 8,  threshold: 0.50, freq_xy: 0.10, freq_z: 0.18 },
    OreBand { kind: OreKind::Tin,    top_offset: 5,  bot_offset: 12, threshold: 0.55, freq_xy: 0.10, freq_z: 0.18 },
    OreBand { kind: OreKind::Iron,   top_offset: 6,  bot_offset: 14, threshold: 0.52, freq_xy: 0.10, freq_z: 0.18 },
    OreBand { kind: OreKind::Silver, top_offset: 10, bot_offset: 18, threshold: 0.60, freq_xy: 0.12, freq_z: 0.20 },
    OreBand { kind: OreKind::Gold,   top_offset: 14, bot_offset: 32, threshold: 0.65, freq_xy: 0.12, freq_z: 0.20 },
];

impl Default for WorldGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Return biome tile thresholds: (water_t, grass_t, farm_t, forest_t).
pub fn biome_thresholds(biome: Biome) -> (f32, f32, f32, f32) {
    match biome {
        Biome::Ocean => (0.90, 0.95, 0.97, 0.99),
        Biome::Tundra => (0.18, 0.80, 0.85, 0.95),
        Biome::Taiga => (0.18, 0.35, 0.40, 0.85),
        Biome::Temperate => (0.26, 0.45, 0.60, 0.85),
        Biome::Grassland => (0.18, 0.60, 0.75, 0.88),
        Biome::Tropical => (0.25, 0.30, 0.35, 0.88),
        Biome::Desert => (0.10, 0.65, 0.68, 0.75),
        Biome::Mountain => (0.12, 0.25, 0.28, 0.50),
    }
}

/// Fractional surface noise value at (tx, ty). Range [0, 1].
///
/// 4-octave FBM with a continental macro layer; the result is reshaped via a
/// signed power curve so peaks and basins push toward the Z extremes instead
/// of clustering near 0.5. Lower base frequency than the original (0.02 vs
/// 0.04) doubles feature wavelength.
fn surface_v(tx: i32, ty: i32, surface: &Perlin) -> f32 {
    let nx = tx as f64 * 0.02;
    let ny = ty as f64 * 0.02;
    let macro_v = surface.get([tx as f64 * 0.005, ty as f64 * 0.005]);
    let v = macro_v * 0.35
        + surface.get([nx, ny]) * 0.40
        + surface.get([nx * 2.0, ny * 2.0]) * 0.18
        + surface.get([nx * 4.0, ny * 4.0]) * 0.07;
    let n = (((v + 1.0) * 0.5) as f32).clamp(0.0, 1.0);
    let centered = (n - 0.5) * 2.0;
    let shaped = centered.signum() * centered.abs().powf(0.65);
    (shaped * 0.5 + 0.5).clamp(0.0, 1.0)
}

/// Compute discrete surface Z level at world tile (tx, ty). O(1).
pub fn surface_height(tx: i32, ty: i32, gen: &WorldGen) -> i32 {
    let v = surface_v(tx, ty, &gen.surface);
    let z = Z_MIN as f32 + v * CHUNK_HEIGHT as f32;
    (z.round() as i32).clamp(Z_MIN, Z_MAX)
}

fn surface_kind_fn(v: f32, water_t: f32, grass_t: f32, farm_t: f32, forest_t: f32) -> TileKind {
    if v < water_t {
        TileKind::Water
    } else if v < grass_t {
        TileKind::Grass
    } else if v < farm_t {
        TileKind::Farmland
    } else if v < forest_t {
        TileKind::Forest
    } else {
        TileKind::Stone
    }
}

/// Deterministically compute the tile at world tile coords (tx, ty, tz).
/// Pure — no side effects, no allocations. Safe to call from any thread.
///
/// `biome` controls topsoil depth (Dirt layer thickness) for subsurface tiles.
pub fn proc_tile(
    tx: i32,
    ty: i32,
    tz: i32,
    gen: &WorldGen,
    biome: Biome,
    water_t: f32,
    grass_t: f32,
    farm_t: f32,
    forest_t: f32,
) -> TileData {
    let v = surface_v(tx, ty, &gen.surface);
    let surf_z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
    let surf_z = surf_z.clamp(Z_MIN, Z_MAX);

    if tz > surf_z {
        return TileData {
            kind: TileKind::Air,
            ..Default::default()
        };
    }

    if tz == surf_z {
        let kind = surface_kind_fn(v, water_t, grass_t, farm_t, forest_t);
        let fertility = if matches!(kind, TileKind::Farmland | TileKind::Grass) {
            ((1.0 - (v - 0.45).abs() * 5.0).max(0.0) * 255.0) as u8
        } else {
            0
        };
        return TileData {
            kind,
            elevation: (v * 255.0) as u8,
            fertility,
            flags: 0,
            ore: 0,
        };
    }

    // Below surface — cave cavities take precedence over geology so that any
    // ore intersected by a cave is "lost" (becomes Air or a Dirt cave floor).
    let nx = tx as f64 * 0.08;
    let ny = ty as f64 * 0.08;
    let nz = tz as f64 * 0.12;
    let cave_v = gen.cave.get([nx, ny, nz]);

    if cave_v > 0.55 {
        // Carved cavity. The first Air tile above solid rock is the Dirt floor.
        let below_v = gen.cave.get([nx, ny, (tz as f64 - 1.0) * 0.12]);
        let kind = if below_v <= 0.55 {
            TileKind::Dirt
        } else {
            TileKind::Air
        };
        return TileData {
            kind,
            ..Default::default()
        };
    }

    // Topsoil layer: a few Dirt tiles directly below the surface, biome-thick.
    let depth = surf_z - tz; // > 0 below surface
    if depth <= topsoil_depth(biome) {
        return TileData {
            kind: TileKind::Dirt,
            ..Default::default()
        };
    }

    // Bedrock band: try ore veins in declared order. Skip the noise sample
    // entirely when the depth is outside an ore's band — keeps deep tiles fast.
    for band in ORE_BANDS {
        if depth < band.top_offset || depth > band.bot_offset {
            continue;
        }
        let perlin = match gen.perlin_for_ore(band.kind) {
            Some(p) => p,
            None => continue,
        };
        let v = perlin.get([
            tx as f64 * band.freq_xy,
            ty as f64 * band.freq_xy,
            tz as f64 * band.freq_z,
        ]);
        if v > band.threshold {
            return TileData {
                kind: TileKind::Ore,
                ore: band.kind.as_u8(),
                ..Default::default()
            };
        }
    }

    TileData {
        kind: TileKind::Wall,
        ..Default::default()
    }
}

/// Surface tile lookup — returns None if the chunk is not loaded.
pub fn surface_tile_at(
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32,
    ty: i32,
) -> Option<TileData> {
    let surf_z = chunk_map.surface_z_at(tx, ty);
    if surf_z < Z_MIN {
        return None;
    }
    Some(tile_at_3d(chunk_map, gen, globe, tx, ty, surf_z))
}

/// Full tile lookup: check delta map first, then fall through to proc_tile.
pub fn tile_at_3d(
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32,
    ty: i32,
    tz: i32,
) -> TileData {
    if let Some(d) = chunk_map.tile_delta_at(tx, ty, tz) {
        return d;
    }
    let biome = biome_mod::classify_at_tile(globe, tx, ty);
    let (water_t, grass_t, farm_t, forest_t) = biome_thresholds(biome);
    proc_tile(tx, ty, tz, gen, biome, water_t, grass_t, farm_t, forest_t)
}

/// Build a new Chunk: empty delta map + surface_z and surface_kind caches
/// pre-filled. Per-tile biome via `biome::classify_at_tile` (continuous
/// across mega-chunk seams). Rivers and lakes from the globe-level network
/// are stamped over the noise-derived surface.
pub fn generate_chunk_from_globe(coord: ChunkCoord, globe: &Globe, gen: &WorldGen) -> Chunk {
    let mut surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_kind = Box::new([[TileKind::default(); CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);

    let chunk_tx0 = coord.0 * CHUNK_SIZE as i32;
    let chunk_ty0 = coord.1 * CHUNK_SIZE as i32;

    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let global_tx = chunk_tx0 + lx as i32;
            let global_ty = chunk_ty0 + ly as i32;
            let biome = biome_mod::classify_at_tile(globe, global_tx, global_ty);
            let (water_t, grass_t, farm_t, forest_t) = biome_thresholds(biome);
            let v = surface_v(global_tx, global_ty, &gen.surface);
            let z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
            let z = z.clamp(Z_MIN, Z_MAX);
            let kind = surface_kind_fn(v, water_t, grass_t, farm_t, forest_t);
            let fertility = if matches!(kind, TileKind::Farmland | TileKind::Grass) {
                ((1.0 - (v - 0.45).abs() * 5.0).max(0.0) * 255.0) as u8
            } else {
                0
            };
            surface_z[ly][lx] = z as i8;
            surface_kind[ly][lx] = kind;
            surface_fertility[ly][lx] = fertility;
        }
    }

    // Stamp rivers (Bresenham along each edge that touches this chunk).
    let chunk_tx1 = chunk_tx0 + CHUNK_SIZE as i32;
    let chunk_ty1 = chunk_ty0 + CHUNK_SIZE as i32;
    let tiles_per_climate_cell = (GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;
    let cell_to_tile = |gx: u32, gy: u32| {
        let tx = (gx as f32 + 0.5) * tiles_per_climate_cell;
        let ty = (gy as f32 + 0.5) * tiles_per_climate_cell;
        (tx as i32, ty as i32)
    };
    for edge in &globe.rivers.edges {
        let (ax, ay) = cell_to_tile(edge.from.0, edge.from.1);
        let (bx, by) = cell_to_tile(edge.to.0, edge.to.1);
        // Quick AABB reject — the edge's bbox vs this chunk's bbox.
        let lo_x = ax.min(bx);
        let hi_x = ax.max(bx);
        let lo_y = ay.min(by);
        let hi_y = ay.max(by);
        let half_w = edge.width as i32;
        if hi_x + half_w < chunk_tx0
            || lo_x - half_w >= chunk_tx1
            || hi_y + half_w < chunk_ty0
            || lo_y - half_w >= chunk_ty1
        {
            continue;
        }
        bresenham_stamp(
            ax,
            ay,
            bx,
            by,
            edge.width as i32,
            chunk_tx0,
            chunk_ty0,
            &mut surface_kind,
            &mut surface_z,
        );
    }

    // Stamp lakes (disc fill).
    for lake in &globe.lakes.lakes {
        let (cx, cy) = lake.center_tile;
        let r = lake.radius_tiles as i32;
        if cx + r < chunk_tx0 || cx - r >= chunk_tx1 || cy + r < chunk_ty0 || cy - r >= chunk_ty1 {
            continue;
        }
        let r2 = r * r;
        for ly in 0..CHUNK_SIZE {
            for lx in 0..CHUNK_SIZE {
                let tx = chunk_tx0 + lx as i32;
                let ty = chunk_ty0 + ly as i32;
                let dx = tx - cx;
                let dy = ty - cy;
                if dx * dx + dy * dy <= r2 {
                    surface_kind[ly][lx] = TileKind::Water;
                    surface_z[ly][lx] = lake.level_z;
                    surface_fertility[ly][lx] = 0;
                }
            }
        }
    }

    Chunk::new(surface_z, surface_kind, surface_fertility)
}

/// Bresenham line from (ax,ay) to (bx,by), widened by `half_w` tiles each
/// side, stamping `Water` into the chunk-local arrays for any covered tile
/// inside [chunk_tx0, chunk_tx0+CHUNK_SIZE) × [chunk_ty0, chunk_ty0+CHUNK_SIZE).
fn bresenham_stamp(
    ax: i32,
    ay: i32,
    bx: i32,
    by: i32,
    half_w: i32,
    chunk_tx0: i32,
    chunk_ty0: i32,
    surface_kind: &mut [[TileKind; CHUNK_SIZE]; CHUNK_SIZE],
    surface_z: &mut [[i8; CHUNK_SIZE]; CHUNK_SIZE],
) {
    let dx = (bx - ax).abs();
    let sx = if ax < bx { 1 } else { -1 };
    let dy = -(by - ay).abs();
    let sy = if ay < by { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = ax;
    let mut y = ay;
    loop {
        // Stamp a half_w×half_w square centered on (x, y).
        for oy in -half_w..=half_w {
            for ox in -half_w..=half_w {
                let lx = x + ox - chunk_tx0;
                let ly = y + oy - chunk_ty0;
                if lx >= 0 && lx < CHUNK_SIZE as i32 && ly >= 0 && ly < CHUNK_SIZE as i32 {
                    surface_kind[ly as usize][lx as usize] = TileKind::Water;
                    // Lower river by 1 below current surface for a gentle channel.
                    let cur = surface_z[ly as usize][lx as usize];
                    surface_z[ly as usize][lx as usize] = (cur as i32 - 1).max(Z_MIN) as i8;
                }
            }
        }
        if x == bx && y == by {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

pub fn spawn_world_system(
    gen: Res<WorldGen>,
    mut chunk_map: ResMut<ChunkMap>,
    globe: Res<Globe>,
    sandbox: Option<Res<crate::sandbox::SandboxMode>>,
    pending: Res<crate::PendingSpawn>,
) {
    let now = Instant::now();
    use crate::world::globe::MEGACHUNK_SIZE_CHUNKS;

    let (chunks_x, chunks_y) = if sandbox.is_some() {
        (5, 5)
    } else {
        (WORLD_CHUNKS_X, WORLD_CHUNKS_Y)
    };

    // Centre the initial chunk pre-gen on the player-picked mega-chunk
    // (PendingSpawn) — fall back to globe centre when nothing was picked
    // (sandbox / no-spawn-select path).
    let (center_cx, center_cy) = match pending.0 {
        Some((mx, my)) => (
            mx * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2,
            my * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2,
        ),
        None => (
            (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS,
            (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS,
        ),
    };

    let start_cx = center_cx - (chunks_x / 2);
    let start_cy = center_cy - (chunks_y / 2);

    for dy in 0..chunks_y {
        for dx in 0..chunks_x {
            let coord = ChunkCoord(start_cx + dx, start_cy + dy);
            let chunk = generate_chunk_from_globe(coord, &globe, &gen);
            chunk_map.0.insert(coord, chunk);
        }
    }

    info!(
        "Initial area generated: {}x{} chunks centered at chunk ({},{}) in {:?}",
        chunks_x,
        chunks_y,
        center_cx,
        center_cy,
        now.elapsed()
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
        let t = proc_tile(5, 5, surf + 1, &gen, Biome::Temperate, 0.22, 0.45, 0.60, 0.75);
        assert_eq!(t.kind, TileKind::Air);
    }

    #[test]
    fn proc_tile_surface_not_wall_or_air() {
        let gen = WorldGen::new();
        let surf = surface_height(5, 5, &gen);
        let t = proc_tile(5, 5, surf, &gen, Biome::Temperate, 0.22, 0.45, 0.60, 0.75);
        assert!(!matches!(t.kind, TileKind::Air | TileKind::Wall));
    }

    #[test]
    fn proc_tile_deep_is_wall_or_cave_or_ore() {
        let gen = WorldGen::new();
        let surf = surface_height(5, 5, &gen);
        let t = proc_tile(5, 5, surf - 10, &gen, Biome::Temperate, 0.22, 0.45, 0.60, 0.75);
        assert!(matches!(
            t.kind,
            TileKind::Wall | TileKind::Air | TileKind::Dirt | TileKind::Ore
        ));
    }

    #[test]
    fn proc_tile_topsoil_is_dirt() {
        let gen = WorldGen::new();
        let surf = surface_height(5, 5, &gen);
        // One tile below the surface should be Dirt (topsoil) for any non-Mountain biome,
        // unless cave noise carves through. Check Temperate which has 4 Dirt tiles.
        let t = proc_tile(5, 5, surf - 1, &gen, Biome::Temperate, 0.22, 0.45, 0.60, 0.75);
        assert!(matches!(t.kind, TileKind::Dirt | TileKind::Air));
    }

    #[test]
    fn proc_tile_deterministic() {
        let gen = WorldGen::new();
        let a = proc_tile(10, 20, 0, &gen, Biome::Temperate, 0.22, 0.45, 0.60, 0.75);
        let b = proc_tile(10, 20, 0, &gen, Biome::Temperate, 0.22, 0.45, 0.60, 0.75);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.ore, b.ore);
    }

    #[test]
    fn tile_world_roundtrip() {
        let pos = tile_to_world(5, 7);
        let (tx, ty) = world_to_tile(pos);
        assert_eq!((tx, ty), (5, 7));
    }
}
