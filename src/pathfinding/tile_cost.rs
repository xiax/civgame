use crate::simulation::construction::{
    BedMap, ChairMap, LoomMap, TableMap, WorkbenchMap,
};
use crate::world::tile::TileKind;

pub const FURNITURE_SPEED_FACTOR: f32 = 0.5;

pub fn tile_speed_multiplier(kind: TileKind) -> f32 {
    match kind {
        TileKind::Grass | TileKind::Stone | TileKind::Ramp => 1.0,
        TileKind::Road => 1.4,
        TileKind::Forest => 0.7,
        TileKind::Farmland => 0.85,
        TileKind::Dirt => 0.9,
        TileKind::Water | TileKind::Air | TileKind::Wall => 0.0,
    }
}

const BASE_STEP_COST: u16 = 100;
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
    pos: (i16, i16),
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
}
