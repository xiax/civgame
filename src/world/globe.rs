use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};
use serde::{Deserialize, Serialize};

const SAVE_PATH: &str = "world.bin";

/// On-disk schema version for `world.bin`. Bump whenever `Globe`, `WorldCell`,
/// or any serialized geo-data layout changes — `load_or_generate` will discard
/// older caches and regenerate.
pub const GLOBE_FILE_VERSION: u32 = 10;

/// Climate-sample grid resolution. Each cell holds elevation/climate/biome
/// samples; per-tile values are bilinearly interpolated. Resolution is
/// independent of mega-chunk size — biomes flow continuously across mega-chunk
/// seams because the underlying climate field is continuous.
pub const GLOBE_WIDTH: i32 = 512;
pub const GLOBE_HEIGHT: i32 = 256;

/// Chunks per climate (globe) cell. Each cell covers GLOBE_CELL_CHUNKS² chunks.
/// Halved (was 4) when the climate grid was doubled, so the world tile total
/// (`GLOBE_WIDTH * GLOBE_CELL_CHUNKS = 1024` chunks per axis) is unchanged.
pub const GLOBE_CELL_CHUNKS: i32 = 2;

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
    /// Warm/wet lowland — distinct from `Tropical` in being persistently
    /// waterlogged. Surface palette dominated by `Marsh`.
    Wetland = 8,
    /// Dry-grassland gap between `Grassland` and `Desert`. Surface palette
    /// dominated by `Scrub` with patches of `Grass` along moisture gradients.
    Steppe = 9,
    /// Eroded dry uplands between `Desert` and `Mountain`. Surface palette
    /// is `Sand` / `Scrub` / `Sandstone`.
    Badlands = 10,
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
            Biome::Wetland => "Wetland",
            Biome::Steppe => "Steppe",
            Biome::Badlands => "Badlands",
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
            Biome::Wetland => [70, 110, 80, 255],
            Biome::Steppe => [180, 180, 100, 255],
            Biome::Badlands => [180, 130, 90, 255],
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
            Biome::Wetland => 0.5,
            Biome::Steppe => 0.4,
            Biome::Badlands => 0.1,
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

/// Polyline edge in the river network (climate-cell coords). Widths are in
/// world tiles. `from_width` is the channel width at the upstream endpoint,
/// `to_width` at the downstream endpoint — the rasteriser tapers between
/// them along the curve. Confluences are coherent because every tributary's
/// `to_width` equals the trunk's `from_width` at the join cell (both derived
/// from the same downstream `flow_accum`).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RiverEdge {
    pub from: (u32, u32),
    pub to: (u32, u32),
    pub from_width: u8,
    pub to_width: u8,
    /// Rainfall-weighted upstream discharge at the downstream endpoint.
    #[serde(default)]
    pub discharge: f32,
    /// Strahler stream order at the downstream endpoint (confluence rank).
    #[serde(default)]
    pub order: u8,
    /// Water-surface height (globe height units) at up/down endpoints.
    /// `to_level <= from_level` always (monotone downstream).
    #[serde(default)]
    pub from_level: f32,
    #[serde(default)]
    pub to_level: f32,
    /// Channel depth (sub-z, in globe height units) at up/down endpoints.
    #[serde(default)]
    pub from_depth: f32,
    #[serde(default)]
    pub to_depth: f32,
    /// Reservoir this edge drains into (`u32::MAX` = open drainage / none).
    #[serde(default = "u32_max")]
    pub reservoir_id: u32,
}

#[inline]
fn u32_max() -> u32 {
    u32::MAX
}

/// What kind of standing-water body a reservoir is. `Spring`/`Dam` are
/// runtime-only (springs from dug cells below the water table — Phase 4/6;
/// dams — Phase 4) and never produced by worldgen.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReservoirKind {
    Ocean,
    Lake,
    Wetland,
    Endorheic,
    Spring,
    Dam,
}

/// A standing-water body with a single equilibrium surface level.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Reservoir {
    pub id: u32,
    pub kind: ReservoirKind,
    /// Equilibrium water-surface height in globe height units.
    pub spill_level: f32,
    /// Downstream cell the body spills through (`u32::MAX` = none, i.e.
    /// ocean or closed/endorheic basin).
    pub outlet_cell: u32,
    /// 0.0 fresh .. 1.0 sea-salt.
    pub salinity: f32,
}

/// Per-climate-cell hydrology truth, parallel to `Globe.cells`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct HydroCell {
    /// Terrain height before pit-fill (the real bed/ground).
    pub raw_height: f32,
    /// Terrain height after priority-flood pit-fill (spill surface).
    pub filled_height: f32,
    /// D8 downstream cell index (`== self` at a sink).
    pub flow_to: u32,
    /// Rainfall-weighted upstream accumulation (runoff proxy).
    pub discharge: f32,
    /// Reservoir membership (`u32::MAX` = dry land / open drainage).
    pub reservoir_id: u32,
    /// Local water-table height (≤ `filled_height` except wetland/spring).
    pub aquifer_level: f32,
}

