//! Procedural floristic regions.
//!
//! After hydrology and relief are built, `build_flora_regions(&Globe)` runs a
//! flood-fill over land cells (skipping ocean) and partitions them into
//! `FloraRegion`s by latitude band + dominant moisture/relief signature. Each
//! region carries one of 12 `FloraRealmKind` labels (see
//! `simulation::plant_catalog::FloraRealmKind`) which the species catalog
//! gates wild spawn on (`PlantCatalog::is_native_to`).
//!
//! The result lives on `Globe.flora_regions: FloraRegionMap` and is
//! bincode-serialised alongside the rest of the globe. `GLOBE_FILE_VERSION`
//! bumped to 11 to invalidate older caches.

use crate::collections::AHashMap;
use serde::{Deserialize, Serialize};

use crate::simulation::plant_catalog::FloraRealmKind;
use crate::world::globe::{Globe, GLOBE_HEIGHT, GLOBE_WIDTH};

/// Stable per-region id. `0` is reserved for "no region" (ocean cells).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FloraRegionId(pub u16);

impl FloraRegionId {
    pub const NONE: Self = Self(0);
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// One floristic region: a connected landmass cluster (possibly split by
/// latitude band) with a chosen realm and a deterministic flavor name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FloraRegion {
    pub id: FloraRegionId,
    pub realm: FloraRealmKindWire,
    pub flavor_name: String,
    /// Approximate area in climate cells. Used by world-map overlay only.
    pub cell_count: u32,
    /// One representative cell (centre-of-mass approximation) for UI.
    pub anchor_cell: (i32, i32),
}

/// Wire-serializable mirror of `FloraRealmKind`. `FloraRealmKind` itself
/// lives in the simulation crate; we keep a parallel enum here for the
/// globe-side bincode payload so the dependency direction stays clean
/// (`world` → `simulation` would be a cycle).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FloraRealmKindWire {
    Boreal,
    Taiga,
    TempForest,
    Mediterranoid,
    GrasslandTemp,
    GrasslandTrop,
    RainforestTrop,
    DesertHot,
    DesertCold,
    MontaneTemp,
    MontaneTrop,
    CoastalWetland,
}

impl FloraRealmKindWire {
    pub fn from_kind(k: FloraRealmKind) -> Self {
        match k {
            FloraRealmKind::Boreal => Self::Boreal,
            FloraRealmKind::Taiga => Self::Taiga,
            FloraRealmKind::TempForest => Self::TempForest,
            FloraRealmKind::Mediterranoid => Self::Mediterranoid,
            FloraRealmKind::GrasslandTemp => Self::GrasslandTemp,
            FloraRealmKind::GrasslandTrop => Self::GrasslandTrop,
            FloraRealmKind::RainforestTrop => Self::RainforestTrop,
            FloraRealmKind::DesertHot => Self::DesertHot,
            FloraRealmKind::DesertCold => Self::DesertCold,
            FloraRealmKind::MontaneTemp => Self::MontaneTemp,
            FloraRealmKind::MontaneTrop => Self::MontaneTrop,
            FloraRealmKind::CoastalWetland => Self::CoastalWetland,
        }
    }
    pub fn to_kind(self) -> FloraRealmKind {
        match self {
            Self::Boreal => FloraRealmKind::Boreal,
            Self::Taiga => FloraRealmKind::Taiga,
            Self::TempForest => FloraRealmKind::TempForest,
            Self::Mediterranoid => FloraRealmKind::Mediterranoid,
            Self::GrasslandTemp => FloraRealmKind::GrasslandTemp,
            Self::GrasslandTrop => FloraRealmKind::GrasslandTrop,
            Self::RainforestTrop => FloraRealmKind::RainforestTrop,
            Self::DesertHot => FloraRealmKind::DesertHot,
            Self::DesertCold => FloraRealmKind::DesertCold,
            Self::MontaneTemp => FloraRealmKind::MontaneTemp,
            Self::MontaneTrop => FloraRealmKind::MontaneTrop,
            Self::CoastalWetland => FloraRealmKind::CoastalWetland,
        }
    }
}

