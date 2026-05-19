use crate::simulation::construction::{BedMap, ChairMap, LoomMap, TableMap, WorkbenchMap};
use crate::world::tile::TileKind;

pub const FURNITURE_SPEED_FACTOR: f32 = 0.5;

pub fn tile_speed_multiplier(kind: TileKind) -> f32 {
    match kind {
        TileKind::Grass | TileKind::Stone | TileKind::Ramp => 1.0,
        TileKind::Road | TileKind::Bridge => 1.4,
        TileKind::Forest => 0.7,
        TileKind::Dirt => 0.9,
        // New climate surfaces
        TileKind::Sand => 0.75,
        TileKind::Snow => 0.6,
        TileKind::Marsh => 0.4,
        TileKind::Scrub => 0.9,
        // Stone lithologies — match generic Stone
        TileKind::Granite | TileKind::Limestone | TileKind::Sandstone | TileKind::Basalt => 1.0,
        // Soil variants — match generic Dirt; SandySoil is a touch slower
        TileKind::Loam | TileKind::Silt | TileKind::Clay | TileKind::Cropland => 0.9,
        TileKind::SandySoil => 0.85,
        TileKind::Water | TileKind::River | TileKind::Air | TileKind::Wall | TileKind::Ore => 0.0,
    }
}

/// Cost of one grass-equivalent step. The chunk-router edge cost is
/// `BASE_STEP_COST`-scaled per border crossing, so `detour.rs` derives
/// its router-units→tiles factor from this constant rather than
/// hardcoding it (keeps the two coherent if step cost is ever retuned).
pub const BASE_STEP_COST: u16 = 100;
pub const IMPASSABLE: u16 = u16::MAX;

pub fn tile_step_cost(kind: TileKind) -> u16 {
    let m = tile_speed_multiplier(kind);
    if m <= 0.0 {
        IMPASSABLE
    } else {
        ((BASE_STEP_COST as f32) / m).round() as u16
    }
}

pub fn furniture_speed_factor(
    pos: (i32, i32),
    bed_map: &BedMap,
    chair_map: &ChairMap,
    table_map: &TableMap,
    workbench_map: &WorkbenchMap,
    loom_map: &LoomMap,
) -> f32 {
    if bed_map.0.contains_key(&pos)
        || chair_map.0.contains_key(&pos)
        || table_map.0.contains_key(&pos)
        || workbench_map.0.contains_key(&pos)
        || loom_map.0.contains_key(&pos)
    {
        FURNITURE_SPEED_FACTOR
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn road_is_fastest() {
        assert!(tile_speed_multiplier(TileKind::Road) > tile_speed_multiplier(TileKind::Grass));
    }

    #[test]
    fn impassables_have_zero_speed() {
        for k in [TileKind::Water, TileKind::Wall, TileKind::Air] {
            assert_eq!(tile_speed_multiplier(k), 0.0);
            assert_eq!(tile_step_cost(k), IMPASSABLE);
        }
    }

    #[test]
    fn forest_costs_more_than_grass() {
        assert!(tile_step_cost(TileKind::Forest) > tile_step_cost(TileKind::Grass));
    }

    #[test]
    fn road_costs_less_than_grass() {
        assert!(tile_step_cost(TileKind::Road) < tile_step_cost(TileKind::Grass));
    }

    #[test]
    fn bridge_is_road_speed() {
        assert_eq!(
            tile_speed_multiplier(TileKind::Bridge),
            tile_speed_multiplier(TileKind::Road)
        );
        assert_eq!(
            tile_step_cost(TileKind::Bridge),
            tile_step_cost(TileKind::Road)
        );
    }

    #[test]
    fn cropland_is_soil_speed() {
        assert_eq!(
            tile_speed_multiplier(TileKind::Cropland),
            tile_speed_multiplier(TileKind::Loam)
        );
        assert!(tile_speed_multiplier(TileKind::Cropland) > 0.0);
        assert!(tile_step_cost(TileKind::Cropland) < IMPASSABLE);
    }

    #[test]
    fn river_remains_impassable() {
        assert_eq!(tile_speed_multiplier(TileKind::River), 0.0);
        assert_eq!(tile_step_cost(TileKind::River), IMPASSABLE);
    }
}