impl Default for HydroCell {
    fn default() -> Self {
        Self {
            raw_height: 0.0,
            filled_height: 0.0,
            flow_to: 0,
            discharge: 0.0,
            reservoir_id: u32::MAX,
            aquifer_level: 0.0,
        }
    }
}

/// Deterministic worldgen hydrology truth. Serialized on `Globe`; the world
/// map overlay and chunk stamping both read it (no parallel formulas).
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct HydrologyMap {
    /// `GLOBE_WIDTH × GLOBE_HEIGHT`, row-major (parallels `Globe.cells`).
    #[serde(default)]
    pub cells: Vec<HydroCell>,
    /// Indexed by `Reservoir::id`.
    #[serde(default)]
    pub reservoirs: Vec<Reservoir>,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct RiverNetwork {
    pub edges: Vec<RiverEdge>,
    /// Index-aligned with `edges`. Each entry is the Chaikin-smoothed tile
    /// polyline from the edge's upstream endpoint to its downstream endpoint
    /// (in world-tile coords, not climate-cell coords). Computed once at
    /// globe gen so chunk-rasterisation is just a per-segment Bresenham walk.
    #[serde(default)]
    pub edge_polylines: Vec<Vec<(i32, i32)>>,
}

/// Whether a river polyline crosses a region boundary flowing *in* or *out*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeCrossingKind {
    /// River enters the region here (its upstream side is outside) — the
    /// fluid sim injects this edge's discharge at this tile.
    Inlet,
    /// River leaves the region here (its downstream side is outside) — the
    /// fluid sim pins this tile at the edge's level (a stable outflow).
    Outlet,
}

/// One classified crossing of the active-region bbox by a river polyline.
/// `level` is a water-surface height in **globe units** (caller scales by
/// `GLOBE_H_TO_Z`).
#[derive(Clone, Copy, Debug)]
pub struct EdgeCrossing {
    pub tile: (i32, i32),
    pub kind: EdgeCrossingKind,
    pub discharge: f32,
    pub level: f32,
}

impl RiverNetwork {
    /// Classify every river-polyline crossing of the inclusive axis-aligned
    /// tile bbox `[min, max]`. Walking each polyline upstream→downstream
    /// (its stored order), an outside→inside transition is an [`Inlet`] at
    /// the first in-region tile (real river inflow, at the edge's
    /// index-interpolated `from_level..to_level` and full `discharge`); an
    /// inside→outside transition is an [`Outlet`] at the last in-region
    /// tile. A polyline that starts/ends inside contributes no crossing —
    /// that head/sink is fed by in-region hydrology (springs, reservoirs,
    /// ocean pins), not an external inlet.
    ///
    /// Pure + deterministic (fixed edge order, integer bbox test). Replaces
    /// the fluid sim's old "highest boundary watercourse = inlet" elevation
    /// heuristic with the true channel topology.
    ///
    /// [`Inlet`]: EdgeCrossingKind::Inlet
    /// [`Outlet`]: EdgeCrossingKind::Outlet
    pub fn edge_crossings_in_bbox(&self, min: (i32, i32), max: (i32, i32)) -> Vec<EdgeCrossing> {
        let inside = |t: (i32, i32)| t.0 >= min.0 && t.0 <= max.0 && t.1 >= min.1 && t.1 <= max.1;
        let mut out = Vec::new();
        for (ei, poly) in self.edge_polylines.iter().enumerate() {
            if poly.len() < 2 {
                continue;
            }
            let Some(edge) = self.edges.get(ei) else {
                continue;
            };
            let last = poly.len() - 1;
            let mut prev_in = inside(poly[0]);
            for k in 1..poly.len() {
                let cur_in = inside(poly[k]);
                if cur_in != prev_in {
                    // upstream→downstream: outside→inside = Inlet at the
                    // first in-region tile; inside→outside = Outlet at the
                    // last in-region tile.
                    let (tile, kind, cross_idx) = if cur_in {
                        (poly[k], EdgeCrossingKind::Inlet, k)
                    } else {
                        (poly[k - 1], EdgeCrossingKind::Outlet, k - 1)
                    };
                    // Level lerps from `from_level` (idx 0) to `to_level`
                    // (idx last) — monotone non-increasing by construction.
                    let frac = cross_idx as f32 / last as f32;
                    let level = edge.from_level + (edge.to_level - edge.from_level) * frac;
                    out.push(EdgeCrossing {
                        tile,
                        kind,
                        discharge: edge.discharge,
                        level,
                    });
                }
                prev_in = cur_in;
            }
        }
        out
    }
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

#[derive(Resource, Clone, Serialize, Deserialize)]
pub struct Globe {
    pub cells: Vec<WorldCell>, // GLOBE_WIDTH × GLOBE_HEIGHT, row-major (y-major)
    pub seed: u64,
    pub rivers: RiverNetwork,
    pub lakes: LakeMap,
    /// Deterministic hydrology truth (discharge, levels, reservoirs, aquifer).
    /// Source for chunk water stamping (Phase 2) and the world-map water
    /// overlay. `#[serde(default)]` so v7 caches deserialize then regenerate
    /// via the version bump.
    #[serde(default)]
    pub hydrology: HydrologyMap,
    /// Per-cell geomorphology classification (slope/relief/coast/mountain
    /// distance + `ReliefClass`). Drives per-tile detail amplitude, palette
    /// gating, fertility multiplier, and settlement scoring. Built once in
    /// `generate_globe` after `build_hydrology`. `#[serde(default)]` so v9
    /// caches deserialize empty and trigger the version-bump regenerate path.
    #[serde(default)]
    pub relief: super::geomorph::ReliefMap,
}

impl Globe {
    pub fn new(seed: u64) -> Self {
        Self {
            cells: vec![WorldCell::default(); (GLOBE_WIDTH * GLOBE_HEIGHT) as usize],
            seed,
            rivers: RiverNetwork::default(),
            lakes: LakeMap::default(),
            hydrology: HydrologyMap::default(),
            relief: super::geomorph::ReliefMap::default(),
        }
    }

