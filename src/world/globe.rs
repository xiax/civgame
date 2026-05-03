use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};
use serde::{Deserialize, Serialize};

const SAVE_PATH: &str = "world.bin";

/// On-disk schema version for `world.bin`. Bump whenever `Globe`, `WorldCell`,
/// or any serialized geo-data layout changes — `load_or_generate` will discard
/// older caches and regenerate.
pub const GLOBE_FILE_VERSION: u32 = 2;

/// Climate-sample grid resolution. Each cell holds elevation/climate/biome
/// samples; per-tile values are bilinearly interpolated. Resolution is
/// independent of mega-chunk size — biomes flow continuously across mega-chunk
/// seams because the underlying climate field is continuous.
pub const GLOBE_WIDTH: i32 = 256;
pub const GLOBE_HEIGHT: i32 = 128;

/// Chunks per climate (globe) cell. Each cell covers GLOBE_CELL_CHUNKS² chunks.
pub const GLOBE_CELL_CHUNKS: i32 = 4;

/// Chunks per mega-chunk. Mega-chunks are the player's settlement / world-map
/// switching unit. Independent of GLOBE_CELL_CHUNKS so a single mega-chunk can
/// span multiple climate cells (mixed biomes within one settlement region).
pub const MEGACHUNK_SIZE_CHUNKS: i32 = 16;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Biome {
    Ocean = 0,
    Tundra = 1,
    Taiga = 2,
    #[default]
    Temperate = 3,
    Grassland = 4,
    Tropical = 5,
    Desert = 6,
    Mountain = 7,
}

