use bevy::prelude::*;
use ahash::AHashMap;

/// Maps (tile_x, tile_y) -> list of entities occupying that tile.
/// Updated each frame by movement system.
#[derive(Resource, Default)]
pub struct SpatialIndex {
    /// All live entities by 2-D tile position.
    pub map: AHashMap<(i32, i32), Vec<Entity>>,
    /// Number of alive mobile agents (Person, Wolf, Deer) at each 3-D tile position (tx, ty, tz).
    /// Used for collision prevention and occupancy checks.
    pub agent_counts: AHashMap<(i32, i32, i32), u8>,
}

impl SpatialIndex {
    pub fn insert(&mut self, tile_x: i32, tile_y: i32, entity: Entity) {
        self.map.entry((tile_x, tile_y)).or_default().push(entity);
    }

    pub fn remove(&mut self, tile_x: i32, tile_y: i32, entity: Entity) {
        if let Some(v) = self.map.get_mut(&(tile_x, tile_y)) {
            v.retain(|&e| e != entity);
        }
    }

    pub fn get(&self, tile_x: i32, tile_y: i32) -> &[Entity] {
        self.map.get(&(tile_x, tile_y)).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Returns true if any alive mobile agent occupies (tx, ty, tz).
    pub fn agent_occupied(&self, tx: i32, ty: i32, tz: i32) -> bool {
        self.agent_counts.get(&(tx, ty, tz)).map_or(false, |&c| c > 0)
    }

    /// Returns the number of agents at (tx, ty, tz).
    pub fn agent_count(&self, tx: i32, ty: i32, tz: i32) -> u8 {
        self.agent_counts.get(&(tx, ty, tz)).copied().unwrap_or(0)
    }
}