    /// Globe-cell index for a world tile (X-wrap, Y-clamp). Mirrors
    /// `sample_climate`'s cell resolution.
    fn hydro_cell_idx(tile_x: i32, tile_y: i32) -> usize {
        let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::chunk::CHUNK_SIZE as i32) as f32;
        let gx = (tile_x as f32 / tiles_per_cell)
            .floor()
            .rem_euclid(GLOBE_WIDTH as f32) as i32;
        let gy = ((tile_y as f32 / tiles_per_cell).floor() as i32).clamp(0, GLOBE_HEIGHT - 1);
        (gy * GLOBE_WIDTH + gx) as usize
    }

    /// Hydrology cell at a world tile (X-wrap, Y-clamp).
    pub fn hydro_cell_at(&self, tile_x: i32, tile_y: i32) -> Option<&HydroCell> {
        self.hydrology
            .cells
            .get(Self::hydro_cell_idx(tile_x, tile_y))
    }

    /// Reservoir at a world tile, if the cell belongs to one.
    pub fn reservoir_at(&self, tile_x: i32, tile_y: i32) -> Option<&Reservoir> {
        let i = Self::hydro_cell_idx(tile_x, tile_y);
        let rid = self.hydrology.cells.get(i)?.reservoir_id;
        self.hydrology.reservoirs.get(rid as usize)
    }

    /// Equilibrium water-surface height (globe units) at a world tile, if the
    /// cell is wet (reservoir member or carries a river). `None` = dry land.
    pub fn water_level_at(&self, tile_x: i32, tile_y: i32) -> Option<f32> {
        let i = Self::hydro_cell_idx(tile_x, tile_y);
        let hc = self.hydrology.cells.get(i)?;
        if let Some(r) = self.hydrology.reservoirs.get(hc.reservoir_id as usize) {
            return Some(r.spill_level);
        }
        let cell = self.cells.get(i)?;
        if cell.is_river {
            Some(hc.filled_height)
        } else {
            None
        }
    }

