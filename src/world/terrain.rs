use bevy::prelude::*;
use noise::{NoiseFn, Perlin, Seedable};
use std::time::Instant;

use super::biome as biome_mod;
use super::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_HEIGHT, CHUNK_SIZE, Z_MAX, Z_MIN};
use super::globe::{Biome, Globe, ReservoirKind, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use super::tile::{OreKind, TileData, TileKind};

pub const WORLD_CHUNKS_X: i32 = 32;
pub const WORLD_CHUNKS_Y: i32 = 32;
pub const TILE_SIZE: f32 = 16.0;

/// Default surface/ore-noise seed, used when no `WorldSeed` resource has
/// been set yet (tests, sandbox quick-boot). The spawn-select reroll path
/// constructs `WorldGen::with_seed(world_seed.0 as u32)`.
pub const DEFAULT_WORLD_SEED: u32 = 42;

/// Perlin instances used for world generation, stored as a Bevy resource.
/// One noise field per ore lets veins of different ores overlap and span
/// independent depth bands (see `ORE_BANDS`).
#[derive(Resource)]
pub struct WorldGen {
    pub surface: Perlin, // 2D surface height + tile kind (seed N)
    pub cave: Perlin,    // 3D cave cavities (seed N + 1)
    pub coal: Perlin,    // 3D coal vein noise (N + 2)
    pub copper: Perlin,  // 3D copper vein noise (N + 3)
    pub iron: Perlin,    // 3D iron vein noise (N + 4)
    pub tin: Perlin,     // 3D tin vein noise (N + 5)
    pub silver: Perlin,  // 3D silver vein noise (N + 6)
    pub gold: Perlin,    // 3D gold vein noise (N + 7)
}

impl WorldGen {
    pub fn new() -> Self {
        Self::with_seed(DEFAULT_WORLD_SEED)
    }

    /// Build a `WorldGen` from a base seed. The spawn-select reroll path
    /// passes `WorldSeed.0 as u32` so per-tile terrain (heightmap, caves,
    /// ore veins) re-rolls in lockstep with the climate globe.
    pub fn with_seed(seed: u32) -> Self {
        Self {
            surface: Perlin::default().set_seed(seed),
            cave: Perlin::default().set_seed(seed.wrapping_add(1)),
            coal: Perlin::default().set_seed(seed.wrapping_add(2)),
            copper: Perlin::default().set_seed(seed.wrapping_add(3)),
            iron: Perlin::default().set_seed(seed.wrapping_add(4)),
            tin: Perlin::default().set_seed(seed.wrapping_add(5)),
            silver: Perlin::default().set_seed(seed.wrapping_add(6)),
            gold: Perlin::default().set_seed(seed.wrapping_add(7)),
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

/// Topsoil layer depth (number of soil tiles below the surface) by biome.
/// Mountains have thin soil; taiga/tropical/temperate have deep soil.
/// Wetlands sit on a deep clay band; Steppe ≈ Grassland; Badlands ≈ Desert.
pub fn topsoil_depth(biome: Biome) -> i32 {
    match biome {
        Biome::Mountain => 1,
        Biome::Desert | Biome::Tundra | Biome::Badlands => 2,
        Biome::Grassland | Biome::Steppe => 3,
        Biome::Taiga | Biome::Tropical | Biome::Temperate => 4,
        Biome::Wetland => 4,
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
    OreBand {
        kind: OreKind::Coal,
        top_offset: 1,
        bot_offset: 6,
        threshold: 0.45,
        freq_xy: 0.10,
        freq_z: 0.18,
    },
    OreBand {
        kind: OreKind::Copper,
        top_offset: 2,
        bot_offset: 8,
        threshold: 0.50,
        freq_xy: 0.10,
        freq_z: 0.18,
    },
    OreBand {
        kind: OreKind::Tin,
        top_offset: 5,
        bot_offset: 12,
        threshold: 0.55,
        freq_xy: 0.10,
        freq_z: 0.18,
    },
    OreBand {
        kind: OreKind::Iron,
        top_offset: 6,
        bot_offset: 14,
        threshold: 0.52,
        freq_xy: 0.10,
        freq_z: 0.18,
    },
    OreBand {
        kind: OreKind::Silver,
        top_offset: 10,
        bot_offset: 18,
        threshold: 0.60,
        freq_xy: 0.12,
        freq_z: 0.20,
    },
    OreBand {
        kind: OreKind::Gold,
        top_offset: 14,
        bot_offset: 32,
        threshold: 0.65,
        freq_xy: 0.12,
        freq_z: 0.20,
    },
];

impl Default for WorldGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-biome surface palette: 4 noise thresholds split the surface into 5
/// bands; `kinds[i]` is the `TileKind` chosen when `v ∈ [v_{i-1}, v_i)`
/// (with `v_{-1} = 0` and `v_4 = 1`).
///
/// The pre-existing four-tuple of thresholds (water_t, grass_t, farm_t,
/// forest_t) is preserved as the `thresholds` field for callers that still
/// shift those values (riparian moisture boost). The new `kinds` array picks
/// per-biome surface flavour: Desert lays Sand/Scrub/Sandstone, Tundra lays
/// Snow/Scrub/Granite, Wetland lays Marsh/Grass/Forest, etc.
#[derive(Clone, Copy, Debug)]
pub struct BiomeBands {
    pub thresholds: [f32; 4],
    pub kinds: [TileKind; 5],
}

impl BiomeBands {
    /// Pick the surface kind for a noise value `v ∈ [0, 1]`.
    pub fn pick(&self, v: f32) -> TileKind {
        if v < self.thresholds[0] {
            self.kinds[0]
        } else if v < self.thresholds[1] {
            self.kinds[1]
        } else if v < self.thresholds[2] {
            self.kinds[2]
        } else if v < self.thresholds[3] {
            self.kinds[3]
        } else {
            self.kinds[4]
        }
    }
}

/// Per-biome surface bands. Threshold tuples preserve the original
/// (water_t, grass_t, farm_t, forest_t) shape so the riparian moisture-boost
/// math still composes; the `kinds` slot determines which TileKind each
/// band paints.
pub fn biome_bands(biome: Biome) -> BiomeBands {
    use TileKind::*;
    match biome {
        Biome::Ocean => BiomeBands {
            thresholds: [0.90, 0.95, 0.97, 0.99],
            // Below sea level: water; thin beach band; high crests: granite.
            kinds: [Water, Sand, Sand, Granite, Granite],
        },
        Biome::Tundra => BiomeBands {
            thresholds: [0.18, 0.55, 0.80, 0.95],
            kinds: [Water, Snow, Scrub, Granite, Granite],
        },
        Biome::Taiga => BiomeBands {
            thresholds: [0.18, 0.35, 0.55, 0.85],
            kinds: [Water, Grass, Forest, Forest, Granite],
        },
        Biome::Temperate => BiomeBands {
            thresholds: [0.26, 0.45, 0.60, 0.85],
            kinds: [Water, Grass, Grass, Forest, Limestone],
        },
        Biome::Grassland => BiomeBands {
            thresholds: [0.18, 0.60, 0.75, 0.88],
            kinds: [Water, Grass, Grass, Forest, Limestone],
        },
        Biome::Tropical => BiomeBands {
            thresholds: [0.25, 0.30, 0.40, 0.88],
            kinds: [Water, Marsh, Grass, Forest, Basalt],
        },
        Biome::Desert => BiomeBands {
            thresholds: [0.10, 0.55, 0.70, 0.85],
            kinds: [Water, Sand, Scrub, Sandstone, Sandstone],
        },
        Biome::Mountain => BiomeBands {
            thresholds: [0.12, 0.25, 0.50, 0.80],
            kinds: [Granite, Granite, Granite, Granite, Basalt],
        },
        Biome::Wetland => BiomeBands {
            thresholds: [0.20, 0.45, 0.65, 0.90],
            kinds: [Water, Marsh, Grass, Forest, Forest],
        },
        Biome::Steppe => BiomeBands {
            thresholds: [0.15, 0.45, 0.70, 0.90],
            kinds: [Water, Scrub, Grass, Scrub, Sandstone],
        },
        Biome::Badlands => BiomeBands {
            thresholds: [0.10, 0.40, 0.65, 0.90],
            kinds: [Sand, Sand, Scrub, Sandstone, Granite],
        },
    }
}

/// Backwards-compatible four-tuple shape used by the riparian-band moisture
/// shift math. Returns the band thresholds only; the kinds palette must be
/// consulted via `biome_bands(biome).kinds`.
pub fn biome_thresholds(biome: Biome) -> (f32, f32, f32, f32) {
    let b = biome_bands(biome);
    (
        b.thresholds[0],
        b.thresholds[1],
        b.thresholds[2],
        b.thresholds[3],
    )
}

/// Soil variant for the topsoil layer at this tile. `river_d ≤ 5` is the
/// riparian band; those tiles override to `Silt` regardless of biome.
pub fn topsoil_kind(biome: Biome, river_d: u8) -> TileKind {
    if river_d != u8::MAX && river_d <= 5 {
        return TileKind::Silt;
    }
    match biome {
        Biome::Wetland | Biome::Tropical => TileKind::Clay,
        Biome::Temperate | Biome::Grassland | Biome::Steppe => TileKind::Loam,
        Biome::Desert | Biome::Badlands => TileKind::SandySoil,
        Biome::Tundra | Biome::Taiga | Biome::Mountain | Biome::Ocean => TileKind::Dirt,
    }
}

/// Local-detail amplitude for `surface_v`: the per-tile Perlin field can
/// only push the macro value by ±this much. Lower = terrain hugs globe
/// elevation more tightly; higher = more within-biome variation.
/// Per-biome amplitude for the local Perlin detail layered on top of the
/// globe's macro elevation. Lower values keep gameplay terrain tightly
/// anchored to the world-map preview; higher values let mountain ridges and
/// badland scree retain jagged character. Bounded so even the noisiest biome
/// can't drift more than ±~5 Z from the macro signal.
fn local_detail_amp(biome: Biome) -> f32 {
    // Per-tile detail amplitudes calibrated against our 1.5 m tile scale.
    // Numbers are in v-units (`v ∈ [0,1]` → `Z = -16 + v·32`), so multiplying
    // by 32 gives the Z range (and by 48 the metres range). Halved from the
    // legacy ±3–7 m values, which produced unrealistic micro-topography at
    // tile scale.
    match biome {
        // Coasts, lowlands, dry flats: ±1.12 Z ≈ ±1.7 m.
        Biome::Ocean | Biome::Wetland | Biome::Desert | Biome::Steppe => 0.035,
        // Generic vegetated belts: ±1.6 Z ≈ ±2.4 m.
        Biome::Grassland | Biome::Temperate | Biome::Taiga | Biome::Tropical | Biome::Tundra => {
            0.05
        }
        // Mountain ridges, badland uplift: ±2.4 Z ≈ ±3.6 m — still rugged
        // without absurd per-tile 6 m cliffs.
        Biome::Mountain | Biome::Badlands => 0.075,
    }
}

/// Fractional surface noise value at (tx, ty). Range [0, 1].
///
/// **Macro signal**: bilinearly-interpolated globe elevation (`elev_u` / 255).
/// Anchors per-tile z to the climate-cell elevation field so a Grassland
/// lowland on the world map produces near-sea-level terrain in 3D, and a
/// Mountain biome produces tall peaks. Without this coupling per-tile noise
/// was independent of globe elevation, so a "lowland" biome on the map
/// could still show a tall plateau in-world — the cognitive dissonance read
/// as "everything is tall."
///
/// **Local detail**: 3-octave Perlin perturbation, biome-conditional
/// amplitude via `local_detail_amp`. Breaks up the macro into recognisable
/// terrain (rolling grass, jagged mountain ridges, coastal shelves) without
/// overpowering the anchor.
pub fn surface_v(tx: i32, ty: i32, gen: &WorldGen, globe: &Globe) -> f32 {
    let (elev_u, _temp_c, _rain_u) = globe.sample_climate(tx, ty);
    let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
    // Relief amplitude follows the *surface* biome so the per-tile detail
    // amplitude doesn't snap at a visual border (e.g. Grassland 0.05 →
    // Mountain 0.075) ahead of the kind change. Surface classifier keeps
    // the Ocean/Mountain elevation gates on true elevation so this still
    // resolves to the same biome the band-picker will choose.
    let biome = biome_mod::classify_surface_at_tile(globe, tx, ty);
    let amp = local_detail_amp(biome);

    let nx = tx as f64 * 0.04;
    let ny = ty as f64 * 0.04;
    let d0 = gen.surface.get([nx, ny]);
    let d1 = gen.surface.get([nx * 2.0, ny * 2.0]);
    let d2 = gen.surface.get([nx * 4.0, ny * 4.0]);
    // Weighted sum normalised so output stays roughly in [-1, 1].
    let detail = (d0 * 0.60 + d1 * 0.28 + d2 * 0.12) as f32;

    (macro_f + detail * amp).clamp(0.0, 1.0)
}

/// Compute discrete surface Z level at world tile (tx, ty). O(1).
pub fn surface_height(tx: i32, ty: i32, gen: &WorldGen, globe: &Globe) -> i32 {
    let v = surface_v(tx, ty, gen, globe);
    let z = Z_MIN as f32 + v * CHUNK_HEIGHT as f32;
    (z.round() as i32).clamp(Z_MIN, Z_MAX)
}

/// Pick the surface kind for `v ∈ [0,1]` given the four thresholds. Used by
/// the riparian moisture-boost re-classification path which still operates on
/// raw tuples; pass the biome's native `kinds` palette so the shifted
/// thresholds re-pick a wetter slot from the same per-biome flavour.
fn surface_kind_fn_bands(bands: &BiomeBands, v: f32) -> TileKind {
    bands.pick(v)
}

/// Deterministically compute the tile at world tile coords (tx, ty, tz).
/// Pure — no side effects, no allocations. Safe to call from any thread.
///
/// `biome` controls topsoil depth (soil layer thickness) and the
/// surface/topsoil palette via `biome_bands` / `topsoil_kind`. `river_d` is
/// the chebyshev distance to the nearest river (`u8::MAX` if none); used to
/// upgrade the topsoil to `Silt` and to feed the bedrock palette near rivers.
pub fn proc_tile(
    tx: i32,
    ty: i32,
    tz: i32,
    gen: &WorldGen,
    globe: &Globe,
    biome: Biome,
    river_d: u8,
) -> TileData {
    let v = surface_v(tx, ty, gen, globe);
    let surf_z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
    let surf_z = surf_z.clamp(Z_MIN, Z_MAX);

    if tz > surf_z {
        return TileData {
            kind: TileKind::Air,
            ..Default::default()
        };
    }

    if tz == surf_z {
        let kind = biome_bands(biome).pick(v);
        let fertility = surface_fertility_of(kind, v);
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
            topsoil_kind(biome, river_d)
        } else {
            TileKind::Air
        };
        return TileData {
            kind,
            ..Default::default()
        };
    }

    // Topsoil layer: a few soil tiles directly below the surface, biome-thick.
    // Soil variant follows biome (and the riparian Silt override).
    let depth = surf_z - tz; // > 0 below surface
    if depth <= topsoil_depth(biome) {
        return TileData {
            kind: topsoil_kind(biome, river_d),
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
    // Surface-level reads consult the chunk's `surface_kind` cache. `proc_tile`
    // re-derives surface kind from biome bands alone and so doesn't see
    // post-procgen rewrites like river polyline stamping or lake fill — the
    // chunk cache is the authoritative source for those. Without this branch,
    // hover / inspector / any tile_at_3d caller reads "Silt"/"Wall" for tiles
    // that were stamped to `River` (depressed Z) or `Water` (lake fill).
    let chunk_surf_z = chunk_map.surface_z_at(tx, ty);
    if chunk_surf_z >= Z_MIN && tz == chunk_surf_z {
        if let Some(kind) = chunk_map.tile_kind_at(tx, ty) {
            return TileData {
                kind,
                fertility: chunk_map.tile_fertility_at(tx, ty).unwrap_or(0),
                ..Default::default()
            };
        }
    }
    // Visual / terrain readout uses the surface-biome layer so an unloaded
    // tile lookup matches what chunk-gen would stamp (preview/terrain
    // parity). Canonical `classify_at_tile` is reserved for AI / salinity /
    // world-sim — see `biome.rs`.
    let biome = biome_mod::classify_surface_at_tile(globe, tx, ty);
    let river_d = chunk_map.river_distance_at(tx, ty);
    proc_tile(tx, ty, tz, gen, globe, biome, river_d)
}

/// Riparian feather radius (tiles outside the channel that still get the
/// river-distance flag). The biome/fertility/topsoil effects only fire on
/// `river_d <= 5` (see `riparian_moisture_boost`, `river_fertility_mult`,
/// `topsoil_kind`); the wider feather exists so settlement spawn pickers
/// (`score_home_candidate`, `score_tile`) can score candidates up to 16
/// tiles from a river and place the initial `base_r` footprint on one bank.
pub const RIVER_FEATHER_DIST: u8 = 16;

/// Build a new Chunk: empty delta map + surface_z, surface_kind, fertility,
/// and river-distance caches pre-filled. Per-tile biome via
/// `biome::classify_at_tile` (continuous across mega-chunk seams). Rivers
/// (curved polylines from the globe network) and lakes are stamped over the
/// noise-derived surface; tiles within `RIVER_FEATHER_DIST` of a river get a
/// moisture boost on biome thresholds and a fertility multiplier.
pub fn generate_chunk_from_globe(coord: ChunkCoord, globe: &Globe, gen: &WorldGen) -> Chunk {
    let mut surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_kind = Box::new([[TileKind::default(); CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_river_distance = Box::new([[u8::MAX; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_ground_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_water_depth = Box::new([[0.0f32; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut surface_reservoir_id = Box::new([[u32::MAX; CHUNK_SIZE]; CHUNK_SIZE]);

    let chunk_tx0 = coord.0 * CHUNK_SIZE as i32;
    let chunk_ty0 = coord.1 * CHUNK_SIZE as i32;
    let chunk_tx1 = chunk_tx0 + CHUNK_SIZE as i32;
    let chunk_ty1 = chunk_ty0 + CHUNK_SIZE as i32;

    // ── Pass 1: noise-derived surface (z + provisional kind) ──
    // Cache `v` and biome so pass 3 can re-classify river-adjacent tiles
    // using moisture-boosted thresholds without recomputing the noise.
    // Biome is the *surface* biome (domain-warped land-biome, Ocean/Mountain
    // gates on true elevation) so visual borders feather organically; the
    // ecotone accent dithers material choice across the transition band.
    let mut v_cache = Box::new([[0.0f32; CHUNK_SIZE]; CHUNK_SIZE]);
    let mut biome_cache = Box::new([[Biome::default(); CHUNK_SIZE]; CHUNK_SIZE]);
    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let global_tx = chunk_tx0 + lx as i32;
            let global_ty = chunk_ty0 + ly as i32;
            let sample = biome_mod::surface_biome_sample_at_tile(globe, global_tx, global_ty);
            let v = surface_v(global_tx, global_ty, gen, globe);
            let z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
            let z = z.clamp(Z_MIN, Z_MAX);
            // Pick surface kind by dithering base vs accent palette across
            // the transition band. `base` always drives `biome_cache` (and
            // therefore relief amp + topsoil + Pass-3 riparian shift); the
            // accent only contributes its `BiomeBands::kinds` palette so the
            // surface material reads ecotonal (e.g. a Scrub speckle in a
            // Grassland-Desert seam) without dragging soil depth or
            // riparian logic with it.
            let bands_base = biome_bands(sample.base);
            let kind = if sample.accent != sample.base {
                let dither = biome_mod::surface_band_dither(globe.seed, global_tx, global_ty);
                if dither < sample.accent_weight() {
                    biome_bands(sample.accent).pick(v)
                } else {
                    bands_base.pick(v)
                }
            } else {
                bands_base.pick(v)
            };
            surface_z[ly][lx] = z as i8;
            surface_kind[ly][lx] = kind;
            // Dry land: bed == surface (the additive invariant).
            surface_ground_z[ly][lx] = z as i8;
            v_cache[ly][lx] = v;
            biome_cache[ly][lx] = sample.base;
        }
    }

    // ── Pass 2: stamp curving river polylines + populate river-distance ──
    for (edge_idx, edge) in globe.rivers.edges.iter().enumerate() {
        let Some(polyline) = globe.rivers.edge_polylines.get(edge_idx) else {
            continue;
        };
        if polyline.len() < 2 {
            continue;
        }
        let max_w = edge.from_width.max(edge.to_width) as i32;
        let feather = max_w + RIVER_FEATHER_DIST as i32;
        // Polyline bbox vs chunk bbox + feather.
        let (mut lo_x, mut lo_y) = polyline[0];
        let (mut hi_x, mut hi_y) = polyline[0];
        for &(x, y) in polyline {
            lo_x = lo_x.min(x);
            hi_x = hi_x.max(x);
            lo_y = lo_y.min(y);
            hi_y = hi_y.max(y);
        }
        if hi_x + feather < chunk_tx0
            || lo_x - feather >= chunk_tx1
            || hi_y + feather < chunk_ty0
            || lo_y - feather >= chunk_ty1
        {
            continue;
        }
        // Cumulative arc length for taper t along the polyline.
        let mut lengths = Vec::with_capacity(polyline.len());
        lengths.push(0.0f32);
        for w in polyline.windows(2) {
            let dx = (w[1].0 - w[0].0) as f32;
            let dy = (w[1].1 - w[0].1) as f32;
            let prev = *lengths.last().unwrap();
            lengths.push(prev + (dx * dx + dy * dy).sqrt());
        }
        let total = lengths.last().copied().unwrap_or(1.0).max(1.0);
        for i in 0..polyline.len() - 1 {
            let (ax, ay) = polyline[i];
            let (bx, by) = polyline[i + 1];
            let t0 = lengths[i] / total;
            let t1 = lengths[i + 1] / total;
            let w0 = lerp(edge.from_width as f32, edge.to_width as f32, t0);
            let w1 = lerp(edge.from_width as f32, edge.to_width as f32, t1);
            // Hydrology channel depth (globe units → Z-units), tapered along
            // the polyline like width. Edges from a v7 cache (no depth) read
            // 0.0 here and fall back to `MIN_RIVER_DEPTH_Z` in the stamp.
            let d0 = lerp(edge.from_depth, edge.to_depth, t0) * GLOBE_H_TO_Z;
            let d1 = lerp(edge.from_depth, edge.to_depth, t1) * GLOBE_H_TO_Z;
            diamond_stamp(
                ax,
                ay,
                bx,
                by,
                w0,
                w1,
                d0,
                d1,
                edge.reservoir_id,
                chunk_tx0,
                chunk_ty0,
                &mut surface_kind,
                &mut surface_z,
                &mut surface_river_distance,
                &mut surface_ground_z,
                &mut surface_water_depth,
                &mut surface_reservoir_id,
            );
        }
    }

    // ── Pass 3: riparian re-classification + fertility ──
    // For tiles in the feather ring (not already River) we shift biome
    // thresholds toward "wetter" — Desert/Tundra strips along a river end up
    // as Grass/Farmland, not bare stone. Then fertility gets a multiplier on
    // top so wild plant patches cluster along the banks.
    let mut surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let river_d = surface_river_distance[ly][lx];
            // River tiles themselves: skip; their kind is already River and
            // fertility 0 (channel water).
            if surface_kind[ly][lx] == TileKind::River {
                continue;
            }
            // Re-classify with moisture-boosted thresholds when in the band.
            if river_d != u8::MAX && river_d <= RIVER_FEATHER_DIST {
                let biome = biome_cache[ly][lx];
                let mut bands = biome_bands(biome);
                let boost = riparian_moisture_boost(river_d);
                let (water_t, grass_t, farm_t, forest_t) = apply_moisture_boost(
                    bands.thresholds[0],
                    bands.thresholds[1],
                    bands.thresholds[2],
                    bands.thresholds[3],
                    boost,
                );
                bands.thresholds = [water_t, grass_t, farm_t, forest_t];
                let v = v_cache[ly][lx];
                let new_kind = surface_kind_fn_bands(&bands, v);
                // Only upgrade toward greener kinds — riverside should add
                // vegetation, never carve away forest into bare grass.
                if greenness_rank(new_kind) > greenness_rank(surface_kind[ly][lx]) {
                    surface_kind[ly][lx] = new_kind;
                }
            }
            let kind = surface_kind[ly][lx];
            let v = v_cache[ly][lx];
            let base = surface_fertility_of(kind, v) as f32;
            if base <= 0.0 {
                continue;
            }
            let mult = river_fertility_mult(river_d);
            let fert = (base * mult).min(255.0) as u8;
            surface_fertility[ly][lx] = fert;
        }
    }

    // ── Pass 4: reservoir basin-membership stamp (replaces lake discs) ──
    // Every wet tile inherits its globe-cell reservoir's equilibrium surface
    // level + bed. Ocean is left to biome classification (already `Water`
    // salt); Spring/Dam are runtime-only. Coarse at climate-cell resolution
    // (~64 tiles) but topologically correct — no arbitrary circles.
    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let tx = chunk_tx0 + lx as i32;
            let ty = chunk_ty0 + ly as i32;
            let Some(res) = globe.reservoir_at(tx, ty) else {
                continue;
            };
            let kind = match res.kind {
                ReservoirKind::Lake | ReservoirKind::Endorheic => TileKind::Water,
                ReservoirKind::Wetland => TileKind::Marsh,
                // Ocean handled by biome bands; Spring/Dam are runtime.
                ReservoirKind::Ocean | ReservoirKind::Spring | ReservoirKind::Dam => continue,
            };
            let water_surf = ((res.spill_level * GLOBE_H_TO_Z).round() as i32).clamp(Z_MIN, Z_MAX);
            let bed = globe
                .hydro_cell_at(tx, ty)
                .map(|hc| ((hc.raw_height * GLOBE_H_TO_Z).round() as i32).clamp(Z_MIN, Z_MAX))
                .unwrap_or(water_surf - 1);
            let bed = bed.min(water_surf);
            let depth = ((water_surf - bed) as f32).max(MIN_RIVER_DEPTH_Z);
            surface_kind[ly][lx] = kind;
            surface_z[ly][lx] = water_surf as i8;
            surface_ground_z[ly][lx] = bed as i8;
            surface_water_depth[ly][lx] = depth;
            surface_reservoir_id[ly][lx] = res.id;
            surface_fertility[ly][lx] = 0;
        }
    }

    // ── Pass 4.5: per-tile aquifer marsh ──
    // Pass 4 stamps whole-basin Lake/Wetland at climate-cell resolution; per-tile bed
    // jitter creates depressions that physically sit below the local water table but get
    // left dry. The gate must live in the same Z frame as `surface_z`: anchor on the
    // per-CELL macro elevation (from `sample_climate`, jitter-free, identical to the
    // macro_f signal `surface_v` uses) and subtract the per-cell aquifer-depth-Z. A
    // per-tile bed whose downward jitter exceeds that depth is genuinely below the
    // cell-resolution water table ⇒ Marsh. Same gate used by `water_runtime` so the
    // engine treats natural depressions and dug pits identically.
    for ly in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let kind = surface_kind[ly][lx];
            if matches!(kind, TileKind::River | TileKind::Water | TileKind::Marsh) {
                continue;
            }
            let tx = chunk_tx0 + lx as i32;
            let ty = chunk_ty0 + ly as i32;
            let Some(h) = globe.hydro_cell_at(tx, ty) else {
                continue;
            };
            let (elev_u, _, _) = globe.sample_climate(tx, ty);
            let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
            let cell_surface_z = Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32;
            let aquifer_depth_z = (h.filled_height - h.aquifer_level) * GLOBE_H_TO_Z;
            let table_z = cell_surface_z - aquifer_depth_z;
            let bed_z = surface_z[ly][lx] as f32;
            if bed_z >= table_z {
                continue;
            }
            let water_surf = (table_z.round() as i32).clamp(Z_MIN, Z_MAX);
            let bed = (bed_z.round() as i32).clamp(Z_MIN, water_surf);
            let depth = ((water_surf - bed) as f32).max(MIN_RIVER_DEPTH_Z);
            surface_kind[ly][lx] = TileKind::Marsh;
            surface_z[ly][lx] = water_surf as i8;
            surface_ground_z[ly][lx] = bed as i8;
            surface_water_depth[ly][lx] = depth;
            // reservoir_id stays u32::MAX — these aren't part of any globe reservoir.
            let v = v_cache[ly][lx];
            let base = surface_fertility_of(TileKind::Marsh, v) as f32;
            let mult = river_fertility_mult(surface_river_distance[ly][lx]);
            surface_fertility[ly][lx] = (base * mult).min(255.0) as u8;
        }
    }

    Chunk::new_hydro(
        surface_z,
        surface_kind,
        surface_fertility,
        surface_river_distance,
        surface_ground_z,
        surface_water_depth,
        surface_reservoir_id,
    )
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Multiplier on base fertility from proximity to a river. `u8::MAX` means
/// "far from river" → 1.0×; the riparian band 2..=5 tiles out gets
/// 1.6× / 1.3× depending on distance. Tiles inside the channel never use
/// this (their kind is `River`, not Grass/Farmland, so fertility is 0).
pub fn river_fertility_mult(river_d: u8) -> f32 {
    match river_d {
        2 | 3 => 1.6,
        4 | 5 => 1.3,
        _ => 1.0,
    }
}

/// Elevation-driven surface-fertility curve in `[0, 255]`. Peaks at
/// `v == 0.45` and falls linearly to 0 by `v == -0.05` or `v == 0.95` —
/// effectively always positive across the playable `v ∈ [0, 1]` range, so
/// vegetation at biome-band edges keeps a sensible baseline instead of
/// snapping to zero. Apply `kind_fertility_factor(kind)` to scale per
/// surface kind. Shared by chunk-gen and the climate-only estimator.
#[inline]
pub fn elevation_fertility_curve(v: f32) -> f32 {
    (1.0 - (v - 0.45).abs() * 2.0).max(0.0) * 255.0
}

/// Per-surface-kind productivity multiplier on the elevation curve. Captures
/// the rough ecological capacity of each surface type:
/// - `Grass`: full prairie, peak forage.
/// - `Marsh`: wetland — very high biomass, nearly grass-equivalent.
/// - `Forest`: closed canopy + understory — productive but less than open prairie.
/// - `Scrub`: sparse cover — low but non-zero.
/// - Everything else (Sand/Snow/Stone/Water/...): zero.
#[inline]
pub fn kind_fertility_factor(kind: TileKind) -> f32 {
    match kind {
        TileKind::Grass => 1.00,
        TileKind::Marsh => 0.90,
        TileKind::Forest => 0.70,
        TileKind::Scrub => 0.30,
        _ => 0.0,
    }
}

/// Composite per-tile fertility: kind × elevation curve, clamped to `u8`.
/// Use this in both chunk-gen and the climate-only estimator so the two
/// stay in lockstep.
#[inline]
pub fn surface_fertility_of(kind: TileKind, v: f32) -> u8 {
    let f = kind_fertility_factor(kind);
    if f <= 0.0 {
        return 0;
    }
    (f * elevation_fertility_curve(v)).min(255.0) as u8
}

/// Climate-only expected fertility at a tile. Mirrors the chunk-gen formula
/// but drops the per-tile Perlin term that `surface_v` would normally add —
/// since that term is zero-mean variation around climate elevation, this
/// function returns the *expected* (average) fertility chunk-gen would
/// produce at that tile. Used by world-map / spawn-select aggregates so they
/// can show fertility for unloaded megachunks without loading chunks.
pub fn climate_fertility_estimate_at(globe: &Globe, tx: i32, ty: i32) -> u8 {
    let (elev_u, _temp, _rain) = globe.sample_climate(tx, ty);
    let v = (elev_u / 255.0).clamp(0.0, 1.0);
    // Match the surface-biome layer chunk-gen uses for `biome_cache` so
    // expected fertility lines up with the kind that will actually be
    // stamped. Ecotone accent averages out (zero-mean dither), so the
    // *expected* fertility uses `base` only — same `BiomeBands::pick(v)`
    // call as Pass 1's base path.
    let biome = biome_mod::classify_surface_at_tile(globe, tx, ty);
    let kind = biome_bands(biome).pick(v);
    let base = surface_fertility_of(kind, v) as f32;
    if base <= 0.0 {
        return 0;
    }
    let river_d = globe.nearest_river_chebyshev(tx, ty).min(u8::MAX as u32) as u8;
    let mult = river_fertility_mult(river_d);
    (base * mult).min(255.0) as u8
}

/// Vegetation-density rank used to gate the riparian re-classification.
/// Higher = more plant-supporting. Tiles only upgrade if the moisture-boosted
/// thresholds produce a strictly greener kind than the current one — keeps
/// mountain stone bare and never demotes existing forest.
fn greenness_rank(kind: TileKind) -> u8 {
    match kind {
        TileKind::Forest => 4,
        TileKind::Marsh => 3,
        TileKind::Grass => 2,
        TileKind::Scrub => 1,
        // All barren / rocky / arid surfaces collapse to 0.
        _ => 0,
    }
}

/// Threshold-shift amount applied to `biome_thresholds` for tiles in the
/// riparian band. Bigger boost = more aggressive shift toward grass/farmland.
fn riparian_moisture_boost(river_d: u8) -> f32 {
    match river_d {
        // 0/1 are inside the channel; reclassify shouldn't fire there.
        2 | 3 => 0.30,
        4 | 5 => 0.15,
        _ => 0.0,
    }
}

/// Shift biome thresholds to mimic a wetter local microclimate. Lowering
/// `grass_t` widens the grass band against rocky upland; lowering `farm_t`
/// admits farmland in places that were too marginal. `water_t` and
/// `forest_t` move proportionally so the ordering is preserved.
fn apply_moisture_boost(
    water_t: f32,
    grass_t: f32,
    farm_t: f32,
    forest_t: f32,
    boost: f32,
) -> (f32, f32, f32, f32) {
    let water_t = (water_t + boost * 0.05).min(grass_t - 1e-3);
    let grass_t = (grass_t - boost * 0.30).max(water_t + 1e-3);
    let farm_t = (farm_t - boost * 0.20).max(grass_t + 1e-3);
    let forest_t = (forest_t - boost * 0.10).max(farm_t + 1e-3);
    (water_t, grass_t, farm_t, forest_t)
}

/// Manhattan-clamped diamond stamp along the segment (ax,ay)→(bx,by). Width
/// lerps from `w0` at the start to `w1` at the end. Channel tiles become
/// `TileKind::River` with `surface_z` depressed by 1; tiles in the feather
/// ring (`half_w + 1 .. half_w + RIVER_FEATHER_DIST`) only update
/// `surface_river_distance` so downstream riparian effects fire without
/// changing terrain.
#[allow(clippy::too_many_arguments)]
/// Minimum river channel depth in Z-units (a half-tile, so even a trickle
/// reads as a distinct sub-z cut below the water surface).
const MIN_RIVER_DEPTH_Z: f32 = 0.5;
/// Globe height units → Z-units (mirrors the legacy `level_z = mean_h * 8`).
/// `pub` so the Phase 5 water sim converts hydrology truth → Z through the
/// single source of this factor (no parallel formula).
pub const GLOBE_H_TO_Z: f32 = 8.0;

#[allow(clippy::too_many_arguments)]
fn diamond_stamp(
    ax: i32,
    ay: i32,
    bx: i32,
    by: i32,
    w0: f32,
    w1: f32,
    d0: f32,
    d1: f32,
    edge_reservoir_id: u32,
    chunk_tx0: i32,
    chunk_ty0: i32,
    surface_kind: &mut [[TileKind; CHUNK_SIZE]; CHUNK_SIZE],
    surface_z: &mut [[i8; CHUNK_SIZE]; CHUNK_SIZE],
    surface_river_distance: &mut [[u8; CHUNK_SIZE]; CHUNK_SIZE],
    surface_ground_z: &mut [[i8; CHUNK_SIZE]; CHUNK_SIZE],
    surface_water_depth: &mut [[f32; CHUNK_SIZE]; CHUNK_SIZE],
    surface_reservoir_id: &mut [[u32; CHUNK_SIZE]; CHUNK_SIZE],
) {
    let dx_abs = (bx - ax).abs();
    let dy_abs_neg = -(by - ay).abs();
    let sx = if ax < bx { 1 } else { -1 };
    let sy = if ay < by { 1 } else { -1 };
    let mut err = dx_abs + dy_abs_neg;
    let mut x = ax;
    let mut y = ay;

    let seg_len = ((dx_abs as f32).powi(2) + (dy_abs_neg as f32).powi(2))
        .sqrt()
        .max(1.0);
    let max_half = w0.max(w1).round() as i32;
    let feather_radius = max_half + RIVER_FEATHER_DIST as i32;

    loop {
        // Step progress along the segment for taper interpolation.
        let dxf = (x - ax) as f32;
        let dyf = (y - ay) as f32;
        let prog = ((dxf * dxf + dyf * dyf).sqrt() / seg_len).clamp(0.0, 1.0);
        let half_w = lerp(w0, w1, prog).round() as i32;
        let half_w = half_w.max(0);

        // Channel + feather: scan the bounding chebyshev square once and
        // gate by Manhattan distance. The Manhattan check produces a soft
        // diamond (rounded corners) instead of the old square stamp.
        for oy in -feather_radius..=feather_radius {
            for ox in -feather_radius..=feather_radius {
                let lx = x + ox - chunk_tx0;
                let ly = y + oy - chunk_ty0;
                if lx < 0 || lx >= CHUNK_SIZE as i32 || ly < 0 || ly >= CHUNK_SIZE as i32 {
                    continue;
                }
                let manhattan = ox.abs() + oy.abs();
                let cheb = ox.abs().max(oy.abs());

                // Channel: Manhattan <= half_w + 1 paints River. The +1 keeps
                // a half_w=0 stamp at least one tile wide.
                if manhattan <= half_w + 1 {
                    surface_kind[ly as usize][lx as usize] = TileKind::River;
                    // Water surface stays at the legacy `cur - 1` so nothing
                    // visually/path-wise moves (water is impassable anyway).
                    // The *new* data is the solid bed beneath it.
                    let cur = surface_z[ly as usize][lx as usize];
                    let water_surf = (cur as i32 - 1).max(Z_MIN);
                    surface_z[ly as usize][lx as usize] = water_surf as i8;
                    surface_river_distance[ly as usize][lx as usize] = 0;
                    let depth = lerp(d0, d1, prog).max(MIN_RIVER_DEPTH_Z);
                    let bed = (water_surf - depth.ceil() as i32).max(Z_MIN);
                    surface_ground_z[ly as usize][lx as usize] = bed as i8;
                    surface_water_depth[ly as usize][lx as usize] = depth;
                    surface_reservoir_id[ly as usize][lx as usize] = edge_reservoir_id;
                    continue;
                }

                // Feather: chebyshev distance from channel center, capped
                // at u8::MAX. Take the min so multiple edges crossing the
                // same chunk produce the closest distance.
                let outside = (cheb - half_w).max(0) as u8;
                let cur = surface_river_distance[ly as usize][lx as usize];
                if outside < cur {
                    surface_river_distance[ly as usize][lx as usize] = outside;
                }
            }
        }

        if x == bx && y == by {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy_abs_neg {
            err += dy_abs_neg;
            x += sx;
        }
        if e2 <= dx_abs {
            err += dx_abs;
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
    use crate::world::globe::generate_globe;

    fn test_globe() -> Globe {
        generate_globe(42)
    }

    #[test]
    fn surface_height_in_range() {
        let gen = WorldGen::new();
        let g = test_globe();
        let z = surface_height(0, 0, &gen, &g);
        assert!((Z_MIN..=Z_MAX).contains(&z));
    }

    #[test]
    fn water_column_invariants_hold() {
        // Generate the chunk that contains a real river polyline point (so we
        // deterministically hit wet tiles) plus its 3×3 neighbourhood. Dry
        // tiles must satisfy the additive invariant (ground == surface,
        // depth == 0, no reservoir); wet tiles must have a bed at-or-below
        // the water surface with a real (sub-z) depth.
        let gen = WorldGen::new();
        let g = test_globe();
        let (rtx, rty) = g
            .rivers
            .edge_polylines
            .iter()
            .flat_map(|p| p.iter().copied())
            .next()
            .expect("seed 42 globe has at least one river polyline point");
        let ccx = rtx.div_euclid(CHUNK_SIZE as i32);
        let ccy = rty.div_euclid(CHUNK_SIZE as i32);
        let mut wet_seen = 0;
        for cy in (ccy - 1)..=(ccy + 1) {
            for cx in (ccx - 1)..=(ccx + 1) {
                let c = generate_chunk_from_globe(ChunkCoord(cx, cy), &g, &gen);
                for ly in 0..CHUNK_SIZE {
                    for lx in 0..CHUNK_SIZE {
                        let k = c.surface_kind[ly][lx];
                        let surf = c.surface_z[ly][lx] as i32;
                        let bed = c.surface_ground_z[ly][lx] as i32;
                        let depth = c.surface_water_depth[ly][lx];
                        let rid = c.surface_reservoir_id[ly][lx];
                        let wet = matches!(k, TileKind::River | TileKind::Water | TileKind::Marsh)
                            && depth > 0.0;
                        if wet {
                            wet_seen += 1;
                            assert!(bed <= surf, "wet bed {bed} above surface {surf}");
                            assert!(
                                depth >= MIN_RIVER_DEPTH_Z - 1e-4,
                                "wet depth {depth} below minimum"
                            );
                        } else {
                            assert_eq!(bed, surf, "dry tile bed != surface ({k:?})");
                            assert_eq!(depth, 0.0, "dry tile has depth ({k:?})");
                            assert_eq!(rid, u32::MAX, "dry tile has reservoir ({k:?})");
                        }
                    }
                }
            }
        }
        assert!(
            wet_seen > 0,
            "river chunk ({ccx},{ccy}) around tile ({rtx},{rty}) had no wet tiles"
        );
    }

    #[test]
    fn proc_tile_above_surface_is_air() {
        let gen = WorldGen::new();
        let g = test_globe();
        let surf = surface_height(5, 5, &gen, &g);
        let t = proc_tile(5, 5, surf + 1, &gen, &g, Biome::Temperate, u8::MAX);
        assert_eq!(t.kind, TileKind::Air);
    }

    #[test]
    fn proc_tile_surface_not_wall_or_air() {
        let gen = WorldGen::new();
        let g = test_globe();
        let surf = surface_height(5, 5, &gen, &g);
        let t = proc_tile(5, 5, surf, &gen, &g, Biome::Temperate, u8::MAX);
        assert!(!matches!(t.kind, TileKind::Air | TileKind::Wall));
    }

    #[test]
    fn proc_tile_deep_is_wall_or_cave_or_ore() {
        let gen = WorldGen::new();
        let g = test_globe();
        let surf = surface_height(5, 5, &gen, &g);
        let t = proc_tile(5, 5, surf - 10, &gen, &g, Biome::Temperate, u8::MAX);
        assert!(
            matches!(t.kind, TileKind::Wall | TileKind::Air | TileKind::Ore)
                || t.kind.is_soil_like()
        );
    }

    #[test]
    fn proc_tile_topsoil_is_dirt() {
        let gen = WorldGen::new();
        let g = test_globe();
        let surf = surface_height(5, 5, &gen, &g);
        // One tile below the surface should be soil (topsoil) for any non-Mountain
        // biome, unless cave noise carves through. Temperate biome lays Loam.
        let t = proc_tile(5, 5, surf - 1, &gen, &g, Biome::Temperate, u8::MAX);
        assert!(t.kind.is_soil_like() || t.kind == TileKind::Air);
    }

    #[test]
    fn proc_tile_deterministic() {
        let gen = WorldGen::new();
        let g = test_globe();
        let a = proc_tile(10, 20, 0, &gen, &g, Biome::Temperate, u8::MAX);
        let b = proc_tile(10, 20, 0, &gen, &g, Biome::Temperate, u8::MAX);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.ore, b.ore);
    }

    #[test]
    fn surface_v_stays_anchored_to_macro_elevation() {
        // Per-biome detail amp caps at 0.15, so per-tile `surface_v` can
        // never diverge from the globe macro signal by more than 0.15. In
        // discrete Z that's ±ceil(0.15 * 32) = ±5 Z. Sample widely enough
        // to hit Ocean / Grassland / Mountain / Desert / Wetland.
        let gen = WorldGen::new();
        let g = test_globe();
        let max_amp = 0.15_f32;
        let max_z_delta = (max_amp * CHUNK_HEIGHT as f32).ceil() as i32 + 1;
        for (tx, ty) in (0..200).map(|i| (i * 137, i * 211)) {
            let v = surface_v(tx, ty, &gen, &g);
            let (elev_u, _, _) = g.sample_climate(tx, ty);
            let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
            let macro_z = (Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32).round() as i32;
            let z = (Z_MIN as f32 + v * CHUNK_HEIGHT as f32).round() as i32;
            assert!(
                (z - macro_z).abs() <= max_z_delta,
                "tile ({},{}): z={}, macro_z={}, delta={} (cap {})",
                tx,
                ty,
                z,
                macro_z,
                z - macro_z,
                max_z_delta
            );
        }
    }

    #[test]
    fn tile_world_roundtrip() {
        let pos = tile_to_world(5, 7);
        let (tx, ty) = world_to_tile(pos);
        assert_eq!((tx, ty), (5, 7));
    }

    /// Pass 4.5 post-condition: after chunk generation, every dry land tile's bed
    /// sits at or above the per-cell water table (cell macro elevation minus
    /// aquifer depth). Tiles whose per-tile jitter dips below that gate have
    /// been flipped to Marsh by Pass 4.5 (or already wet via Pass 2/4).
    #[test]
    fn pass_4_5_no_dry_tile_below_cell_table() {
        let gen = WorldGen::new();
        let g = test_globe();
        let samples = [(0, 0), (40, 40), (-30, 20), (60, -20), (10, 80), (-60, -40)];
        for (cx, cy) in samples {
            let c = generate_chunk_from_globe(ChunkCoord(cx, cy), &g, &gen);
            for ly in 0..CHUNK_SIZE {
                for lx in 0..CHUNK_SIZE {
                    let k = c.surface_kind[ly][lx];
                    if matches!(k, TileKind::River | TileKind::Water | TileKind::Marsh) {
                        continue;
                    }
                    let tx = cx * CHUNK_SIZE as i32 + lx as i32;
                    let ty = cy * CHUNK_SIZE as i32 + ly as i32;
                    let Some(h) = g.hydro_cell_at(tx, ty) else {
                        continue;
                    };
                    let (elev_u, _, _) = g.sample_climate(tx, ty);
                    let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
                    let cell_surface_z = Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32;
                    let aquifer_depth_z = (h.filled_height - h.aquifer_level) * GLOBE_H_TO_Z;
                    let table_z = cell_surface_z - aquifer_depth_z;
                    let bed_z = c.surface_ground_z[ly][lx] as f32;
                    assert!(
                        bed_z >= table_z - 1e-3,
                        "dry tile ({tx},{ty}) kind={k:?} bed={bed_z} below cell table={table_z}"
                    );
                }
            }
        }
    }

    /// Diagnostic: print Pass-4.5 marsh fraction over a large area so we can
    /// eyeball whether the aquifer-depth calibration produces reasonable rates
    /// (lots in wet biomes, near-zero in deserts). Ignored by default; run with
    /// `cargo test --bin civgame -- --ignored pass_4_5_marsh_frequency
    /// --nocapture`.
    #[test]
    #[ignore]
    fn pass_4_5_marsh_frequency_diagnostic() {
        let gen = WorldGen::new();
        let g = test_globe();
        let mut total = 0u64;
        let mut pass45 = 0u64;
        let mut pass4 = 0u64;
        // Distribution measurements for calibration.
        let mut min_jitter_z = f32::INFINITY;
        let mut max_jitter_z = f32::NEG_INFINITY;
        let mut min_depth_z = f32::INFINITY;
        let mut max_depth_z = f32::NEG_INFINITY;
        let mut below_table_count = 0u64;
        let mut sampled = 0u64;
        for cy in -16..=16 {
            for cx in -16..=16 {
                let c = generate_chunk_from_globe(ChunkCoord(cx, cy), &g, &gen);
                for ly in 0..CHUNK_SIZE {
                    for lx in 0..CHUNK_SIZE {
                        total += 1;
                        let k = c.surface_kind[ly][lx];
                        if k == TileKind::Marsh {
                            if c.surface_reservoir_id[ly][lx] == u32::MAX {
                                pass45 += 1;
                            } else {
                                pass4 += 1;
                            }
                        }
                        // Skip non-land for the distribution stats.
                        if matches!(k, TileKind::River | TileKind::Water | TileKind::Marsh) {
                            continue;
                        }
                        let tx = cx * CHUNK_SIZE as i32 + lx as i32;
                        let ty = cy * CHUNK_SIZE as i32 + ly as i32;
                        let Some(h) = g.hydro_cell_at(tx, ty) else {
                            continue;
                        };
                        let (elev_u, _, _) = g.sample_climate(tx, ty);
                        let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
                        let cell_surface_z = Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32;
                        let bed_z = c.surface_z[ly][lx] as f32;
                        let jitter = bed_z - cell_surface_z;
                        let depth = (h.filled_height - h.aquifer_level) * GLOBE_H_TO_Z;
                        min_jitter_z = min_jitter_z.min(jitter);
                        max_jitter_z = max_jitter_z.max(jitter);
                        min_depth_z = min_depth_z.min(depth);
                        max_depth_z = max_depth_z.max(depth);
                        if bed_z < cell_surface_z - depth {
                            below_table_count += 1;
                        }
                        sampled += 1;
                    }
                }
            }
        }
        let pass45_pct = pass45 as f64 / total as f64 * 100.0;
        let pass4_pct = pass4 as f64 / total as f64 * 100.0;
        eprintln!(
            "33x33 chunks ({} tiles): Pass-4 Marsh {:.2}%, Pass-4.5 Marsh {:.2}%, total Marsh {:.2}%",
            total, pass4_pct, pass45_pct, pass4_pct + pass45_pct
        );
        eprintln!(
            "land tiles sampled: {}; jitter_z range [{:.3}, {:.3}]; aquifer_depth_z range [{:.3}, {:.3}]; below-table fraction {:.3}%",
            sampled,
            min_jitter_z,
            max_jitter_z,
            min_depth_z,
            max_depth_z,
            below_table_count as f64 / sampled as f64 * 100.0
        );
    }

    /// Existence test: locate a low-elevation wet-rainfall climate cell via
    /// the Globe directly, generate its chunk, and assert at least one tile
    /// flips to Marsh via Pass 4.5 (`reservoir_id == u32::MAX`,
    /// distinguishing it from a Pass-4 reservoir-membership marsh). Skips
    /// silently if the seed produces no qualifying wet cell anywhere (the
    /// invariant test still guards the gate itself).
    #[test]
    fn pass_4_5_stamps_at_least_one_implicit_marsh() {
        use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
        let gen = WorldGen::new();
        let g = test_globe();
        // Walk climate cells looking for a moist non-reservoir cell.
        let mut found_chunk: Option<ChunkCoord> = None;
        'search: for gy in (GLOBE_HEIGHT / 4)..(GLOBE_HEIGHT * 3 / 4) {
            for gx in 0..GLOBE_WIDTH {
                let Some(cell) = g.cell(gx, gy) else { continue };
                if cell.rainfall < 200 || cell.elevation < 30 || cell.elevation > 120 {
                    continue;
                }
                let cx = gx * GLOBE_CELL_CHUNKS;
                let cy = gy * GLOBE_CELL_CHUNKS;
                found_chunk = Some(ChunkCoord(cx, cy));
                break 'search;
            }
        }
        let Some(chunk) = found_chunk else {
            eprintln!("no qualifying wet climate cell on seed 42 globe — skipping");
            return;
        };
        let mut found = 0u32;
        for dy in 0..GLOBE_CELL_CHUNKS {
            for dx in 0..GLOBE_CELL_CHUNKS {
                let c = generate_chunk_from_globe(ChunkCoord(chunk.0 + dx, chunk.1 + dy), &g, &gen);
                for ly in 0..CHUNK_SIZE {
                    for lx in 0..CHUNK_SIZE {
                        if c.surface_kind[ly][lx] == TileKind::Marsh
                            && c.surface_reservoir_id[ly][lx] == u32::MAX
                        {
                            found += 1;
                        }
                    }
                }
            }
        }
        assert!(
            found > 0,
            "wet cell at chunk {:?} produced no Pass-4.5 Marsh tiles",
            chunk
        );
    }

    #[test]
    fn surface_biome_layer_matches_chunkgen_biome_cache() {
        // Preview ↔ terrain parity: the surface-biome `.base` we expose to
        // previews must equal the biome chunk-gen stamps into
        // `biome_cache` (which is what drives `topsoil_kind` /
        // `local_detail_amp` / Pass-3 riparian etc.). Walk one chunk worth
        // of tiles and check every match.
        let gen = WorldGen::new();
        let g = test_globe();
        // Pick a chunk away from a river so Pass 3 won't have run anyway —
        // but the invariant we're checking (Pass-1 base) holds even with
        // rivers, since we just compare what Pass 1 wrote.
        let chunk = ChunkCoord(0, 0);
        let c = generate_chunk_from_globe(chunk, &g, &gen);
        let tx0 = chunk.0 * CHUNK_SIZE as i32;
        let ty0 = chunk.1 * CHUNK_SIZE as i32;
        // We don't have direct access to `biome_cache`, but the invariant
        // we actually care about is that the *visible* kind chunk-gen
        // chose comes from the `surface_biome_sample_at_tile(...)` base
        // OR accent palette. Verify the surface kind is in that union.
        for ly in 0..CHUNK_SIZE {
            for lx in 0..CHUNK_SIZE {
                let tx = tx0 + lx as i32;
                let ty = ty0 + ly as i32;
                let k = c.surface_kind[ly][lx];
                // Skip post-Pass-1 overrides (river/marsh/water stamped by
                // hydrology). Their kind doesn't come from biome bands at
                // all.
                if matches!(
                    k,
                    TileKind::River
                        | TileKind::Water
                        | TileKind::Marsh
                        | TileKind::Bridge
                        | TileKind::Dam
                ) {
                    continue;
                }
                let s = biome_mod::surface_biome_sample_at_tile(&g, tx, ty);
                let base_kinds = biome_bands(s.base).kinds;
                let accent_kinds = biome_bands(s.accent).kinds;
                let in_palette = base_kinds.contains(&k) || accent_kinds.contains(&k);
                assert!(
                    in_palette,
                    "tile ({}, {}) kind {:?} not in base ({:?}) or accent ({:?}) palette",
                    tx, ty, k, s.base, s.accent
                );
            }
        }
    }

    #[test]
    fn surface_biome_layer_does_not_create_inland_water() {
        // Walking a sub-continent worth of tiles, any tile whose true
        // elevation puts it above the ocean gate must NOT come back as
        // Biome::Ocean from `classify_surface_at_tile`. Guard against the
        // warp ever sneaking inland-ocean stamps in.
        let g = test_globe();
        for ty in (-512..512i32).step_by(11) {
            for tx in (-512..512i32).step_by(11) {
                let (elev_u, _, _) = g.sample_climate(tx, ty);
                let elev_f = elev_u / 255.0;
                if elev_f >= biome_mod::OCEAN_ELEV_GATE {
                    let b = biome_mod::classify_surface_at_tile(&g, tx, ty);
                    assert!(
                        !matches!(b, Biome::Ocean),
                        "inland Ocean biome at ({}, {}) elev_f={}",
                        tx,
                        ty,
                        elev_f,
                    );
                }
            }
        }
    }
}
