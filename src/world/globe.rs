use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};
use serde::{Deserialize, Serialize};

const SAVE_PATH: &str = "world.bin";

pub const GLOBE_WIDTH: i32 = 64;
pub const GLOBE_HEIGHT: i32 = 32;
pub const GLOBE_CELL_CHUNKS: i32 = 16; // one globe cell = 16×16 local chunks = 512×512 tiles

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
    // world-level simulation state for off-screen cells
    pub faction_id: u32, // 0 = unclaimed
    pub population: u16,
    pub food_stock: f32,
}

#[derive(Resource, Serialize, Deserialize)]
pub struct Globe {
    pub cells: Vec<WorldCell>, // GLOBE_WIDTH × GLOBE_HEIGHT, row-major (y-major)
    pub seed: u64,
}

impl Globe {
    pub fn new(seed: u64) -> Self {
        Self {
            cells: vec![WorldCell::default(); (GLOBE_WIDTH * GLOBE_HEIGHT) as usize],
            seed,
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
}

fn whittaker_biome(elevation_f: f32, temp_f: f32, rainfall_f: f32) -> Biome {
    if elevation_f > 0.82 {
        return Biome::Mountain;
    }
    if elevation_f < 0.22 {
        return Biome::Ocean;
    }
    // temp_f: 0=cold, 1=hot; rainfall_f: 0=dry, 1=wet
    match (temp_f > 0.55, rainfall_f > 0.55, temp_f > 0.3) {
        _ if temp_f < 0.2 => Biome::Tundra,
        _ if temp_f < 0.35 && rainfall_f > 0.45 => Biome::Taiga,
        (true, true, _) => Biome::Tropical,
        (true, false, _) => Biome::Desert,
        (false, true, true) => Biome::Temperate,
        _ => Biome::Grassland,
    }
}

pub fn generate_globe(seed: u64) -> Globe {
    let mut globe = Globe::new(seed);

    let elev_noise = Perlin::default().set_seed(seed as u32);
    let rain_noise = Perlin::default().set_seed(seed as u32 ^ 0xDEAD_BEEF);

    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let nx = gx as f64 * 0.06;
            let ny = gy as f64 * 0.06;

            // Elevation: layered noise
            let ev = elev_noise.get([nx, ny]) * 0.60
                + elev_noise.get([nx * 2.0, ny * 2.0]) * 0.30
                + elev_noise.get([nx * 4.0, ny * 4.0]) * 0.10;
            let elev_f = ((ev + 1.0) * 0.5) as f32;

            // Temperature: warm equator, cold poles, cool mountains
            let lat_f = (gy as f32 - GLOBE_HEIGHT as f32 * 0.5).abs() / (GLOBE_HEIGHT as f32 * 0.5);
            let temp_f = (1.0 - lat_f * 0.55 - elev_f * 0.45).clamp(0.0, 1.0);
            let temp_c = (temp_f * 80.0 - 30.0) as i8; // -30 to +50°C

            // Rainfall
            let rv = rain_noise.get([nx + 5.0, ny + 5.0]) * 0.70
                + rain_noise.get([nx * 3.0, ny * 3.0]) * 0.30;
            let rain_f = ((rv + 1.0) * 0.5) as f32;
            // Deserts form in dry high-temp regions; rainfall modulated by temp
            let rain_adj = (rain_f * (0.4 + temp_f * 0.6)).clamp(0.0, 1.0);
            let rainfall = (rain_adj * 255.0) as u8;

            let biome = whittaker_biome(elev_f, temp_f, rain_adj);

            // Resource flags from biome
            let resources = {
                let mut r = 0u8;
                if matches!(biome, Biome::Temperate | Biome::Taiga | Biome::Tropical) {
                    r |= 0x01;
                } // forest
                if elev_f > 0.65 {
                    r |= 0x02;
                } // stone
                if elev_f > 0.70 && biome == Biome::Mountain {
                    r |= 0x04;
                } // ore
                r
            };

            // Seed initial populations on habitable non-ocean cells (deterministic)
            let (population, faction_id, food_stock) = if biome.is_habitable() {
                let hash = (gx as u64)
                    .wrapping_mul(73_856_093)
                    .wrapping_add((gy as u64).wrapping_mul(19_349_663))
                    ^ seed;
                if hash % 8 == 0 {
                    let pop = ((hash >> 8) % 15 + 5) as u16; // 5..20
                    (pop, 0u32, biome.yield_rate() * 50.0)
                } else {
                    (0, 0, 0.0)
                }
            } else {
                (0, 0, 0.0)
            };

            if let Some(cell) = globe.cell_mut(gx, gy) {
                *cell = WorldCell {
                    biome,
                    elevation: (elev_f * 255.0) as u8,
                    temperature: temp_c,
                    rainfall,
                    resources,
                    explored: false,
                    faction_id,
                    population,
                    food_stock,
                };
            }
        }
    }

    info!(
        "Globe generated: {}×{} = {} cells",
        GLOBE_WIDTH,
        GLOBE_HEIGHT,
        GLOBE_WIDTH * GLOBE_HEIGHT
    );

    globe
}

/// Load globe from disk if available, otherwise generate and save it.
pub fn load_or_generate(seed: u64) -> Globe {
    if let Ok(bytes) = std::fs::read(SAVE_PATH) {
        if let Ok(globe) = bincode::deserialize::<Globe>(&bytes) {
            info!("Globe loaded from {SAVE_PATH}");
            return globe;
        }
        warn!("Failed to deserialize {SAVE_PATH} — regenerating");
    }

    let globe = generate_globe(seed);

    match bincode::serialize(&globe) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(SAVE_PATH, &bytes) {
                warn!("Could not save globe to {SAVE_PATH}: {e}");
            } else {
                info!("Globe saved to {SAVE_PATH}");
            }
        }
        Err(e) => warn!("Could not serialize globe: {e}"),
    }

    globe
}