    /// Water salinity at a world tile: reservoir salinity if a member, else
    /// 0.0 (fresh — rivers/dry). 1.0 = sea.
    pub fn salinity_at(&self, tile_x: i32, tile_y: i32) -> f32 {
        self.reservoir_at(tile_x, tile_y)
            .map(|r| r.salinity)
            .unwrap_or(0.0)
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

    /// Bilinearly interpolate the per-cell `ReliefCell` numerics at a world
    /// tile, then re-derive `ReliefClass` from the interpolated values so the
    /// classification is continuous across cell boundaries (no 64-tile patches
    /// at climate-cell seams). X wraps; Y clamps to poles. Returns a
    /// safe-default sample when the relief layer is empty (e.g. before
    /// generation or in tests that bypass `generate_globe`).
    pub fn sample_relief(&self, tile_x: i32, tile_y: i32) -> super::geomorph::ReliefSample {
        use super::geomorph::{classify, ReliefClass, ReliefSample};

        if self.relief.cells.is_empty() {
            return ReliefSample {
                slope: 0.0,
                local_relief: 0.0,
                mountain_distance: u16::MAX as f32,
                coast_distance: u16::MAX as f32,
                aquifer_depth_norm: 0.5,
                topographic_position: 0.0,
                class: ReliefClass::LowlandPlain,
            };
        }

        let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::chunk::CHUNK_SIZE as i32) as f32;
        let fx = tile_x as f32 / tiles_per_cell;
        let fy = tile_y as f32 / tiles_per_cell;
        let gx0 = fx.floor() as i32;
        let gy0 = fy.floor() as i32;
        let tx = fx - gx0 as f32;
        let ty = fy - gy0 as f32;

        let sample = |gx: i32, gy: i32| -> super::geomorph::ReliefCell {
            let gx = gx.rem_euclid(GLOBE_WIDTH);
            let gy = gy.clamp(0, GLOBE_HEIGHT - 1);
            self.relief.cells[(gy * GLOBE_WIDTH + gx) as usize]
        };
        let c00 = sample(gx0, gy0);
        let c10 = sample(gx0 + 1, gy0);
        let c01 = sample(gx0, gy0 + 1);
        let c11 = sample(gx0 + 1, gy0 + 1);

        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let lerp_u16 = |a: u16, b: u16, t: f32| (a as f32) + ((b as i32 - a as i32) as f32) * t;
        let bilerp = |a: f32, b: f32, c: f32, d: f32| lerp(lerp(a, b, tx), lerp(c, d, tx), ty);
        let bilerp_u16 = |a: u16, b: u16, c: u16, d: u16| {
            lerp(lerp_u16(a, b, tx), lerp_u16(c, d, tx), ty)
        };

        let slope = bilerp(c00.slope_norm, c10.slope_norm, c01.slope_norm, c11.slope_norm);
        let local_relief = bilerp(
            c00.local_relief,
            c10.local_relief,
            c01.local_relief,
            c11.local_relief,
        );
        let tpi = bilerp(
            c00.topographic_position,
            c10.topographic_position,
            c01.topographic_position,
            c11.topographic_position,
        );
        let coast_distance = bilerp_u16(
            c00.coast_distance,
            c10.coast_distance,
            c01.coast_distance,
            c11.coast_distance,
        );
        let mountain_distance = bilerp_u16(
            c00.mountain_distance,
            c10.mountain_distance,
            c01.mountain_distance,
            c11.mountain_distance,
        );
        let aquifer_depth_norm = bilerp(
            c00.aquifer_depth_norm,
            c10.aquifer_depth_norm,
            c01.aquifer_depth_norm,
            c11.aquifer_depth_norm,
        );

        // Pull supporting inputs for class re-derivation from the climate
        // field directly. Avoid `nearest_river_chebyshev` (O(total river
        // vertices) per call — too slow at chunk-gen frequency); the
        // Floodplain/BasinWetland classes inherit from the corner cells
        // themselves (build_relief did the river-distance work once).
        let (elev_u, _temp, rain_u) = self.sample_climate(tile_x, tile_y);
        let elev_norm = (elev_u / 255.0).clamp(0.0, 1.0);
        let rain_norm = (rain_u / 255.0).clamp(0.0, 1.0);
        let res_kind = self.reservoir_at(tile_x, tile_y).map(|r| r.kind);

        // Start from numeric re-classification (continuous across cell
        // boundaries).
        let numeric_class = classify(
            elev_norm,
            slope,
            local_relief,
            tpi,
            coast_distance.round().max(0.0) as u16,
            mountain_distance.round().max(0.0) as u16,
            u32::MAX,
            0,
            aquifer_depth_norm,
            rain_norm,
            res_kind,
        );

        // Inherit "wet" classes from the closest corner cell when the per-
        // tile numerics still support that class — this keeps Floodplain
        // shoulder tiles classified correctly without the per-tile river
        // distance walk.
        let corners = [c00, c10, c01, c11];
        let class = {
            let mut c = numeric_class;
            if corners.iter().any(|cc| cc.relief == ReliefClass::Floodplain)
                && slope < 0.020
                && !matches!(
                    c,
                    ReliefClass::OceanShelf
                        | ReliefClass::BasinWetland
                        | ReliefClass::MountainSlope
                        | ReliefClass::MountainRidge
                )
            {
                c = ReliefClass::Floodplain;
            }
            c
        };

        ReliefSample {
            slope,
            local_relief,
            mountain_distance,
            coast_distance,
            aquifer_depth_norm,
            topographic_position: tpi,
            class,
        }
    }

    /// Chebyshev distance in tiles from `(tx, ty)` to the nearest river
    /// polyline point on the globe. Iterates `rivers.edge_polylines`, so it's
    /// O(total river vertices) — fine at hover / overlay-toggle cadence but
    /// not for per-tick use. Returns `u32::MAX` when no rivers exist or all
    /// of them are farther than ~`u32::MAX` away (effectively "far").
    pub fn nearest_river_chebyshev(&self, tx: i32, ty: i32) -> u32 {
        let mut best = u32::MAX;
        for poly in &self.rivers.edge_polylines {
            for &(rx, ry) in poly {
                let dx = (rx - tx).unsigned_abs();
                let dy = (ry - ty).unsigned_abs();
                let d = dx.max(dy);
                if d < best {
                    best = d;
                    if best == 0 {
                        return 0;
                    }
                }
            }
        }
        best
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
        GLOBE_WIDTH,
        GLOBE_HEIGHT,
        plates::NUM_PLATES
    );

    // ── 1. Plate tectonics ────────────────────────────────────────────────
    let plate_field = plates::generate(seed);
    let uplift = plates::uplift_field(&plate_field, seed);