/// Per-cell region id grid + region table. Empty `Default` for legacy
/// bincode caches that don't carry the field.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FloraRegionMap {
    /// Per-cell `FloraRegionId`, length `GLOBE_WIDTH * GLOBE_HEIGHT`,
    /// row-major. `FloraRegionId::NONE` = ocean / unclassified.
    pub cell_region: Vec<u16>,
    /// Region table indexed by `FloraRegionId.0 - 1` (id 0 reserved).
    pub regions: Vec<FloraRegion>,
}

impl FloraRegionMap {
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
    pub fn region_at_cell(&self, gx: i32, gy: i32) -> Option<&FloraRegion> {
        if gx < 0 || gy < 0 || gx >= GLOBE_WIDTH || gy >= GLOBE_HEIGHT {
            return None;
        }
        let idx = (gy * GLOBE_WIDTH + gx) as usize;
        let id = *self.cell_region.get(idx)?;
        if id == 0 {
            return None;
        }
        self.regions.get((id - 1) as usize)
    }
}

/// Climate-cell-resolution lookup for a tile.
pub fn floristic_region_at_tile<'a>(
    globe: &'a Globe,
    tile_x: i32,
    tile_y: i32,
) -> Option<&'a FloraRegion> {
    let tiles_per_cell =
        (crate::world::globe::GLOBE_CELL_CHUNKS * crate::world::chunk::CHUNK_SIZE as i32) as f32;
    let gx = (tile_x as f32 / tiles_per_cell)
        .floor()
        .rem_euclid(GLOBE_WIDTH as f32) as i32;
    let gy = ((tile_y as f32 / tiles_per_cell).floor() as i32).clamp(0, GLOBE_HEIGHT - 1);
    globe.flora_regions.region_at_cell(gx, gy)
}

