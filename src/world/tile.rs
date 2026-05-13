/// Tile types for the world grid.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TileKind {
    #[default]
    Grass = 0,
    Water = 1,
    Stone = 2,
    Forest = 3,
    /// Hot/dry sandy surface. Reuses the slot freed by removing Farmland.
    Sand = 4,
    Road = 5,
    Air = 6,    // open space — above ground or underground cavity
    Wall = 7,   // solid rock/earth — blocks movement and LOS
    Ramp = 8,   // slope — passable, allows ±1 Z movement
    Dirt = 9,   // underground floor (carved cave ceiling/floor)
    Ore = 10,   // ore-bearing rock; specific ore is in TileData.ore (OreKind)
    River = 11, // freshwater channel (sibling of Water; impassable, but distinguishable)
    // ── New surface variants ──
    Snow = 12,  // tundra/cold surface
    Marsh = 13, // wetland surface (passable, slow)
    Scrub = 14, // dry sparse-vegetation (steppe / badlands / arid grassland)
    // ── Stone lithologies (`is_stone_like`) ──
    Granite = 15,   // hard, slow to mine; cold/mountain biomes
    Limestone = 16, // soft sedimentary; warm lowlands; higher mining yield
    Sandstone = 17, // arid sedimentary; deserts/badlands
    Basalt = 18,    // volcanic; tropical/coastal/Mountain core
    // ── Soil variants (`is_soil_like`) ──
    Loam = 19,      // fertile temperate / grassland topsoil
    Silt = 20,      // riverbank topsoil; very fertile
    Clay = 21,      // wet topsoil; tropical/wetland
    SandySoil = 22, // dry desert/badlands topsoil
}

impl TileKind {
    pub fn is_passable(self) -> bool {
        !matches!(
            self,
            TileKind::Water | TileKind::River | TileKind::Wall | TileKind::Air | TileKind::Ore
        )
    }

    /// Solid tiles cannot be entered from any direction.
    pub fn is_solid(self) -> bool {
        matches!(self, TileKind::Wall | TileKind::Ore)
    }

    /// Opaque tiles block line of sight.
    pub fn is_opaque(self) -> bool {
        matches!(self, TileKind::Wall | TileKind::Ore)
    }

    /// Whether this tile can support an agent standing on top of it.
    /// Anything but Air, Water, and River; Wall counts because it's the
    /// ceiling/floor of a tunnel below it.
    pub fn is_floor(self) -> bool {
        !matches!(self, TileKind::Air | TileKind::Water | TileKind::River)
    }

    /// Water-shaped: ocean/lake (`Water`) or river channel (`River`).
    /// Use this when behaviour ("don't walk into it", "looks blue") doesn't
    /// care whether the water is fresh or salt.
    pub fn is_water_like(self) -> bool {
        matches!(self, TileKind::Water | TileKind::River)
    }

    /// Drinkable freshwater. Currently only `River`; lakes stay `Water` until
    /// `LakeBasin` learns a fresh/salt flag.
    pub fn is_freshwater(self) -> bool {
        matches!(self, TileKind::River)
    }

    /// True when the tile carries some kind of water (fresh or salt); the
    /// caller is responsible for using `water_kind_at` to disambiguate when
    /// it matters (e.g. drinking / collecting). Rivers are always fresh.
    pub fn is_drinkable_candidate(self) -> bool {
        matches!(self, TileKind::Water | TileKind::River | TileKind::Marsh)
    }

    /// Generic "this tile is rock" — covers the legacy `Stone` plus all four
    /// lithology variants, plus underground bedrock walls and ore tiles. Used
    /// by `carve_tile` for mining-yield routing and by writability checks.
    pub fn is_stone_like(self) -> bool {
        matches!(
            self,
            TileKind::Stone
                | TileKind::Granite
                | TileKind::Limestone
                | TileKind::Sandstone
                | TileKind::Basalt
                | TileKind::Wall
                | TileKind::Ore
        )
    }

    /// Generic "this tile is topsoil" — legacy `Dirt` plus the four soil
    /// variants. Used by plant-fertility plumbing and farmland-yard
    /// writability checks.
    pub fn is_soil_like(self) -> bool {
        matches!(
            self,
            TileKind::Dirt | TileKind::Loam | TileKind::Silt | TileKind::Clay | TileKind::SandySoil
        )
    }

    /// Mining yield count when this stone-like tile is carved. Soft sedimentary
    /// rock (Limestone) yields more per swing than hard igneous/metamorphic
    /// (Granite/Basalt). Sandstone matches Granite for now. Wall and Ore are
    /// not routed through this path — see `carve_tile`.
    pub fn stone_yield_count(self) -> u32 {
        match self {
            TileKind::Limestone => 3,
            TileKind::Stone
            | TileKind::Granite
            | TileKind::Sandstone
            | TileKind::Basalt
            | TileKind::Wall => 2,
            _ => 0,
        }
    }