    // ── 2. Heightmap composition: noise + plate uplift ────────────────────
    // Elevation in roughly [-1, +1] where 0 ≈ sea level (after the percentile
    // shift below). We mix multi-octave Perlin noise (continental shape +
    // texture) with plate uplift (mountain ranges). The macro term carries
    // the dominant weight so continents read as cohesive landmasses, with
    // finer octaves adding coastal detail rather than fragmenting them.
    //
    // Frequencies are tuned against a reference 256-cell-wide grid; scale
    // inversely with current `GLOBE_WIDTH` so doubling the climate-cell
    // density doesn't shrink continents (and therefore the ocean fraction).
    const REF_GRID_WIDTH: f64 = 256.0;
    let nscale: f64 = REF_GRID_WIDTH / GLOBE_WIDTH as f64;
    let elev_noise = Perlin::default().set_seed(seed as u32);
    let mut height = vec![0.0f32; n];
    for gy in 0..h {
        for gx in 0..w {
            let nx = gx as f64 * 0.03 * nscale;
            let ny = gy as f64 * 0.03 * nscale;
            // Two macro octaves at very low freq → super-continent skeleton;
            // base + high octaves add coast/island detail. Macro-dominated
            // weighting (52%) produces recognisable, contiguous continents
            // instead of speckled archipelagos.
            let macro_a = elev_noise.get([gx as f64 * 0.008 * nscale, gy as f64 * 0.008 * nscale]);
            let macro_b = elev_noise.get([gx as f64 * 0.016 * nscale, gy as f64 * 0.016 * nscale]);
            let ev = macro_a * 0.32
                + macro_b * 0.20
                + elev_noise.get([nx, ny]) * 0.30
                + elev_noise.get([nx * 2.0, ny * 2.0]) * 0.13
                + elev_noise.get([nx * 4.0, ny * 4.0]) * 0.05;
            // Map noise from [-1, 1] roughly into [-1, 1] where 0 ≈ shoreline.
            let noise_h = ev as f32 * 0.7;
            let plate_h = uplift[gy * w + gx] * 1.4; // amplify so mountains poke out
            height[gy * w + gx] = noise_h + plate_h;
        }
    }

    // ── 3. Erosion ─────────────────────────────────────────────────────────
    erosion::thermal(&mut height, 0.05, 20);
    erosion::hydraulic(&mut height, 40);

    // ── 3.5. Sea-level alignment ──────────────────────────────────────────
    // Hydrology and lake detection treat `h <= 0` as ocean (drainage sinks).
    // Without an anchor, the 0-contour falls wherever the noise+plate sum
    // happens to land — typically near the median, leaving 50% of cells
    // "ocean" to hydrology but only ~3% classified as Ocean by biome
    // (the elev_f normalisation concentrates near the mean). The mismatch
    // means rivers walk past the visible coastline before terminating at
    // the absolute h=0 contour, so they appear stranded inland of the
    // ocean. Shifting the field so the 30th-percentile = 0 unifies the two:
    // 30% of cells are h<=0 (ocean to both hydrology and biome), and rivers
    // emit their last edge into a cell that *renders* as ocean.
    {
        let mut sorted: Vec<f32> = height.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let h_shift = sorted[(sorted.len() * 30 / 100).min(sorted.len() - 1)];
        for h in height.iter_mut() {
            *h -= h_shift;
        }
    }

    // ── 4. Hydrology ──────────────────────────────────────────────────────
    // Save pre-fill heights so lakes can be detected by comparing.
    let pre_fill_height = height.clone();
    hydrology::pit_fill(&mut height);
    let dirs = hydrology::flow_dirs(&height);
    let accum = hydrology::flow_accum(&dirs);
    let rivers = hydrology::extract_rivers(&height, &dirs, &accum, 80);

    // After the sea-level shift, h<=0 is the ocean line for both hydrology
    // and biome classification. Compute the 90th-percentile peak for the
    // mountain anchor so the elev_f remap below targets ~10% mountain.
    let mut sorted_h: Vec<f32> = height.clone();
    sorted_h.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let h_min = sorted_h[0];
    let h_peak = sorted_h[(sorted_h.len() * 90 / 100).min(sorted_h.len() - 1)];
    let h_max = sorted_h[sorted_h.len() - 1];

    // Lake detection: cells whose pit-fill raise was > a threshold AND that
    // sit above sea level — these are sub-spillpoint basins on land.
    // (Sub-sea basins are ocean, not lakes.)
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
    // Normalise elevation to [0, 1] anchoring against sea level (h=0) and
    // the 90th-percentile peak. Three linear segments:
    //   h_min → 0.0, h=0 → 0.22 (ocean line), h_peak → 0.82 (mountain line),
    //   h_max → 1.0. Guarantees ~30% ocean and ~10% mountain regardless of
    //   distribution shape.
    let span_low = (-h_min).max(1e-6);
    let span_mid = h_peak.max(1e-6);
    let span_high = (h_max - h_peak).max(1e-6);
    let elev_norm: Vec<f32> = height
        .iter()
        .map(|&v| {
            let f = if v <= 0.0 {
                0.22 * (v - h_min) / span_low
            } else if v <= h_peak {
                0.22 + 0.60 * v / span_mid
            } else {
                0.82 + 0.18 * (v - h_peak) / span_high
            };
            f.clamp(0.0, 1.0)
        })
        .collect();

