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
}

impl TileKind {
    pub fn is_passable(self) -> bool {
        !matches!(self, TileKind::Water | TileKind::Wall | TileKind::Air)
    }

    /// Solid tiles cannot be entered from any direction.
    pub fn is_solid(self) -> bool {
        matches!(self, TileKind::Wall)
    }

    /// Opaque tiles block line of sight.
    pub fn is_opaque(self) -> bool {
        matches!(self, TileKind::Wall)
    }
}

/// 4 bytes per tile — cache-friendly.
#[derive(Clone, Copy, Default)]
pub struct TileData {
    pub kind: TileKind,
    pub elevation: u8,
    pub fertility: u8,
    /// bit 0: has_building, bit 1: has_road, bit 2: explored, bit 3: currently_visible
    pub flags: u8,
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
