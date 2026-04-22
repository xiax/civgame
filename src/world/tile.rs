/// Tile types for the world grid.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TileKind {
    #[default]
    Grass    = 0,
    Water    = 1,
    Stone    = 2,
    Forest   = 3,
    Farmland = 4,
    Road     = 5,
}

impl TileKind {
    pub fn is_passable(self) -> bool {
        !matches!(self, TileKind::Water)
    }
}

/// 4 bytes per tile — cache-friendly.
#[derive(Clone, Copy, Default)]
pub struct TileData {
    pub kind:      TileKind,
    pub elevation: u8,
    pub fertility: u8,
    /// bit 0: has_building, bit 1: has_road
    pub flags: u8,
}

impl TileData {
    pub fn is_passable(self) -> bool {
        self.kind.is_passable()
    }

    pub fn has_building(self) -> bool {
        self.flags & 0b01 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn water_not_passable() {
        let t = TileData { kind: TileKind::Water, ..Default::default() };
        assert!(!t.is_passable());
    }

    #[test]
    fn grass_passable() {
        let t = TileData { kind: TileKind::Grass, ..Default::default() };
        assert!(t.is_passable());
    }
}