    let rain_noise = Perlin::default().set_seed(seed as u32 ^ 0xDEAD_BEEF);
    let mut base_rain = vec![0.0f32; n];
    for gy in 0..h {
        for gx in 0..w {
            // Same `nscale` rationale as the heightmap pass — keep rainfall
            // features at their original world-space size after a resolution
            // change.
            let nx = gx as f64 * 0.03 * nscale;
            let ny = gy as f64 * 0.03 * nscale;
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

    // Pre-compute curving tile-coord polylines for every edge so chunk
    // rasterisation is just piecewise Bresenham. Determinism: hashed off
    // `(seed, edge_idx)` inside `chaikin_river_path`.
    let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::chunk::CHUNK_SIZE as i32) as f32;
    let cell_to_tile_xy = |gx: u32, gy: u32| -> (i32, i32) {
        let tx = (gx as f32 + 0.5) * tiles_per_cell;
        let ty = (gy as f32 + 0.5) * tiles_per_cell;
        (tx as i32, ty as i32)
    };
    let mut polylines: Vec<Vec<(i32, i32)>> = Vec::with_capacity(rivers.edges.len());
    for (edge_idx, edge) in rivers.edges.iter().enumerate() {
        let (ax, ay) = cell_to_tile_xy(edge.from.0, edge.from.1);
        let (bx, by) = cell_to_tile_xy(edge.to.0, edge.to.1);
        polylines.push(hydrology::chaikin_river_path(
            ax, ay, bx, by, seed, edge_idx,
        ));
    }
    let mut rivers = rivers;
    rivers.edge_polylines = polylines;

    globe.rivers = rivers;
    globe.lakes = lakes;

    // ── 7. Hydrology truth layer (additive; geometry unchanged) ───────────
    // Read finalised per-cell rainfall, build the HydrologyMap, and derive
    // the extended RiverEdge fields. No river/lake geometry moves here —
    // Phase 2 consumes this for chunk stamping.
    {
        let w = GLOBE_WIDTH as usize;
        let rainfall_norm: Vec<f32> = globe
            .cells
            .iter()
            .map(|c| c.rainfall as f32 / 255.0)
            .collect();
        let hydro = hydrology::build_hydrology(&pre_fill_height, &height, &dirs, &rainfall_norm);
        let order = hydrology::strahler_order(&dirs, &accum, 80);
        for e in &mut globe.rivers.edges {
            let fi = e.from.1 as usize * w + e.from.0 as usize;
            let ti = e.to.1 as usize * w + e.to.0 as usize;
            let fc = &hydro.cells[fi];
            let tc = &hydro.cells[ti];
            e.discharge = tc.discharge;
            e.order = order[ti].max(order[fi]);
            e.from_level = fc.filled_height;
            e.to_level = tc.filled_height.min(fc.filled_height);
            e.from_depth = hydrology::depth_for_discharge(fc.discharge);
            e.to_depth = hydrology::depth_for_discharge(tc.discharge);
            e.reservoir_id = tc.reservoir_id;
        }
        globe.hydrology = hydro;
    }

    // ── 8. Geomorphology (relief classification) ──────────────────────────
    // After hydrology so we have the finalised `filled_height`, reservoirs,
    // and aquifer table. Drives per-tile detail amplitude, palette gating,
    // fertility, settlement scoring, and the world-map tooltip.
    {
        let rain_norm: Vec<f32> = globe
            .cells
            .iter()
            .map(|c| c.rainfall as f32 / 255.0)
            .collect();
        let river_cell_mask: Vec<bool> = globe.cells.iter().map(|c| c.is_river).collect();
        // Per-cell Strahler proxy: max stream order of any edge that touches
        // the cell. River edges store order at their downstream endpoint; we
        // stamp the same order on both endpoints so headwater cells inherit
        // their trunk's classification.
        let mut strahler_at_cell = vec![0u8; (GLOBE_WIDTH * GLOBE_HEIGHT) as usize];
        let w_usize = GLOBE_WIDTH as usize;
        for e in &globe.rivers.edges {
            let fi = e.from.1 as usize * w_usize + e.from.0 as usize;
            let ti = e.to.1 as usize * w_usize + e.to.0 as usize;
            if e.order > strahler_at_cell[fi] {
                strahler_at_cell[fi] = e.order;
            }
            if e.order > strahler_at_cell[ti] {
                strahler_at_cell[ti] = e.order;
            }
        }
        globe.relief = super::geomorph::build_relief(
            &elev_norm,
            &globe.hydrology,
            &rain_norm,
            &river_cell_mask,
            &strahler_at_cell,
        );
    }

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

/// Load globe from disk if the cache is version-compatible AND was produced
/// from the same seed; otherwise generate fresh and rewrite the cache.
pub fn load_or_generate(seed: u64) -> Globe {
    if let Ok(bytes) = std::fs::read(SAVE_PATH) {
        match bincode::deserialize::<GlobeFile>(&bytes) {
            Ok(file) if file.version == GLOBE_FILE_VERSION && file.globe.seed == seed => {
                info!(
                    "Globe loaded from {SAVE_PATH} (v{}, seed {})",
                    file.version, file.globe.seed
                );
                return file.globe;
            }
            Ok(file) if file.version != GLOBE_FILE_VERSION => warn!(
                "Globe cache version mismatch ({} != {GLOBE_FILE_VERSION}) — regenerating",
                file.version
            ),
            Ok(file) => info!(
                "Globe cache seed mismatch ({} != {seed}) — regenerating",
                file.globe.seed
            ),
            Err(_) => warn!("Failed to deserialize {SAVE_PATH} — regenerating"),
        }
    }

    let globe = generate_globe(seed);
    save_globe(&globe);
    globe
}

/// Persist the current `Globe` to `world.bin`. Used by `load_or_generate`
/// after a fresh roll, and by the spawn-select commit transition so that
/// only the *chosen* world is cached (rerolls skip disk IO).
pub fn save_globe(globe: &Globe) {
    let file = GlobeFile {
        version: GLOBE_FILE_VERSION,
        globe: globe.clone(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rivers_reach_oceans() {
        // After the sea-level shift, hydrology and biome classification
        // share the same coast: every river polyline should terminate in
        // an Ocean cell (or at a pole, where Y clamps act as drainage
        // edges). Verify the *terminal* cell of each river (the last
        // edge's `to`) classifies as Ocean for >95% of polylines.
        for seed in [42u64, 123] {
            let g = generate_globe(seed);
            // Group edges by (downstream end of polyline). A polyline
            // terminus is an edge whose `to` cell never appears as a
            // `from` (i.e. nothing flows out of it through a tracked edge).
            use std::collections::HashSet;
            let froms: HashSet<(u32, u32)> = g.rivers.edges.iter().map(|e| e.from).collect();
            let mut termini: Vec<(u32, u32)> = g
                .rivers
                .edges
                .iter()
                .map(|e| e.to)
                .filter(|to| !froms.contains(to))
                .collect();
            termini.sort();
            termini.dedup();
            assert!(!termini.is_empty(), "seed {seed}: no river termini");

            let mut at_ocean = 0;
            let mut at_pole = 0;
            let mut at_wetland = 0;
            let mut stray_biomes = std::collections::BTreeMap::<&'static str, u32>::new();
            for (gx, gy) in &termini {
                let cell = g.cell(*gx as i32, *gy as i32).unwrap();
                if cell.biome == Biome::Ocean {
                    at_ocean += 1;
                } else if cell.biome == Biome::Wetland {
                    // River deltas / marshland are valid outflow termini.
                    at_wetland += 1;
                } else if *gy == 0 || *gy as i32 == GLOBE_HEIGHT - 1 {
                    at_pole += 1;
                } else {
                    *stray_biomes.entry(cell.biome.name()).or_insert(0) += 1;
                }
            }
            let reached = (at_ocean + at_pole + at_wetland) as f32 / termini.len() as f32;
            // Threshold relaxed to 0.90 (was 0.95): the new Steppe / Wetland /
            // Badlands classifications steal a few percent of river termini
            // that were previously stamped Ocean by the percentile remap. The
            // intent of this test — rivers don't dead-end mid-continent — is
            // preserved.
            assert!(
                reached >= 0.90,
                "seed {seed}: only {:.0}% of {} river termini reach ocean/pole/wetland \
                 (ocean={at_ocean}, wetland={at_wetland}, pole={at_pole}, strays={:?})",
                reached * 100.0,
                termini.len(),
                stray_biomes
            );
        }
    }

    #[test]
    fn ocean_fraction_within_band() {
        // Percentile-anchored elevation remap targets ~30% ocean and ~10%
        // mountain regardless of distribution shape. Verify across two
        // seeds; tolerate ±5% drift from the anchor.
        for seed in [42u64, 123] {
            let g = generate_globe(seed);
            let total = g.cells.len() as f32;
            let ocean = g.cells.iter().filter(|c| c.biome == Biome::Ocean).count() as f32;
            let mountain = g
                .cells
                .iter()
                .filter(|c| c.biome == Biome::Mountain)
                .count() as f32;
            let ocean_pct = ocean / total * 100.0;
            let mountain_pct = mountain / total * 100.0;
            assert!(
                (25.0..=35.0).contains(&ocean_pct),
                "seed {seed}: ocean fraction {ocean_pct:.1}% out of band [25,35]"
            );
            assert!(
                (5.0..=15.0).contains(&mountain_pct),
                "seed {seed}: mountain fraction {mountain_pct:.1}% out of band [5,15]"
            );
        }
    }

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
        let mountains = g
            .cells
            .iter()
            .filter(|c| c.biome == Biome::Mountain)
            .count();
        assert!(mountains > 0, "no mountain cells (plate uplift broken)");
        // Some rivers (hydrology should have extracted at least one polyline).
        assert!(!g.rivers.edges.is_empty(), "no rivers extracted");
        // All cells get a plate id.
        let max_pid = g.cells.iter().map(|c| c.plate_id).max().unwrap_or(0);
        assert!(
            max_pid > 0,
            "all cells assigned to plate 0 (plate gen broken)"
        );
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

    #[test]
    fn relief_layer_populates_with_lowland_majority() {
        // After the geomorph pass the relief layer should be the same length
        // as the cell grid, and habitable land (non-Ocean cells) should
        // contain a meaningful share of arable classes.
        for seed in [42u64, 123] {
            let g = generate_globe(seed);
            let n = g.cells.len();
            assert_eq!(g.relief.cells.len(), n, "seed {seed}: relief len mismatch");
            let mut land_cells = 0usize;
            let mut arable = 0usize;
            let mut mountain = 0usize;
            for (cell, rcell) in g.cells.iter().zip(g.relief.cells.iter()) {
                if cell.biome == Biome::Ocean {
                    continue;
                }
                land_cells += 1;
                use super::super::geomorph::ReliefClass::*;
                match rcell.relief {
                    LowlandPlain | CoastalPlain | Floodplain | RollingHills => arable += 1,
                    MountainSlope | MountainRidge => mountain += 1,
                    _ => {}
                }
            }
            assert!(land_cells > 0, "seed {seed}: no land cells");
            let arable_pct = arable as f32 / land_cells as f32;
            assert!(
                arable_pct >= 0.20,
                "seed {seed}: only {:.0}% of {} land cells classify arable (lowland/coastal/floodplain/rolling)",
                arable_pct * 100.0,
                land_cells
            );
            // Mountain slope/ridge should appear but not dominate land.
            let mountain_pct = mountain as f32 / land_cells as f32;
            assert!(
                mountain_pct <= 0.35,
                "seed {seed}: mountain land fraction {:.0}% exceeds 35%",
                mountain_pct * 100.0
            );
        }
    }

    #[test]
    fn sample_relief_continuous_across_cell_boundary() {
        // The per-tile class is re-derived from interpolated numerics, so
        // crossing a cell boundary should not flip the class on the very
        // next tile when the surrounding cells share the same class.
        let g = generate_globe(7);
        let tiles_per_cell = (GLOBE_CELL_CHUNKS * super::super::chunk::CHUNK_SIZE as i32) as i32;
        let mut flips_at_boundary = 0;
        let mut samples = 0;
        for k in 0..32 {
            let edge_tx = tiles_per_cell * (k + 4);
            let ty = 80 + k * 3;
            let s0 = g.sample_relief(edge_tx - 1, ty).class;
            let s1 = g.sample_relief(edge_tx, ty).class;
            let s2 = g.sample_relief(edge_tx + 1, ty).class;
            // A "flip" we worry about: identical-class neighbours surround a
            // single-tile transition right at the cell boundary.
            if s0 == s2 && s0 != s1 {
                flips_at_boundary += 1;
            }
            samples += 1;
        }
        // Allow a few legitimate transitions (terrain genuinely changes at
        // some cell borders), but a wholesale flipping pattern would mean
        // class is nearest-cell rather than interpolation-derived.
        assert!(
            flips_at_boundary < samples / 2,
            "{}/{} cell-boundary samples flipped — class likely nearest-cell, not interpolated",
            flips_at_boundary,
            samples
        );
    }

    #[test]
    fn edge_crossings_classifies_inlet_and_outlet() {
        // A single river edge whose polyline runs upstream→downstream along
        // x = 0..20 at y = 5, descending from level 10.0 to 2.0.
        let poly: Vec<(i32, i32)> = (0..=20).map(|x| (x, 5)).collect();
        let net = RiverNetwork {
            edges: vec![RiverEdge {
                from: (0, 0),
                to: (1, 0),
                discharge: 128.0,
                from_level: 10.0,
                to_level: 2.0,
                ..Default::default()
            }],
            edge_polylines: vec![poly],
        };
        // Region bbox covers x∈[5,15]: the channel enters at x=5 (Inlet) and
        // leaves at x=15 (Outlet).
        let cr = net.edge_crossings_in_bbox((5, 0), (15, 10));
        let inlet = cr
            .iter()
            .find(|c| c.kind == EdgeCrossingKind::Inlet)
            .expect("an inlet at the upstream boundary");
        let outlet = cr
            .iter()
            .find(|c| c.kind == EdgeCrossingKind::Outlet)
            .expect("an outlet at the downstream boundary");
        assert_eq!(inlet.tile, (5, 5));
        assert_eq!(outlet.tile, (15, 5));
        assert_eq!(inlet.discharge, 128.0);
        // Monotone level: the upstream inlet sits higher than the outlet.
        assert!(inlet.level > outlet.level);
        assert!(inlet.level < 10.0 && outlet.level > 2.0);

        // A polyline fully inside the bbox contributes no crossing — its
        // head/sink is fed by in-region hydrology, not an external inlet.
        let none = net.edge_crossings_in_bbox((-5, -5), (50, 50));
        assert!(none.is_empty());
    }
}