impl Biome {
    pub fn name(self) -> &'static str {
        match self {
            Biome::Ocean => "Ocean",
            Biome::Tundra => "Tundra",
            Biome::Taiga => "Taiga",
            Biome::Temperate => "Temperate",
            Biome::Grassland => "Grassland",
            Biome::Tropical => "Tropical",
            Biome::Desert => "Desert",
            Biome::Mountain => "Mountain",
        }
    }

    /// Base RGBA color for world map rendering.
    pub fn color(self) -> [u8; 4] {
        match self {
            Biome::Ocean => [30, 80, 160, 255],
            Biome::Tundra => [220, 230, 240, 255],
            Biome::Taiga => [60, 100, 60, 255],
            Biome::Temperate => [80, 150, 60, 255],
            Biome::Grassland => [150, 190, 80, 255],
            Biome::Tropical => [30, 160, 60, 255],
            Biome::Desert => [210, 180, 100, 255],
            Biome::Mountain => [140, 130, 120, 255],
        }
    }

    /// Approximate food yield per tick for world-level sim.
    pub fn yield_rate(self) -> f32 {
        match self {
            Biome::Ocean => 0.0,
            Biome::Tundra => 0.1,
            Biome::Taiga => 0.3,
            Biome::Temperate => 0.7,
            Biome::Grassland => 0.6,
            Biome::Tropical => 0.8,
            Biome::Desert => 0.05,
            Biome::Mountain => 0.1,
        }
    }

    pub fn is_habitable(self) -> bool {
        !matches!(self, Biome::Ocean | Biome::Mountain)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorldCell {
    pub biome: Biome,
    pub elevation: u8,   // 0–255, normalised
    pub temperature: i8, // -50 to 50 (°C approx)
    pub rainfall: u8,    // 0–255
    pub resources: u8,   // bitflags: 0x01=forest, 0x02=stone, 0x04=ore
    pub explored: bool,
    /// Tectonic plate id (0..=NUM_PLATES-1).
    pub plate_id: u8,
    /// Cell carries a river segment (placed by `hydrology`).
    pub is_river: bool,
    /// Cell sits inside a lake basin (placed by `hydrology`).
    pub is_lake: bool,
    /// Quantised D8 flow accumulation (in upstream cells / 16, saturating).
    pub flow_accum: u16,
    // world-level simulation state for off-screen cells (legacy; will move to
    // SettledRegions in Phase B)
    pub faction_id: u32, // 0 = unclaimed
    pub population: u16,
    pub food_stock: f32,
}

/// Polyline edge in the river network (climate-cell coords). Width is in
/// world tiles (river is rasterised that many tiles wide).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RiverEdge {
    pub from: (u32, u32),
    pub to: (u32, u32),
    pub width: u8,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct RiverNetwork {
    pub edges: Vec<RiverEdge>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LakeBasin {
    /// Tile-coord centre of the lake.
    pub center_tile: (i32, i32),
    pub radius_tiles: u16,
    pub level_z: i8,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct LakeMap {
    pub lakes: Vec<LakeBasin>,
}

#[derive(Resource, Serialize, Deserialize)]
pub struct Globe {
    pub cells: Vec<WorldCell>, // GLOBE_WIDTH × GLOBE_HEIGHT, row-major (y-major)
    pub seed: u64,
    pub rivers: RiverNetwork,
    pub lakes: LakeMap,
}

impl Globe {
    pub fn new(seed: u64) -> Self {
        Self {
            cells: vec![WorldCell::default(); (GLOBE_WIDTH * GLOBE_HEIGHT) as usize],
            seed,
            rivers: RiverNetwork::default(),
            lakes: LakeMap::default(),
        }
    }

    fn idx(gx: i32, gy: i32) -> Option<usize> {
        if gx < 0 || gy < 0 || gx >= GLOBE_WIDTH || gy >= GLOBE_HEIGHT {
            return None;
        }
        Some((gy * GLOBE_WIDTH + gx) as usize)
    }

    pub fn cell(&self, gx: i32, gy: i32) -> Option<&WorldCell> {
        Self::idx(gx, gy).map(|i| &self.cells[i])
    }

    pub fn cell_mut(&mut self, gx: i32, gy: i32) -> Option<&mut WorldCell> {
        Self::idx(gx, gy).map(|i| &mut self.cells[i])
    }

    /// Which globe cell does a local chunk coordinate belong to?
    pub fn cell_for_chunk(cx: i32, cy: i32) -> (i32, i32) {
        (
            cx.div_euclid(GLOBE_CELL_CHUNKS),
            cy.div_euclid(GLOBE_CELL_CHUNKS),
        )
    }

    /// Chunk coordinate range for a globe cell.
    pub fn chunk_range(gx: i32, gy: i32) -> (i32, i32, i32, i32) {
        let cx0 = gx * GLOBE_CELL_CHUNKS;
        let cy0 = gy * GLOBE_CELL_CHUNKS;
        (cx0, cy0, cx0 + GLOBE_CELL_CHUNKS, cy0 + GLOBE_CELL_CHUNKS)
    }

    /// Bilinearly interpolate the (elevation, temperature, rainfall) climate
    /// fields at a world tile coordinate. Returns `(elev, temp_c, rainfall)`
    /// as f32s normalised to roughly the same scales as the underlying
    /// `WorldCell` u8/i8 fields. X wraps; Y clamps to poles.
    pub fn sample_climate(&self, tile_x: i32, tile_y: i32) -> (f32, f32, f32) {
        let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::chunk::CHUNK_SIZE as i32) as f32;
        let fx = tile_x as f32 / tiles_per_cell;
        let fy = tile_y as f32 / tiles_per_cell;
        let gx0 = fx.floor() as i32;
        let gy0 = fy.floor() as i32;
        let tx = fx - gx0 as f32;
        let ty = fy - gy0 as f32;

        let sample = |gx: i32, gy: i32| -> (f32, f32, f32) {
            let gx = gx.rem_euclid(GLOBE_WIDTH);
            let gy = gy.clamp(0, GLOBE_HEIGHT - 1);
            let c = &self.cells[(gy * GLOBE_WIDTH + gx) as usize];
            (c.elevation as f32, c.temperature as f32, c.rainfall as f32)
        };

        let (e00, t00, r00) = sample(gx0, gy0);
        let (e10, t10, r10) = sample(gx0 + 1, gy0);
        let (e01, t01, r01) = sample(gx0, gy0 + 1);
        let (e11, t11, r11) = sample(gx0 + 1, gy0 + 1);

        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let ex = lerp(lerp(e00, e10, tx), lerp(e01, e11, tx), ty);
        let tt = lerp(lerp(t00, t10, tx), lerp(t01, t11, tx), ty);
        let rx = lerp(lerp(r00, r10, tx), lerp(r01, r11, tx), ty);
        (ex, tt, rx)
    }
}

pub fn generate_globe(seed: u64) -> Globe {
    use super::{biome, climate, erosion, hydrology, plates};

    let mut globe = Globe::new(seed);
    let w = GLOBE_WIDTH as usize;
    let h = GLOBE_HEIGHT as usize;
    let n = w * h;

    info!(
        "Generating globe: {}×{} cells, {} plates...",
        GLOBE_WIDTH, GLOBE_HEIGHT, plates::NUM_PLATES
    );

    // ── 1. Plate tectonics ────────────────────────────────────────────────
    let plate_field = plates::generate(seed);
    let uplift = plates::uplift_field(&plate_field);

    // ── 2. Heightmap composition: noise + plate uplift ────────────────────
    // Elevation in roughly [-1, +1] where 0 ≈ sea level. We mix multi-octave
    // Perlin noise (continental shape + texture) with plate uplift (mountain
    // ranges). Noise dominates at 70% so existing biome distribution is
    // recognisable; plates add the geographically-coherent ridges/rifts.
    let elev_noise = Perlin::default().set_seed(seed as u32);
    let mut height = vec![0.0f32; n];
    for gy in 0..h {
        for gx in 0..w {
            let nx = gx as f64 * 0.03;
            let ny = gy as f64 * 0.03;
            let macro_e = elev_noise.get([gx as f64 * 0.012, gy as f64 * 0.012]);
            let ev = macro_e * 0.30
                + elev_noise.get([nx, ny]) * 0.42
                + elev_noise.get([nx * 2.0, ny * 2.0]) * 0.20
                + elev_noise.get([nx * 4.0, ny * 4.0]) * 0.08;
            // Map noise from [-1, 1] roughly into [-1, 1] where 0 ≈ shoreline.
            let noise_h = ev as f32 * 0.7;
            let plate_h = uplift[gy * w + gx] * 1.4; // amplify so mountains poke out
            height[gy * w + gx] = noise_h + plate_h;
        }
    }

    // ── 3. Erosion ─────────────────────────────────────────────────────────
    erosion::thermal(&mut height, 0.05, 20);
    erosion::hydraulic(&mut height, 40);

    // ── 4. Hydrology ──────────────────────────────────────────────────────
    // Save pre-fill heights so lakes can be detected by comparing.
    let pre_fill_height = height.clone();
    hydrology::pit_fill(&mut height);
    let dirs = hydrology::flow_dirs(&height);
    let accum = hydrology::flow_accum(&dirs);
    let rivers = hydrology::extract_rivers(&height, &dirs, &accum, 80);

    // Lake detection: cells whose pit-fill raise was > a threshold AND that
    // sit above sea level — these are sub-spillpoint basins.
    let mut lakes = LakeMap::default();
    let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::chunk::CHUNK_SIZE as i32) as i32;
    let mut is_lake_cell = vec![false; n];
    for i in 0..n {
        let raise = height[i] - pre_fill_height[i];
        if raise > 0.02 && height[i] > 0.0 {
            is_lake_cell[i] = true;
        }
    }
    // Cluster contiguous lake cells into discs (one LakeBasin per cluster).
    let mut visited = vec![false; n];
    for start in 0..n {
        if !is_lake_cell[start] || visited[start] {
            continue;
        }
        let mut stack = vec![start];
        let mut cluster = Vec::new();
        while let Some(i) = stack.pop() {
            if visited[i] {
                continue;
            }
            visited[i] = true;
            if !is_lake_cell[i] {
                continue;
            }
            cluster.push(i);
            let gx = i % w;
            let gy = i / w;
            let xm = (gx + w - 1) % w;
            let xp = (gx + 1) % w;
            stack.push(idx_of(xm, gy, w));
            stack.push(idx_of(xp, gy, w));
            if gy > 0 {
                stack.push(idx_of(gx, gy - 1, w));
            }
            if gy + 1 < h {
                stack.push(idx_of(gx, gy + 1, w));
            }
        }
        if cluster.is_empty() {
            continue;
        }
        let mut sx = 0i64;
        let mut sy = 0i64;
        let mut sh = 0.0f32;
        for &i in &cluster {
            sx += (i % w) as i64;
            sy += (i / w) as i64;
            sh += height[i];
        }
        let cn = cluster.len() as i64;
        let cx = (sx / cn) as i32;
        let cy = (sy / cn) as i32;
        let mean_h = sh / cluster.len() as f32;
        let radius_cells = ((cluster.len() as f32 / std::f32::consts::PI).sqrt()).max(1.0);
        let level_z = ((mean_h * 8.0).round() as i32).clamp(-16, 15) as i8;
        lakes.lakes.push(LakeBasin {
            center_tile: (cx * tiles_per_cell, cy * tiles_per_cell),
            radius_tiles: (radius_cells * tiles_per_cell as f32) as u16,
            level_z,
        });
    }

    // ── 5. Climate ────────────────────────────────────────────────────────
    // Normalise elevation to [0, 1] for the temperature/rainfall formulas.
    let mut min_h = f32::INFINITY;
    let mut max_h = f32::NEG_INFINITY;
    for &v in &height {
        min_h = min_h.min(v);
        max_h = max_h.max(v);
    }
    let span = (max_h - min_h).max(1e-6);
    let elev_norm: Vec<f32> = height.iter().map(|&v| ((v - min_h) / span).clamp(0.0, 1.0)).collect();

    let rain_noise = Perlin::default().set_seed(seed as u32 ^ 0xDEAD_BEEF);
    let mut base_rain = vec![0.0f32; n];
    for gy in 0..h {
        for gx in 0..w {
            let nx = gx as f64 * 0.03;
            let ny = gy as f64 * 0.03;
            let rv = rain_noise.get([nx + 5.0, ny + 5.0]) * 0.70
                + rain_noise.get([nx * 3.0, ny * 3.0]) * 0.30;
            base_rain[gy * w + gx] = ((rv + 1.0) * 0.5) as f32;
        }
    }
    let rain_adj = climate::orographic(&base_rain, &elev_norm, 4);

    // ── 6. Per-cell biome + WorldCell write ───────────────────────────────
    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let i = (gy * GLOBE_WIDTH + gx) as usize;
            let elev_f = elev_norm[i];
            let temp_c = climate::temperature_c(gy as usize, elev_f);
            let temp_f = ((temp_c as f32 + 30.0) / 80.0).clamp(0.0, 1.0);
            // Deserts form in dry hot regions: modulate rainfall by temperature.
            let rain_f = (rain_adj[i] * (0.4 + temp_f * 0.6)).clamp(0.0, 1.0);
            let bm = biome::classify(elev_f, temp_f, rain_f);

            let resources = {
                let mut r = 0u8;
                if matches!(bm, Biome::Temperate | Biome::Taiga | Biome::Tropical) {
                    r |= 0x01; // forest
                }
                if elev_f > 0.65 {
                    r |= 0x02; // stone
                }
                if elev_f > 0.70 && bm == Biome::Mountain {
                    r |= 0x04; // ore
                }
                r
            };

            // Legacy world-sim seed (carry through until SettledRegions lands).
            let (population, faction_id, food_stock) = if bm.is_habitable() {
                let hash = (gx as u64)
                    .wrapping_mul(73_856_093)
                    .wrapping_add((gy as u64).wrapping_mul(19_349_663))
                    ^ seed;
                if hash % 8 == 0 {
                    let pop = ((hash >> 8) % 15 + 5) as u16;
                    (pop, 0u32, bm.yield_rate() * 50.0)
                } else {
                    (0, 0, 0.0)
                }
            } else {
                (0, 0, 0.0)
            };

            if let Some(cell) = globe.cell_mut(gx, gy) {
                *cell = WorldCell {
                    biome: bm,
                    elevation: (elev_f * 255.0) as u8,
                    temperature: temp_c,
                    rainfall: (rain_f * 255.0) as u8,
                    resources,
                    explored: false,
                    plate_id: plate_field.at(gx, gy),
                    is_river: false, // set below from rivers
                    is_lake: is_lake_cell[i],
                    flow_accum: hydrology::quantise_accum(accum[i]),
                    faction_id,
                    population,
                    food_stock,
                };
            }
        }
    }

    // Mark river cells (every cell touched by an extracted edge endpoint).
    for edge in &rivers.edges {
        for &(ex, ey) in &[edge.from, edge.to] {
            if let Some(cell) = globe.cell_mut(ex as i32, ey as i32) {
                cell.is_river = true;
            }
        }
    }

    globe.rivers = rivers;
    globe.lakes = lakes;

    info!(
        "Globe generated: {}×{} cells, {} river edges, {} lakes",
        GLOBE_WIDTH,
        GLOBE_HEIGHT,
        globe.rivers.edges.len(),
        globe.lakes.lakes.len()
    );

    globe
}