/// Approximate latitude band for a globe row. Six bands from north pole to
/// south pole.
fn latitude_band(gy: i32) -> LatBand {
    let mid = GLOBE_HEIGHT / 2;
    let dist = (gy - mid).abs();
    let frac = dist as f32 / (GLOBE_HEIGHT / 2) as f32;
    if frac > 0.80 {
        LatBand::Polar
    } else if frac > 0.55 {
        LatBand::Subpolar
    } else if frac > 0.35 {
        LatBand::Temperate
    } else if frac > 0.20 {
        LatBand::Subtropical
    } else {
        LatBand::Tropical
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
enum LatBand {
    Polar,
    Subpolar,
    Temperate,
    Subtropical,
    Tropical,
}

/// Map climate signature → realm. Picks based on dominant biome class +
/// latitude band + relief.
fn pick_realm(lat: LatBand, dominant: BiomeBucket, mountainy: bool) -> FloraRealmKind {
    use crate::world::globe::Biome;
    match (lat, dominant, mountainy) {
        (LatBand::Polar | LatBand::Subpolar, _, _) if matches!(dominant.biome, Biome::Tundra) => {
            FloraRealmKind::Boreal
        }
        (LatBand::Subpolar, _, _) if matches!(dominant.biome, Biome::Taiga) => FloraRealmKind::Taiga,
        (LatBand::Temperate, _, true) => FloraRealmKind::MontaneTemp,
        (LatBand::Tropical, _, true) => FloraRealmKind::MontaneTrop,
        (LatBand::Temperate, _, _) => match dominant.biome {
            Biome::Temperate => FloraRealmKind::TempForest,
            Biome::Grassland | Biome::Steppe => FloraRealmKind::GrasslandTemp,
            Biome::Desert | Biome::Badlands => FloraRealmKind::DesertCold,
            Biome::Wetland => FloraRealmKind::CoastalWetland,
            _ => FloraRealmKind::TempForest,
        },
        (LatBand::Subtropical, _, _) => match dominant.biome {
            Biome::Desert | Biome::Badlands => FloraRealmKind::DesertHot,
            Biome::Wetland => FloraRealmKind::CoastalWetland,
            Biome::Tropical => FloraRealmKind::GrasslandTrop,
            Biome::Grassland => FloraRealmKind::Mediterranoid,
            _ => FloraRealmKind::Mediterranoid,
        },
        (LatBand::Tropical, _, _) => match dominant.biome {
            Biome::Tropical | Biome::Wetland => FloraRealmKind::RainforestTrop,
            Biome::Grassland | Biome::Steppe => FloraRealmKind::GrasslandTrop,
            Biome::Desert | Biome::Badlands => FloraRealmKind::DesertHot,
            _ => FloraRealmKind::GrasslandTrop,
        },
        _ => FloraRealmKind::TempForest,
    }
}

#[derive(Copy, Clone, Debug)]
struct BiomeBucket {
    biome: crate::world::globe::Biome,
}

/// Generate a deterministic flavor name from realm + seed + region id.
/// Small per-realm lexicon — terse evocative roots.
fn flavor_name(realm: FloraRealmKind, seed: u64, region_id: u16) -> String {
    let lex: &[&str] = match realm {
        FloraRealmKind::Boreal => &["Hyperborea", "Frostmere", "Aurorath", "Tundril", "Glacis"],
        FloraRealmKind::Taiga => &["Conifron", "Tannenmark", "Spruceloom", "Pinevale", "Larcholt"],
        FloraRealmKind::TempForest => &["Oakreach", "Brackenfen", "Leafholm", "Greenward", "Sylvalt"],
        FloraRealmKind::Mediterranoid => &["Olivelands", "Sunmark", "Cypria", "Vinemere", "Arida"],
        FloraRealmKind::GrasslandTemp => &["Veldwide", "Steppmark", "Pampera", "Grasshalt", "Prarith"],
        FloraRealmKind::GrasslandTrop => &["Sahelmark", "Savannath", "Drysavanna", "Mongala", "Llantana"],
        FloraRealmKind::RainforestTrop => &["Vermara", "Canopria", "Mossridge", "Vine-Hollows", "Selva"],
        FloraRealmKind::DesertHot => &["Saharan", "Erghaze", "Sandreef", "Calcaria", "Dunemark"],
        FloraRealmKind::DesertCold => &["Coldsand", "Bleakreach", "Mesabar", "Aridholt", "Saltflat"],
        FloraRealmKind::MontaneTemp => &["Highbarrow", "Cragveld", "Spineholm", "Stonepike", "Alpenrath"],
        FloraRealmKind::MontaneTrop => &["Cloudridge", "Mistpeak", "Skyloft", "Verbridge", "Andean"],
        FloraRealmKind::CoastalWetland => &["Tidewash", "Mareshold", "Estuara", "Mangrovia", "Surfmark"],
    };
    // Splitmix64 mix of (seed, region_id, realm tag) → pick lex entry.
    let mut h = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(region_id as u64 * 0x517C_C1B7_2722_0A95)
        .wrapping_add(realm as u8 as u64 * 0x9E37_79B9);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    let pick = (h as usize) % lex.len();
    lex[pick].to_string()
}

/// Build `FloraRegionMap` from a fully-generated `Globe`. Called from
/// `generate_globe` after `build_hydrology` + `build_relief`. Pure of Bevy
/// world state.
pub fn build_flora_regions(globe: &Globe) -> FloraRegionMap {
    let total_cells = (GLOBE_WIDTH * GLOBE_HEIGHT) as usize;
    let mut cell_region: Vec<u16> = vec![0; total_cells];
    let mut regions: Vec<FloraRegion> = Vec::new();

    let is_land = |gx: i32, gy: i32| -> bool {
        if gx < 0 || gy < 0 || gx >= GLOBE_WIDTH || gy >= GLOBE_HEIGHT {
            return false;
        }
        let idx = (gy * GLOBE_WIDTH + gx) as usize;
        !matches!(
            globe.cells.get(idx).map(|c| c.biome),
            Some(crate::world::globe::Biome::Ocean) | None
        )
    };

    // Flood-fill land cells. Each connected component becomes one or more
    // regions split by latitude band.
    let mut visited = vec![false; total_cells];
    for sy in 0..GLOBE_HEIGHT {
        for sx in 0..GLOBE_WIDTH {
            let sidx = (sy * GLOBE_WIDTH + sx) as usize;
            if visited[sidx] || !is_land(sx, sy) {
                continue;
            }
            // BFS over connected land.
            let mut queue = std::collections::VecDeque::new();
            queue.push_back((sx, sy));
            visited[sidx] = true;
            // Group cells by (lat_band, biome_bucket) so a single huge
            // continent splits naturally between climate zones.
            let mut groups: AHashMap<(LatBand, BiomeKey), Vec<(i32, i32)>> = AHashMap::default();
            while let Some((cx, cy)) = queue.pop_front() {
                let cidx = (cy * GLOBE_WIDTH + cx) as usize;
                let cell = &globe.cells[cidx];
                let lat = latitude_band(cy);
                let bk = BiomeKey::from(cell.biome);
                groups.entry((lat, bk)).or_default().push((cx, cy));
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    let nx = ((cx + dx).rem_euclid(GLOBE_WIDTH)) as i32;
                    let ny = cy + dy;
                    if ny < 0 || ny >= GLOBE_HEIGHT {
                        continue;
                    }
                    let nidx = (ny * GLOBE_WIDTH + nx) as usize;
                    if visited[nidx] || !is_land(nx, ny) {
                        continue;
                    }
                    visited[nidx] = true;
                    queue.push_back((nx, ny));
                }
            }
            // Promote each group to a region. Sort by (lat, biome) so the
            // AHashMap iteration order — which is keyed on a per-process
            // hasher state — cannot leak into region ids / cell_region.
            let mut groups_sorted: Vec<((LatBand, BiomeKey), Vec<(i32, i32)>)> =
                groups.into_iter().collect();
            groups_sorted.sort_by_key(|((lat, bk), _)| (*lat as u8, bk.0));
            for ((lat, biome_key), cells) in groups_sorted {
                if cells.is_empty() {
                    continue;
                }
                let region_id = (regions.len() + 1) as u16;
                let mountainy = matches!(biome_key.to_biome(), crate::world::globe::Biome::Mountain);
                let realm = pick_realm(
                    lat,
                    BiomeBucket {
                        biome: biome_key.to_biome(),
                    },
                    mountainy,
                );
                let flavor = flavor_name(realm, globe.seed, region_id);
                // Centre-of-mass anchor.
                let sum_x: i64 = cells.iter().map(|(x, _)| *x as i64).sum();
                let sum_y: i64 = cells.iter().map(|(_, y)| *y as i64).sum();
                let n = cells.len() as i64;
                let anchor = ((sum_x / n.max(1)) as i32, (sum_y / n.max(1)) as i32);
                let cell_count = cells.len() as u32;
                regions.push(FloraRegion {
                    id: FloraRegionId(region_id),
                    realm: FloraRealmKindWire::from_kind(realm),
                    flavor_name: flavor,
                    cell_count,
                    anchor_cell: anchor,
                });
                for (cx, cy) in cells {
                    let i = (cy * GLOBE_WIDTH + cx) as usize;
                    cell_region[i] = region_id;
                }
            }
            // Cap regions at u16::MAX to fit the cell_region slot.
            if regions.len() >= u16::MAX as usize {
                break;
            }
        }
    }

    FloraRegionMap {
        cell_region,
        regions,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct BiomeKey(u8);

impl BiomeKey {
    fn from(b: crate::world::globe::Biome) -> Self {
        Self(b as u8)
    }
    fn to_biome(self) -> crate::world::globe::Biome {
        use crate::world::globe::Biome;
        match self.0 {
            0 => Biome::Ocean,
            1 => Biome::Tundra,
            2 => Biome::Taiga,
            3 => Biome::Temperate,
            4 => Biome::Grassland,
            5 => Biome::Tropical,
            6 => Biome::Desert,
            7 => Biome::Mountain,
            8 => Biome::Wetland,
            9 => Biome::Steppe,
            10 => Biome::Badlands,
            _ => Biome::Temperate,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — Phase 7 of biome-native plants follow-ups. Globe generation is
// expensive (~hundreds of ms); each test runs once per seed.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::globe::generate_globe;

    /// Same seed → identical region count, identical realm/flavor at every
    /// region id. Catches accidental nondeterminism in `build_flora_regions`
    /// (e.g. HashMap iteration order leaking into output).
    #[test]
    fn flora_regions_deterministic_under_same_seed() {
        let g1 = generate_globe(987_654);
        let g2 = generate_globe(987_654);
        let r1 = &g1.flora_regions.regions;
        let r2 = &g2.flora_regions.regions;
        assert_eq!(
            r1.len(),
            r2.len(),
            "region count must match across runs of same seed"
        );
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.realm, b.realm);
            assert_eq!(a.flavor_name, b.flavor_name);
            assert_eq!(a.cell_count, b.cell_count);
            assert_eq!(a.anchor_cell, b.anchor_cell);
        }
        assert_eq!(g1.flora_regions.cell_region, g2.flora_regions.cell_region);
    }
}
