/// Tile types for the world grid.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TileKind {
    #[default]
    Grass = 0,
    Water = 1,
    Stone = 2,
    Forest = 3,
    Farmland = 4,
    Road = 5,
    Air = 6,  // open space — above ground or underground cavity
    Wall = 7, // solid rock/earth — blocks movement and LOS
    Ramp = 8, // slope — passable, allows ±1 Z movement
    Dirt = 9, // underground floor (carved cave ceiling/floor)
    Ore = 10, // ore-bearing rock; specific ore is in TileData.ore (OreKind)
}

impl TileKind {
    pub fn is_passable(self) -> bool {
        !matches!(self, TileKind::Water | TileKind::Wall | TileKind::Air | TileKind::Ore)
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
    /// Anything but Air and Water; Wall counts because it's the ceiling/floor
    /// of a tunnel below it.
    pub fn is_floor(self) -> bool {
        !matches!(self, TileKind::Air | TileKind::Water)
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
}