#[inline]
fn idx_of(gx: usize, gy: usize, w: usize) -> usize {
    gy * w + gx
}

#[derive(Serialize, Deserialize)]
struct GlobeFile {
    version: u32,
    globe: Globe,
}

/// Load globe from disk if available and version-compatible, otherwise
/// generate and save it.
pub fn load_or_generate(seed: u64) -> Globe {
    if let Ok(bytes) = std::fs::read(SAVE_PATH) {
        match bincode::deserialize::<GlobeFile>(&bytes) {
            Ok(file) if file.version == GLOBE_FILE_VERSION => {
                info!("Globe loaded from {SAVE_PATH} (v{})", file.version);
                return file.globe;
            }
            Ok(file) => warn!(
                "Globe cache version mismatch ({} != {GLOBE_FILE_VERSION}) — regenerating",
                file.version
            ),
            Err(_) => warn!("Failed to deserialize {SAVE_PATH} — regenerating"),
        }
    }

    let globe = generate_globe(seed);

    let file = GlobeFile {
        version: GLOBE_FILE_VERSION,
        globe,
    };
    match bincode::serialize(&file) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(SAVE_PATH, &bytes) {
                warn!("Could not save globe to {SAVE_PATH}: {e}");
            } else {
                info!("Globe saved to {SAVE_PATH} (v{GLOBE_FILE_VERSION})");
            }
        }
        Err(e) => warn!("Could not serialize globe: {e}"),
    }

    file.globe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_globe_smoke() {
        let g = generate_globe(123);
        assert_eq!(g.cells.len(), (GLOBE_WIDTH * GLOBE_HEIGHT) as usize);
        // Land + ocean both exist.
        let ocean_count = g.cells.iter().filter(|c| c.biome == Biome::Ocean).count();
        let land_count = g.cells.len() - ocean_count;
        assert!(ocean_count > 0, "no ocean cells");
        assert!(land_count > 0, "no land cells");
        // At least one mountain (plates should have produced ranges).
        let mountains = g.cells.iter().filter(|c| c.biome == Biome::Mountain).count();
        assert!(mountains > 0, "no mountain cells (plate uplift broken)");
        // Some rivers (hydrology should have extracted at least one polyline).
        assert!(!g.rivers.edges.is_empty(), "no rivers extracted");
        // All cells get a plate id.
        let max_pid = g.cells.iter().map(|c| c.plate_id).max().unwrap_or(0);
        assert!(max_pid > 0, "all cells assigned to plate 0 (plate gen broken)");
    }

    #[test]
    fn sample_climate_continuous() {
        // Sampled climate at a tile near a cell boundary should be a smooth
        // interpolation, not a step.
        let g = generate_globe(7);
        let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::super::chunk::CHUNK_SIZE as i32) as i32;
        let edge_tx = tiles_per_cell;
        let (e0, _, _) = g.sample_climate(edge_tx - 1, 100);
        let (e1, _, _) = g.sample_climate(edge_tx, 100);
        let (e2, _, _) = g.sample_climate(edge_tx + 1, 100);
        // Diff between adjacent samples should be small relative to the cell
        // span (no hard step at the cell boundary).
        assert!((e0 - e1).abs() < 30.0, "discontinuity at cell boundary");
        assert!((e1 - e2).abs() < 30.0, "discontinuity at cell boundary");
    }
}