    /// Multiplier applied to a plant's per-tick growth/fertility when growing
    /// on this soil. `Grass` is 1.0 baseline; soils diverge from there.
    pub fn soil_fertility_mult(self) -> f32 {
        match self {
            TileKind::Loam => 1.5,
            TileKind::Silt => 1.4,
            TileKind::Clay => 1.0,
            TileKind::Dirt => 1.0,
            TileKind::SandySoil => 0.6,
            TileKind::Grass => 1.0,
            _ => 1.0,
        }
    }
}

/// Specific ore embedded in a `TileKind::Ore` tile. Stored as a `u8` in
/// `TileData.ore` to keep `TileData` POD-friendly.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OreKind {
    #[default]
    None = 0,
    Copper = 1,
    Tin = 2,
    Iron = 3,
    Coal = 4,
    Gold = 5,
    Silver = 6,
}

impl OreKind {
    pub fn from_u8(v: u8) -> OreKind {
        match v {
            1 => OreKind::Copper,
            2 => OreKind::Tin,
            3 => OreKind::Iron,
            4 => OreKind::Coal,
            5 => OreKind::Gold,
            6 => OreKind::Silver,
            _ => OreKind::None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn name(self) -> &'static str {
        match self {
            OreKind::None => "None",
            OreKind::Copper => "Copper",
            OreKind::Tin => "Tin",
            OreKind::Iron => "Iron",
            OreKind::Coal => "Coal",
            OreKind::Gold => "Gold",
            OreKind::Silver => "Silver",
        }
    }
}

/// 5 bytes per tile — cache-friendly. `ore` is meaningful only when
/// `kind == TileKind::Ore`; otherwise it's `OreKind::None` (0).
#[derive(Clone, Copy, Default)]
pub struct TileData {
    pub kind: TileKind,
    pub elevation: u8,
    pub fertility: u8,
    /// bit 0: has_building, bit 1: has_road, bit 2: explored, bit 3: currently_visible
    pub flags: u8,
    pub ore: u8,
}

impl TileData {
    pub fn is_passable(self) -> bool {
        self.kind.is_passable()
    }

    pub fn has_building(self) -> bool {
        self.flags & 0b0001 != 0
    }

    pub fn is_explored(self) -> bool {
        self.flags & 0b0100 != 0
    }

    pub fn is_visible(self) -> bool {
        self.flags & 0b1000 != 0
    }

    pub fn ore_kind(self) -> OreKind {
        OreKind::from_u8(self.ore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn water_not_passable() {
        let t = TileData {
            kind: TileKind::Water,
            ..Default::default()
        };
        assert!(!t.is_passable());
    }

    #[test]
    fn grass_passable() {
        let t = TileData {
            kind: TileKind::Grass,
            ..Default::default()
        };
        assert!(t.is_passable());
    }

    #[test]
    fn wall_not_passable() {
        let t = TileData {
            kind: TileKind::Wall,
            ..Default::default()
        };
        assert!(!t.is_passable());
        assert!(t.kind.is_solid());
        assert!(t.kind.is_opaque());
    }

    #[test]
    fn air_not_passable() {
        let t = TileData {
            kind: TileKind::Air,
            ..Default::default()
        };
        assert!(!t.is_passable());
    }

    #[test]
    fn ramp_passable() {
        let t = TileData {
            kind: TileKind::Ramp,
            ..Default::default()
        };
        assert!(t.is_passable());
    }

    #[test]
    fn new_surfaces_passable() {
        for k in [
            TileKind::Sand,
            TileKind::Snow,
            TileKind::Marsh,
            TileKind::Scrub,
        ] {
            assert!(k.is_passable(), "{:?} should be passable", k);
            assert!(k.is_floor(), "{:?} should be floor", k);
        }
    }

    #[test]
    fn stone_variants_classified() {
        for k in [
            TileKind::Granite,
            TileKind::Limestone,
            TileKind::Sandstone,
            TileKind::Basalt,
            TileKind::Stone,
        ] {
            assert!(k.is_stone_like());
            assert!(k.is_passable());
        }
    }

    #[test]
    fn soil_variants_classified() {
        for k in [
            TileKind::Loam,
            TileKind::Silt,
            TileKind::Clay,
            TileKind::SandySoil,
            TileKind::Dirt,
        ] {
            assert!(k.is_soil_like());
            assert!(k.is_passable());
        }
    }

    #[test]
    fn limestone_softer_than_granite() {
        assert!(TileKind::Limestone.stone_yield_count() > TileKind::Granite.stone_yield_count());
    }

    #[test]
    fn loam_more_fertile_than_sandy() {
        assert!(TileKind::Loam.soil_fertility_mult() > TileKind::SandySoil.soil_fertility_mult());
    }
}
